//! macOS backend: Bluetooth Classic HID *device* emulation with IOBluetooth.
//!
//! 1. A transient HID SDP record ([`super::sdp`]) is published with
//!    `+[IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:]`
//!    (no `Persistent` local attribute: it is removed on stop and, at the
//!    latest, when the process exits).
//! 2. Host-initiated connections: `+[IOBluetoothL2CAPChannel
//!    registerForChannelOpenNotifications:selector:withPSM:direction:]` for
//!    PSM 0x11 (control) and 0x13 (interrupt), incoming direction.
//! 3. Device-initiated (re)connection — the way an iPhone is normally
//!    driven, as Typeeto does: pair the iPhone with the Mac once, then
//!    [`MacHidDevice`] opens L2CAP 0x11 then 0x13 to it with
//!    `-[IOBluetoothDevice openL2CAPChannelSync:withPSM:delegate:]`; iOS
//!    reads our SDP record and treats the Mac as a keyboard/mouse.
//! 4. Reports go out as HIDP `DATA|INPUT` (0xA1) on the interrupt channel;
//!    control requests are answered by [`super::hidp::HidpDevice`].
//!
//! All IOBluetooth objects live on one worker thread, which also spins its
//! CFRunLoop. IOBluetooth may deliver delegate callbacks on the main run
//! loop instead (it does for some APIs); the delegate only enqueues events
//! on a channel, so either works — in a CLI without a main run loop,
//! host-initiated connections may then never be reported.
//!
//! **Unverified on macOS 27**: whether `bluetoothd` still lets a user process
//! publish a record with the HID PSMs and open/accept HID L2CAP channels
//! (macOS itself is a HID *host* and owns those PSMs for incoming
//! keyboards). [`probe`] publishes the record, registers both PSMs, reads the
//! record back and removes everything — it reports what the OS accepted.

use super::hidp::{HidpDevice, HostEvent, PSM_HID_CONTROL, PSM_HID_INTERRUPT};
use super::sdp::{self, HidSdpConfig, PlistValue};
use super::{BackendInfo, HidPeripheralBackend, HidPeripheralConfig, LinkState, PairedDevice, PeripheralStatus, Transport};
use crate::hid_report::HidReport;
use crate::{InputError, Result};
use core_foundation_sys::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, Message};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSString};
use objc2_io_bluetooth::{
    BluetoothHCIPowerState, IOBluetoothDevice, IOBluetoothHostController, IOBluetoothL2CAPChannel,
    IOBluetoothSDPServiceRecord, IOBluetoothUserNotification, IOBluetoothUserNotificationChannelDirection,
};
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const K_IO_RETURN_SUCCESS: i32 = 0;

/// Name of a well-known `IOReturn` (or Bluetooth HCI status) for error messages.
pub fn io_return_name(r: i32) -> String {
    let name = match r as u32 {
        0xE00002BC => "kIOReturnError",
        0xE00002BD => "kIOReturnNoMemory",
        0xE00002BE => "kIOReturnNoResources",
        0xE00002C0 => "kIOReturnNoDevice",
        0xE00002C1 => "kIOReturnNotPrivileged",
        0xE00002C2 => "kIOReturnBadArgument",
        0xE00002C5 => "kIOReturnExclusiveAccess",
        0xE00002C7 => "kIOReturnUnsupported",
        0xE00002CD => "kIOReturnNotOpen",
        0xE00002D5 => "kIOReturnBusy",
        0xE00002D6 => "kIOReturnTimeout",
        0xE00002D8 => "kIOReturnNotReady",
        0xE00002E2 => "kIOReturnNotPermitted",
        0xE00002EB => "kIOReturnAborted",
        0x04 => "HCI page timeout (device out of range or not listening)",
        0x05 => "HCI authentication failure",
        0x06 => "HCI PIN or key missing (not paired)",
        0x0D => "HCI connection rejected: limited resources",
        0x0E => "HCI connection rejected: security reasons",
        0x13 => "HCI remote user terminated connection",
        _ => return format!("{r:#010x}"),
    };
    format!("{name} ({:#010x})", r as u32)
}

fn ns_object(v: &PlistValue) -> Retained<NSObject> {
    match v {
        PlistValue::Int(i) => NSNumber::numberWithLongLong(*i).into_super().into_super(),
        PlistValue::String(s) => NSString::from_str(s).into_super(),
        PlistValue::Data(d) => NSData::with_bytes(d).into_super(),
        PlistValue::Array(items) => {
            let objs: Vec<Retained<NSObject>> = items.iter().map(ns_object).collect();
            NSArray::from_retained_slice(&objs).into_super()
        }
        PlistValue::Dict(kv) => ns_dictionary(kv).into_super(),
    }
}

fn ns_dictionary(kv: &[(String, PlistValue)]) -> Retained<NSDictionary<NSString, NSObject>> {
    let keys: Vec<Retained<NSString>> = kv.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let vals: Vec<Retained<NSObject>> = kv.iter().map(|(_, v)| ns_object(v)).collect();
    let kr: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let vr: Vec<&NSObject> = vals.iter().map(|v| &**v).collect();
    NSDictionary::from_slices(&kr, &vr)
}

/// The HID service dictionary as handed to IOBluetooth.
pub fn service_dictionary(cfg: &HidSdpConfig) -> Retained<NSDictionary> {
    let PlistValue::Dict(kv) = sdp::to_apple_dict(&sdp::hid_service_record(cfg)) else {
        unreachable!("to_apple_dict returns a dictionary")
    };
    // SAFETY: NSDictionary's generics are compile-time only; the object is an
    // NSDictionary whose keys are NSString and values NSObject subclasses.
    unsafe { Retained::cast_unchecked::<NSDictionary>(ns_dictionary(&kv)) }
}

/// A published, transient SDP record; removed on drop.
struct PublishedRecord(Option<Retained<IOBluetoothSDPServiceRecord>>);

impl PublishedRecord {
    fn publish(cfg: &HidSdpConfig) -> Result<Self> {
        let dict = service_dictionary(cfg);
        let rec = unsafe { IOBluetoothSDPServiceRecord::publishedServiceRecordWithDictionary(Some(&dict)) };
        rec.map(|r| Self(Some(r))).ok_or_else(|| {
            InputError::Os(
                "IOBluetooth refused to publish the HID SDP record (publishedServiceRecordWithDictionary returned nil; \
                 Bluetooth off, permission denied, or HID records reserved by bluetoothd)"
                    .into(),
            )
        })
    }

    fn rec(&self) -> &IOBluetoothSDPServiceRecord {
        self.0.as_ref().expect("record present until removed")
    }

    fn handle(&self) -> Option<u32> {
        let mut h = 0u32;
        (unsafe { self.rec().getServiceRecordHandle(&mut h) } == K_IO_RETURN_SUCCESS).then_some(h)
    }

    fn psm(&self) -> Option<u16> {
        let mut p = 0u16;
        (unsafe { self.rec().getL2CAPPSM(&mut p) } == K_IO_RETURN_SUCCESS).then_some(p)
    }

    fn attribute_ids(&self) -> Vec<u16> {
        let attrs = unsafe { self.rec().attributes() };
        let keys = attrs.allKeys();
        let mut ids: Vec<u16> = keys.iter().map(|k| unsafe { msg_send![&*k, unsignedShortValue] }).collect();
        ids.sort_unstable();
        ids
    }

    fn remove(mut self) -> std::result::Result<(), String> {
        let rec = self.0.take().expect("record present until removed");
        let r = unsafe { rec.removeServiceRecord() };
        if r == K_IO_RETURN_SUCCESS { Ok(()) } else { Err(io_return_name(r)) }
    }
}

impl Drop for PublishedRecord {
    fn drop(&mut self) {
        if let Some(rec) = self.0.take() {
            unsafe { rec.removeServiceRecord() };
        }
    }
}

/// `IOBluetoothUserNotification` registration; unregistered on drop.
struct Registration(Retained<IOBluetoothUserNotification>);

impl Drop for Registration {
    fn drop(&mut self) {
        unsafe { self.0.unregister() };
    }
}

// ---------------------------------------------------------------- delegate

/// IOBluetooth objects are only *used* on the worker thread; they are moved
/// across threads only inside events.
struct SendChannel(Retained<IOBluetoothL2CAPChannel>);
// SAFETY: IOBluetooth objects are reference counted with atomic retain/release;
// the worker thread is the only one that messages them.
unsafe impl Send for SendChannel {}

enum MacEvent {
    Opened(SendChannel),
    Data { channel: usize, bytes: Vec<u8> },
    Closed { channel: usize },
}

struct DelegateIvars {
    tx: Mutex<Sender<MacEvent>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "AetherLinkHidL2CAPDelegate"]
    #[ivars = DelegateIvars]
    struct HidDelegate;

    impl HidDelegate {
        /// Target of `registerForChannelOpenNotifications:selector:withPSM:direction:`.
        #[unsafe(method(newL2CAPChannelOpened:channel:))]
        fn new_channel(&self, _n: *mut IOBluetoothUserNotification, ch: *mut IOBluetoothL2CAPChannel) {
            if let Some(ch) = unsafe { ch.as_ref() } {
                self.emit(MacEvent::Opened(SendChannel(ch.retain())));
            }
        }

        #[unsafe(method(l2capChannelData:data:length:))]
        fn channel_data(&self, ch: *mut IOBluetoothL2CAPChannel, data: *mut c_void, len: usize) {
            if data.is_null() || ch.is_null() {
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) }.to_vec();
            self.emit(MacEvent::Data { channel: ch as usize, bytes });
        }

        #[unsafe(method(l2capChannelClosed:))]
        fn channel_closed(&self, ch: *mut IOBluetoothL2CAPChannel) {
            self.emit(MacEvent::Closed { channel: ch as usize });
        }

        #[unsafe(method(l2capChannelOpenComplete:status:))]
        fn channel_open_complete(&self, _ch: *mut IOBluetoothL2CAPChannel, _status: i32) {}

        #[unsafe(method(l2capChannelWriteComplete:refcon:status:))]
        fn channel_write_complete(&self, _ch: *mut IOBluetoothL2CAPChannel, _refcon: *mut c_void, _status: i32) {}
    }

    unsafe impl NSObjectProtocol for HidDelegate {}
);

impl HidDelegate {
    fn new(tx: Sender<MacEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { tx: Mutex::new(tx) });
        unsafe { msg_send![super(this), init] }
    }

    fn emit(&self, ev: MacEvent) {
        if let Ok(tx) = self.ivars().tx.lock() {
            let _ = tx.send(ev);
        }
    }

    fn as_any(&self) -> &AnyObject {
        self.as_ref()
    }
}

fn register_incoming(delegate: &HidDelegate, psm: u16) -> Result<Registration> {
    // Sent by hand: the runtime encodes `direction` as `int` while the
    // generated binding uses an unsigned newtype, which objc2's debug
    // encoding check rejects (verified with method_getTypeEncoding:
    // `@40@0:8@16:24S32i36`).
    let n: Option<Retained<IOBluetoothUserNotification>> = unsafe {
        msg_send![
            IOBluetoothL2CAPChannel::class(),
            registerForChannelOpenNotifications: delegate.as_any(),
            selector: sel!(newL2CAPChannelOpened:channel:),
            withPSM: psm,
            direction: IOBluetoothUserNotificationChannelDirection::Incoming.0 as i32
        ]
    };
    n.map(Registration)
        .ok_or_else(|| InputError::Os(format!("IOBluetooth refused the incoming L2CAP registration for PSM {psm:#06x}")))
}

// ------------------------------------------------------------------- probe

/// Local controller facts (read-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerInfo {
    pub address: Option<String>,
    pub name: Option<String>,
    pub powered: bool,
    pub class_of_device: u32,
}

pub fn controller_info() -> Option<ControllerInfo> {
    let c = unsafe { IOBluetoothHostController::defaultController() }?;
    unsafe {
        Some(ControllerInfo {
            address: c.addressAsString().map(|s| s.to_string()),
            name: c.nameAsString().map(|s| s.to_string()),
            // The header declares `BluetoothHCIPowerState` (unsigned) but the
            // runtime method returns `int`; objc2's encoding check rejects the
            // generated binding, so send the message with the runtime type.
            powered: {
                let p: i32 = msg_send![&*c, powerState];
                p as u32 == BluetoothHCIPowerState::ON.0
            },
            class_of_device: c.classOfDevice(),
        })
    }
}

/// What [`probe`] observed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeReport {
    pub controller: Option<ControllerInfo>,
    pub record_published: bool,
    pub record_handle: Option<u32>,
    /// PSM IOBluetooth reads back from the published record (expect 0x0011).
    pub published_psm: Option<u16>,
    /// Attribute IDs present in the published record.
    pub published_attribute_ids: Vec<u16>,
    pub control_registration: bool,
    pub interrupt_registration: bool,
    /// Record removed again (`removeServiceRecord` returned success).
    pub cleaned_up: bool,
    pub errors: Vec<String>,
}

impl ProbeReport {
    /// Everything this process can check locally was accepted. (Whether a
    /// remote host's L2CAP connection actually reaches us still needs a peer.)
    pub fn allowed(&self) -> bool {
        self.record_published
            && self.published_psm == Some(PSM_HID_CONTROL)
            && self.published_attribute_ids.contains(&sdp::attr::HID_DESCRIPTOR_LIST)
            && self.control_registration
            && self.interrupt_registration
    }
}

/// Non-intrusive capability check: publish the HID record, register for
/// incoming PSM 0x11/0x13, read the record back, then unregister and remove
/// everything. Does not make the Mac discoverable, pair or connect.
pub fn probe() -> ProbeReport {
    let mut r = ProbeReport { controller: controller_info(), ..Default::default() };
    match &r.controller {
        None => r.errors.push("no Bluetooth host controller (IOBluetoothHostController.defaultController is nil)".into()),
        Some(c) if !c.powered => r.errors.push("Bluetooth is powered off".into()),
        _ => {}
    }
    let rec = match PublishedRecord::publish(&HidSdpConfig::default()) {
        Ok(rec) => rec,
        Err(e) => {
            r.errors.push(e.to_string());
            return r;
        }
    };
    r.record_published = true;
    r.record_handle = rec.handle();
    r.published_psm = rec.psm();
    r.published_attribute_ids = rec.attribute_ids();
    let (tx, _rx) = mpsc::channel();
    let delegate = HidDelegate::new(tx);
    match register_incoming(&delegate, PSM_HID_CONTROL) {
        Ok(reg) => {
            r.control_registration = true;
            drop(reg);
        }
        Err(e) => r.errors.push(e.to_string()),
    }
    match register_incoming(&delegate, PSM_HID_INTERRUPT) {
        Ok(reg) => {
            r.interrupt_registration = true;
            drop(reg);
        }
        Err(e) => r.errors.push(e.to_string()),
    }
    match rec.remove() {
        Ok(()) => r.cleaned_up = true,
        Err(e) => r.errors.push(format!("removeServiceRecord failed: {e}")),
    }
    r
}

pub(crate) fn backend_info() -> BackendInfo {
    // Deliberately does not touch IOBluetooth (that may raise the Bluetooth
    // privacy prompt); `probe()` is the explicit check.
    BackendInfo {
        backend: HidPeripheralBackend::MacIoBluetooth,
        available: true,
        detail: "IOBluetooth Classic HID device emulation: pair the iPhone with this Mac, then connect(address). \
                 Whether bluetoothd accepts HID PSMs from apps on this macOS is checked by bt_hid::macos::probe()"
            .into(),
    }
}

fn normalize_address(s: &str) -> String {
    s.replace('-', ":").to_ascii_lowercase()
}

/// Paired devices (read-only; may trigger the Bluetooth privacy prompt).
pub fn paired_devices() -> Result<Vec<PairedDevice>> {
    let arr = unsafe { IOBluetoothDevice::pairedDevices() }.unwrap_or_else(NSArray::new);
    let mut v = Vec::new();
    for obj in arr.iter() {
        // SAFETY: +pairedDevices returns an array of IOBluetoothDevice.
        let dev: Retained<IOBluetoothDevice> = unsafe { Retained::cast_unchecked(obj) };
        unsafe {
            v.push(PairedDevice {
                address: dev.addressString().map(|s| normalize_address(&s.to_string())).unwrap_or_default(),
                name: dev.nameOrAddress().map(|s| s.to_string()),
                connected: dev.isConnected(),
            });
        }
    }
    Ok(v)
}

// ------------------------------------------------------------------ device

enum Cmd {
    Report(HidReport),
    Connect(String, Sender<Result<()>>),
    Disconnect(Sender<Result<()>>),
    Stop,
}

pub struct MacHidDevice {
    cmd: Sender<Cmd>,
    status: Arc<Mutex<PeripheralStatus>>,
    worker: Option<JoinHandle<()>>,
}

struct Worker {
    delegate: Retained<HidDelegate>,
    events: Receiver<MacEvent>,
    status: Arc<Mutex<PeripheralStatus>>,
    hidp: HidpDevice,
    control: Option<Retained<IOBluetoothL2CAPChannel>>,
    interrupt: Option<Retained<IOBluetoothL2CAPChannel>>,
    device: Option<Retained<IOBluetoothDevice>>,
    cod: Option<(Retained<IOBluetoothHostController>, Instant)>,
    // Dropped (unregistered / removed) after the channels are closed.
    registrations: Vec<Registration>,
    record: Option<PublishedRecord>,
}

fn write_channel(ch: &IOBluetoothL2CAPChannel, bytes: &[u8]) -> std::result::Result<(), String> {
    let mut buf = bytes.to_vec();
    let r = unsafe { ch.writeSync_length(buf.as_mut_ptr().cast(), buf.len() as u16) };
    if r == K_IO_RETURN_SUCCESS { Ok(()) } else { Err(io_return_name(r)) }
}

fn ptr_of(ch: &Option<Retained<IOBluetoothL2CAPChannel>>) -> usize {
    ch.as_ref().map(|c| Retained::as_ptr(c) as usize).unwrap_or(0)
}

impl Worker {
    fn set_status(&self, f: impl FnOnce(&mut PeripheralStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn error(&self, e: String) {
        self.set_status(|s| s.last_error = Some(e));
    }

    fn update_link(&mut self) {
        let connected = self.control.is_some() && self.interrupt.is_some();
        let addr = self
            .control
            .as_ref()
            .and_then(|c| unsafe { c.device() })
            .and_then(|d| unsafe { d.addressString() })
            .map(|s| normalize_address(&s.to_string()));
        let (protocol, leds) = (self.hidp.protocol, self.hidp.leds);
        self.set_status(|s| {
            let was = matches!(s.state, LinkState::Connected(_));
            if connected {
                if !was {
                    s.connection_epoch += 1;
                }
                s.state = LinkState::Connected(addr.unwrap_or_else(|| "host".into()));
            } else if !matches!(s.state, LinkState::Connecting(_)) {
                s.state = LinkState::Listening;
            }
            s.protocol = protocol;
            s.leds = leds;
        });
    }

    fn on_event(&mut self, ev: MacEvent) {
        match ev {
            MacEvent::Opened(SendChannel(ch)) => {
                let psm = unsafe { ch.PSM() };
                let r = unsafe { ch.setDelegate(Some(self.delegate.as_any())) };
                if r != K_IO_RETURN_SUCCESS {
                    self.error(format!("setDelegate on incoming PSM {psm:#06x} failed: {}", io_return_name(r)));
                }
                match psm {
                    PSM_HID_CONTROL => {
                        self.hidp.on_connect();
                        self.control = Some(ch);
                    }
                    PSM_HID_INTERRUPT => self.interrupt = Some(ch),
                    _ => return,
                }
                self.update_link();
            }
            MacEvent::Data { channel, bytes } => {
                if channel == ptr_of(&self.control) {
                    let reply = self.hidp.handle_control(&bytes);
                    if let (Some(resp), Some(ch)) = (reply.response, self.control.as_ref())
                        && let Err(e) = write_channel(ch, &resp)
                    {
                        self.error(format!("HID control reply failed: {e}"));
                    }
                    self.on_host_event(reply.event);
                } else if channel == ptr_of(&self.interrupt) {
                    let ev = self.hidp.handle_interrupt(&bytes);
                    self.on_host_event(ev);
                }
            }
            MacEvent::Closed { channel } => {
                if channel == ptr_of(&self.control) {
                    self.control = None;
                }
                if channel == ptr_of(&self.interrupt) {
                    self.interrupt = None;
                }
                self.update_link();
            }
        }
    }

    fn on_host_event(&mut self, ev: Option<HostEvent>) {
        match ev {
            Some(HostEvent::VirtualCableUnplug) => self.close_link(),
            Some(_) => self.update_link(),
            None => {}
        }
    }

    fn send(&mut self, r: &HidReport) {
        let Some(ch) = self.interrupt.clone() else {
            self.set_status(|s| s.reports_dropped += 1);
            return;
        };
        for pkt in self.hidp.encode_input(r) {
            match write_channel(&ch, &pkt) {
                Ok(()) => self.set_status(|s| s.reports_sent += 1),
                Err(e) => self.set_status(|s| {
                    s.reports_dropped += 1;
                    s.last_error = Some(format!("HID interrupt write failed: {e}"));
                }),
            }
        }
    }

    fn open(&self, dev: &IOBluetoothDevice, psm: u16) -> Result<Retained<IOBluetoothL2CAPChannel>> {
        let mut ch: Option<Retained<IOBluetoothL2CAPChannel>> = None;
        let r = unsafe { dev.openL2CAPChannelSync_withPSM_delegate(Some(&mut ch), psm, Some(self.delegate.as_any())) };
        match (r, ch) {
            (K_IO_RETURN_SUCCESS, Some(ch)) => Ok(ch),
            (r, _) => Err(InputError::Os(format!(
                "opening HID L2CAP PSM {psm:#06x} failed: {} (the iPhone must be paired with this Mac and in range; \
                 kIOReturnNotPermitted means bluetoothd reserves the HID PSMs)",
                io_return_name(r)
            ))),
        }
    }

    fn connect(&mut self, address: &str) -> Result<()> {
        if self.control.is_some() && self.interrupt.is_some() {
            return Err(InputError::InvalidArgument("already connected; disconnect first".into()));
        }
        let ns = NSString::from_str(&address.replace(':', "-"));
        let dev = unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&ns)) }
            .ok_or_else(|| InputError::InvalidArgument(format!("unknown Bluetooth address {address}")))?;
        if !unsafe { dev.isPaired() } {
            return Err(InputError::InvalidArgument(format!(
                "{address} is not paired with this Mac; pair it first (System Settings → Bluetooth)"
            )));
        }
        self.set_status(|s| s.state = LinkState::Connecting(address.to_string()));
        let res = (|| {
            let control = self.open(&dev, PSM_HID_CONTROL)?;
            self.hidp.on_connect();
            self.control = Some(control);
            let interrupt = self.open(&dev, PSM_HID_INTERRUPT)?;
            self.interrupt = Some(interrupt);
            Ok(())
        })();
        if res.is_err() {
            self.close_link();
        } else {
            self.device = Some(dev);
        }
        self.set_status(|s| {
            if matches!(s.state, LinkState::Connecting(_)) {
                s.state = LinkState::Listening;
            }
        });
        self.update_link();
        res
    }

    fn close_link(&mut self) {
        for ch in [self.interrupt.take(), self.control.take()].into_iter().flatten() {
            unsafe { ch.closeChannel() };
        }
        if let Some(d) = self.device.take() {
            unsafe { d.closeConnection() };
        }
        self.update_link();
    }

    fn refresh_cod(&mut self) {
        if let Some((c, last)) = &mut self.cod
            && last.elapsed() > Duration::from_secs(100)
        {
            unsafe { c.setClassOfDevice_forTimeInterval(sdp::CLASS_OF_DEVICE, 120.0) };
            *last = Instant::now();
        }
    }

    fn run(mut self, cmds: Receiver<Cmd>) {
        loop {
            // Service IOBluetooth sources attached to this thread's run loop.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) };
            while let Ok(ev) = self.events.try_recv() {
                self.on_event(ev);
            }
            self.refresh_cod();
            let cmd = match cmds.recv_timeout(Duration::from_millis(2)) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => Cmd::Stop,
            };
            match cmd {
                Cmd::Report(r) => self.send(&r),
                Cmd::Connect(addr, reply) => {
                    let _ = reply.send(self.connect(&addr));
                }
                Cmd::Disconnect(reply) => {
                    self.close_link();
                    let _ = reply.send(Ok(()));
                }
                Cmd::Stop => break,
            }
        }
        self.close_link();
        self.registrations.clear();
        if let Some(rec) = self.record.take()
            && let Err(e) = rec.remove()
        {
            self.error(format!("removeServiceRecord failed: {e}"));
        }
        self.set_status(|s| s.state = LinkState::Stopped);
    }
}

impl MacHidDevice {
    pub fn start(cfg: &HidPeripheralConfig) -> Result<Self> {
        let status = Arc::new(Mutex::new(PeripheralStatus::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();
        let sdp_cfg = HidSdpConfig { service_name: cfg.device_name.clone(), ..HidSdpConfig::default() };
        let (listen, set_cod, st) = (cfg.listen, cfg.set_class_of_device, status.clone());
        let worker = std::thread::Builder::new()
            .name("aether-hid-iobluetooth".into())
            .spawn(move || {
                let init = || -> Result<Worker> {
                    let ctrl = controller_info()
                        .ok_or(InputError::Unsupported("no Bluetooth controller (IOBluetoothHostController is nil)"))?;
                    if !ctrl.powered {
                        return Err(InputError::Os("Bluetooth is powered off".into()));
                    }
                    let record = PublishedRecord::publish(&sdp_cfg)?;
                    if record.psm() != Some(PSM_HID_CONTROL) {
                        return Err(InputError::Os(format!(
                            "published HID record reads back PSM {:?} instead of 0x0011",
                            record.psm()
                        )));
                    }
                    let (ev_tx, ev_rx) = mpsc::channel();
                    let delegate = HidDelegate::new(ev_tx);
                    let mut registrations = Vec::new();
                    if listen {
                        registrations.push(register_incoming(&delegate, PSM_HID_CONTROL)?);
                        registrations.push(register_incoming(&delegate, PSM_HID_INTERRUPT)?);
                    }
                    let cod = if set_cod {
                        let c = unsafe { IOBluetoothHostController::defaultController() }
                            .ok_or(InputError::Unsupported("no Bluetooth controller"))?;
                        let r = unsafe { c.setClassOfDevice_forTimeInterval(sdp::CLASS_OF_DEVICE, 120.0) };
                        if r != K_IO_RETURN_SUCCESS {
                            return Err(InputError::Os(format!("setClassOfDevice failed: {}", io_return_name(r))));
                        }
                        Some((c, Instant::now()))
                    } else {
                        None
                    };
                    Ok(Worker {
                        delegate,
                        events: ev_rx,
                        status: st,
                        hidp: HidpDevice::new(),
                        control: None,
                        interrupt: None,
                        device: None,
                        cod,
                        registrations,
                        record: Some(record),
                    })
                };
                match init() {
                    Ok(w) => {
                        w.set_status(|s| s.state = LinkState::Listening);
                        let _ = init_tx.send(Ok(()));
                        w.run(cmd_rx);
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                    }
                }
            })
            .map_err(InputError::Io)?;
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { cmd: cmd_tx, status, worker: Some(worker) }),
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(_) => Err(InputError::Os("IOBluetooth worker thread died during start".into())),
        }
    }

    fn call(&self, f: impl FnOnce(Sender<Result<()>>) -> Cmd) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.cmd.send(f(tx)).map_err(|_| InputError::Os("IOBluetooth worker stopped".into()))?;
        rx.recv().map_err(|_| InputError::Os("IOBluetooth worker stopped".into()))?
    }
}

impl Transport for MacHidDevice {
    fn send_report(&mut self, r: &HidReport) -> Result<()> {
        if !matches!(self.status().state, LinkState::Connected(_)) {
            return Err(InputError::Os("no HID host connected".into()));
        }
        self.cmd.send(Cmd::Report(r.clone())).map_err(|_| InputError::Os("IOBluetooth worker stopped".into()))
    }

    fn status(&self) -> PeripheralStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
        paired_devices()
    }

    fn connect(&mut self, address: &str) -> Result<()> {
        let a = address.to_string();
        self.call(|tx| Cmd::Connect(a, tx))
    }

    fn disconnect(&mut self) -> Result<()> {
        self.call(Cmd::Disconnect)
    }

    fn set_pairing(&mut self, _enabled: bool, _timeout: Duration) -> Result<()> {
        Err(InputError::Unsupported(
            "macOS: pair the iPhone from System Settings → Bluetooth, then connect(address); apps cannot make the Mac discoverable",
        ))
    }

    fn stop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for MacHidDevice {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_converts_to_foundation() {
        // Pure Foundation objects; does not touch Bluetooth.
        let d = service_dictionary(&HidSdpConfig::default());
        assert_eq!(d.count(), sdp::hid_service_record(&HidSdpConfig::default()).len());
        let key = NSString::from_str("0206 - HIDDescriptorList");
        let obj = unsafe { d.objectForKey(&*Retained::cast_unchecked::<AnyObject>(key)) };
        assert!(obj.is_some());
    }

    #[test]
    fn io_return_names() {
        assert!(io_return_name(0xE00002E2u32 as i32).starts_with("kIOReturnNotPermitted"));
        assert_eq!(io_return_name(0x1234), "0x00001234");
        assert_eq!(normalize_address("AA-bb-CC-dd-EE-ff"), "aa:bb:cc:dd:ee:ff");
    }

    /// Publishes and immediately removes a transient HID SDP record and
    /// registers/unregisters the PSM notifications. No discovery, pairing
    /// or connection. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "touches the real Bluetooth stack (may show the Bluetooth privacy prompt)"]
    fn probe_iobluetooth() {
        let t = Instant::now();
        let c = controller_info();
        println!("controller_info after {:?}: {c:?}", t.elapsed());
        let r = probe();
        println!("probe finished after {:?}", t.elapsed());
        println!("{r:#?}\nallowed = {}", r.allowed());
        assert!(r.cleaned_up || !r.record_published, "record must not be left behind");
    }

    #[test]
    #[ignore = "reads the real list of paired Bluetooth devices"]
    fn list_paired() {
        println!("{:#?}", paired_devices());
    }
}
