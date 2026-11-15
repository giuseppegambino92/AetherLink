//! Physical USB device backend on top of `rusb` (vendored libusb).

use std::collections::{HashMap, HashSet};
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use aether_protocol::{
    UrbComplete, UrbStatus, UrbSubmit, UsbDeviceInfo, UsbDirection, UsbSpeed, UsbTransferType,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use rusb::{Context, Device, DeviceHandle, UsbContext};
use tokio::sync::oneshot;

use crate::descriptors::{DT_CONFIG, DeviceDescriptor};
use crate::error::{Result, UsbError};
use crate::usbip_server::UsbBackend;

const STRING_TIMEOUT: Duration = Duration::from_millis(500);
/// Control transfers submitted without a timeout still get this bound.
const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bulk/interrupt transfers without a timeout are executed in slices of this length so
/// they stay cancellable (libusb reports partial progress on each slice timeout).
const SLICE_MS: u32 = 250;

const LIBUSB_ERROR_NO_DEVICE: c_int = -4;
const LIBUSB_ERROR_TIMEOUT: c_int = -7;
const LIBUSB_ERROR_OVERFLOW: c_int = -8;
const LIBUSB_ERROR_PIPE: c_int = -9;

fn device_id(dev: &Device<Context>) -> u32 {
    ((dev.bus_number() as u32) << 8) | dev.address() as u32
}

fn bcd(v: rusb::Version) -> u16 {
    let major = v.major() as u16;
    ((major / 10) << 12)
        | ((major % 10) << 8)
        | ((v.minor() as u16 & 0xF) << 4)
        | (v.sub_minor() as u16 & 0xF)
}

fn speed(s: rusb::Speed) -> UsbSpeed {
    match s {
        rusb::Speed::Low => UsbSpeed::Low,
        rusb::Speed::Full => UsbSpeed::Full,
        rusb::Speed::High => UsbSpeed::High,
        rusb::Speed::Super => UsbSpeed::Super,
        rusb::Speed::SuperPlus => UsbSpeed::SuperPlus,
        _ => UsbSpeed::Unknown,
    }
}

fn raw_device_descriptor(d: &rusb::DeviceDescriptor) -> Vec<u8> {
    DeviceDescriptor {
        bcd_usb: bcd(d.usb_version()),
        class: d.class_code(),
        subclass: d.sub_class_code(),
        protocol: d.protocol_code(),
        max_packet_size0: d.max_packet_size(),
        vendor_id: d.vendor_id(),
        product_id: d.product_id(),
        bcd_device: bcd(d.device_version()),
        manufacturer_index: d.manufacturer_string_index().unwrap_or(0),
        product_index: d.product_string_index().unwrap_or(0),
        serial_index: d.serial_number_string_index().unwrap_or(0),
        num_configurations: d.num_configurations(),
    }
    .to_bytes()
    .to_vec()
}

/// Rebuild a configuration descriptor blob from libusb's parsed form (used when the
/// device cannot be opened to read the raw bytes). Class-specific descriptors are kept
/// through libusb's `extra` buffers.
fn rebuild_config(c: &rusb::ConfigDescriptor) -> Vec<u8> {
    let attrs = 0x80 | ((c.self_powered() as u8) << 6) | ((c.remote_wakeup() as u8) << 5);
    let mut out = vec![
        9,
        DT_CONFIG,
        0,
        0,
        c.num_interfaces(),
        c.number(),
        c.description_string_index().unwrap_or(0),
        attrs,
        (c.max_power() / 2).min(255) as u8,
    ];
    out.extend_from_slice(c.extra());
    for iface in c.interfaces() {
        for alt in iface.descriptors() {
            out.extend_from_slice(&[
                9,
                4,
                alt.interface_number(),
                alt.setting_number(),
                alt.num_endpoints(),
                alt.class_code(),
                alt.sub_class_code(),
                alt.protocol_code(),
                alt.description_string_index().unwrap_or(0),
            ]);
            out.extend_from_slice(alt.extra());
            for ep in alt.endpoint_descriptors() {
                let tt = match ep.transfer_type() {
                    rusb::TransferType::Control => 0,
                    rusb::TransferType::Isochronous => 1,
                    rusb::TransferType::Bulk => 2,
                    rusb::TransferType::Interrupt => 3,
                };
                let sync = match ep.sync_type() {
                    rusb::SyncType::NoSync => 0,
                    rusb::SyncType::Asynchronous => 1,
                    rusb::SyncType::Adaptive => 2,
                    rusb::SyncType::Synchronous => 3,
                };
                let usage = match ep.usage_type() {
                    rusb::UsageType::Data => 0,
                    rusb::UsageType::Feedback => 1,
                    rusb::UsageType::FeedbackData => 2,
                    rusb::UsageType::Reserved => 3,
                };
                let mps = ep.max_packet_size().to_le_bytes();
                let audio = ep.length() >= 9;
                out.extend_from_slice(&[
                    if audio { 9 } else { 7 },
                    5,
                    ep.address(),
                    tt | (sync << 2) | (usage << 4),
                    mps[0],
                    mps[1],
                    ep.interval(),
                ]);
                if audio {
                    out.extend_from_slice(&[ep.refresh(), ep.synch_address()]);
                }
                if let Some(extra) = ep.extra() {
                    out.extend_from_slice(extra);
                }
            }
        }
    }
    let total = (out.len() as u16).to_le_bytes();
    out[2..4].copy_from_slice(&total);
    out
}

/// Read a configuration descriptor's raw bytes with GET_DESCRIPTOR.
fn read_raw_config(h: &DeviceHandle<Context>, index: u8) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 9];
    let value = ((DT_CONFIG as u16) << 8) | index as u16;
    let n = h
        .read_control(0x80, 6, value, 0, &mut hdr, STRING_TIMEOUT)
        .ok()?;
    if n < 9 {
        return None;
    }
    let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
    let mut buf = vec![0u8; total];
    let n = h
        .read_control(0x80, 6, value, 0, &mut buf, STRING_TIMEOUT)
        .ok()?;
    (n == total).then_some(buf)
}

fn info_of(dev: &Device<Context>) -> Result<UsbDeviceInfo> {
    let dd = dev.device_descriptor()?;
    let mut configs: Vec<Vec<u8>> = (0..dd.num_configurations())
        .filter_map(|i| dev.config_descriptor(i).ok().map(|c| rebuild_config(&c)))
        .collect();
    let (mut manufacturer, mut product, mut serial) = (None, None, None);
    // Opening is best-effort (needs permissions on Linux; may be refused for some devices).
    if let Ok(h) = dev.open() {
        if let Ok(langs) = h.read_languages(STRING_TIMEOUT)
            && let Some(&lang) = langs.first()
        {
            manufacturer = h.read_manufacturer_string(lang, &dd, STRING_TIMEOUT).ok();
            product = h.read_product_string(lang, &dd, STRING_TIMEOUT).ok();
            serial = h.read_serial_number_string(lang, &dd, STRING_TIMEOUT).ok();
        }
        for (i, slot) in configs.iter_mut().enumerate() {
            if let Some(raw) = read_raw_config(&h, i as u8) {
                *slot = raw;
            }
        }
    }
    Ok(UsbDeviceInfo {
        device_id: device_id(dev),
        bus: dev.bus_number(),
        address: dev.address(),
        vendor_id: dd.vendor_id(),
        product_id: dd.product_id(),
        class: dd.class_code(),
        subclass: dd.sub_class_code(),
        protocol: dd.protocol_code(),
        speed: speed(dev.speed()),
        manufacturer,
        product,
        serial,
        device_descriptor: raw_device_descriptor(&dd),
        config_descriptors: configs,
    })
}

struct Job {
    urb: UrbSubmit,
    reply: oneshot::Sender<UrbComplete>,
}

struct Attached {
    device_id: u32,
    handle: DeviceHandle<Context>,
    claimed: Mutex<Vec<u8>>,
    /// One worker thread per endpoint (control = key 0), so a pending interrupt IN does
    /// not block control or bulk traffic.
    workers: Mutex<HashMap<u8, std_mpsc::Sender<Job>>>,
    inflight: Mutex<HashSet<u64>>,
    cancelled: Mutex<HashSet<u64>>,
}

impl Attached {
    fn claim_active(&self) -> Result<()> {
        let cfg = self.handle.device().active_config_descriptor()?;
        let mut claimed = self.claimed.lock();
        for iface in cfg.interfaces() {
            if let Err(e) = self.handle.claim_interface(iface.number()) {
                for n in claimed.drain(..) {
                    let _ = self.handle.release_interface(n);
                }
                return Err(UsbError::Backend(format!(
                    "cannot claim interface {}: {e}",
                    iface.number()
                )));
            }
            claimed.push(iface.number());
        }
        Ok(())
    }

    fn release_all(&self) {
        for n in self.claimed.lock().drain(..) {
            let _ = self.handle.release_interface(n);
        }
    }

    fn take_cancel(&self, urb_id: u64) -> bool {
        self.cancelled.lock().remove(&urb_id)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.release_all();
        tracing::debug!("usb device {:#x} released", self.device_id);
    }
}

fn status_of(e: rusb::Error) -> UrbStatus {
    match e {
        rusb::Error::Timeout => UrbStatus::Timeout,
        rusb::Error::Pipe => UrbStatus::Stall,
        rusb::Error::NoDevice => UrbStatus::NoDevice,
        rusb::Error::Overflow => UrbStatus::Overflow,
        rusb::Error::Interrupted => UrbStatus::Cancelled,
        _ => UrbStatus::Error,
    }
}

fn status_of_rc(rc: c_int) -> UrbStatus {
    match rc {
        0 => UrbStatus::Ok,
        LIBUSB_ERROR_TIMEOUT => UrbStatus::Timeout,
        LIBUSB_ERROR_PIPE => UrbStatus::Stall,
        LIBUSB_ERROR_NO_DEVICE => UrbStatus::NoDevice,
        LIBUSB_ERROR_OVERFLOW => UrbStatus::Overflow,
        _ => UrbStatus::Error,
    }
}

/// Synchronous bulk/interrupt transfer via libusb directly, so the number of bytes moved
/// before a timeout is not lost (rusb drops it).
fn raw_transfer(
    h: &DeviceHandle<Context>,
    interrupt: bool,
    ep: u8,
    buf: &mut [u8],
    timeout_ms: u32,
) -> (c_int, usize) {
    let mut transferred: c_int = 0;
    let len = buf.len().min(c_int::MAX as usize) as c_int;
    // SAFETY: `h.as_raw()` is a valid open handle for the lifetime of `h`; `buf` is a live,
    // exclusively borrowed buffer of at least `len` bytes; `transferred` outlives the call.
    let rc = unsafe {
        if interrupt {
            rusb::ffi::libusb_interrupt_transfer(
                h.as_raw(),
                ep,
                buf.as_mut_ptr(),
                len,
                &mut transferred,
                timeout_ms,
            )
        } else {
            rusb::ffi::libusb_bulk_transfer(
                h.as_raw(),
                ep,
                buf.as_mut_ptr(),
                len,
                &mut transferred,
                timeout_ms,
            )
        }
    };
    (rc, transferred.max(0) as usize)
}

fn complete(urb: &UrbSubmit, status: UrbStatus, data: Vec<u8>, actual: usize) -> UrbComplete {
    UrbComplete {
        urb_id: urb.urb_id,
        status,
        data,
        actual_length: actual as u32,
    }
}

fn execute(dev: &Attached, urb: &UrbSubmit) -> UrbComplete {
    let h = &dev.handle;
    match urb.transfer_type {
        UsbTransferType::Isochronous => complete(urb, UrbStatus::Error, Vec::new(), 0),
        UsbTransferType::Control => {
            let Some(s) = urb.setup else {
                return complete(urb, UrbStatus::Error, Vec::new(), 0);
            };
            let (bm, req) = (s[0], s[1]);
            let value = u16::from_le_bytes([s[2], s[3]]);
            let index = u16::from_le_bytes([s[4], s[5]]);
            let wlen = u16::from_le_bytes([s[6], s[7]]) as usize;
            let timeout = if urb.timeout_ms == 0 {
                DEFAULT_CONTROL_TIMEOUT
            } else {
                Duration::from_millis(urb.timeout_ms as u64)
            };
            // Standard requests that change host-side state go through libusb's API so
            // the OS stack stays consistent (same approach as the Linux usbip stub).
            let special = match (bm, req) {
                (0x00, 0x09) => Some((|| {
                    dev.release_all();
                    h.set_active_configuration(value as u8)?;
                    dev.claim_active().map_err(|_| rusb::Error::Busy)
                })()),
                (0x01, 0x0B) => Some(h.set_alternate_setting(index as u8, value as u8)),
                (0x02, 0x01) if value == 0 => Some(h.clear_halt(index as u8)),
                _ => None,
            };
            if let Some(r) = special {
                return match r {
                    Ok(()) => complete(urb, UrbStatus::Ok, Vec::new(), 0),
                    Err(e) => complete(urb, status_of(e), Vec::new(), 0),
                };
            }
            if bm & 0x80 != 0 {
                let mut buf = vec![0u8; wlen.min(urb.length as usize)];
                match h.read_control(bm, req, value, index, &mut buf, timeout) {
                    Ok(n) => {
                        buf.truncate(n);
                        complete(urb, UrbStatus::Ok, buf, n)
                    }
                    Err(e) => complete(urb, status_of(e), Vec::new(), 0),
                }
            } else {
                match h.write_control(bm, req, value, index, &urb.data, timeout) {
                    Ok(n) => complete(urb, UrbStatus::Ok, Vec::new(), n),
                    Err(e) => complete(urb, status_of(e), Vec::new(), 0),
                }
            }
        }
        UsbTransferType::Bulk | UsbTransferType::Interrupt => {
            let interrupt = urb.transfer_type == UsbTransferType::Interrupt;
            let endless = urb.timeout_ms == 0;
            let slice = if endless { SLICE_MS } else { urb.timeout_ms };
            if urb.direction == UsbDirection::In {
                let ep = urb.endpoint | 0x80;
                let mut buf = vec![0u8; urb.length as usize];
                loop {
                    let (rc, n) = raw_transfer(h, interrupt, ep, &mut buf, slice);
                    if rc == LIBUSB_ERROR_TIMEOUT && n == 0 && endless {
                        if dev.take_cancel(urb.urb_id) {
                            return complete(urb, UrbStatus::Cancelled, Vec::new(), 0);
                        }
                        continue;
                    }
                    buf.truncate(n);
                    let status = if rc == LIBUSB_ERROR_TIMEOUT && n > 0 {
                        UrbStatus::Ok
                    } else {
                        status_of_rc(rc)
                    };
                    return complete(urb, status, buf, n);
                }
            } else {
                let ep = urb.endpoint & 0x7F;
                let mut data = urb.data.clone();
                let mut off = 0usize;
                loop {
                    let (rc, n) = raw_transfer(h, interrupt, ep, &mut data[off..], slice);
                    off += n;
                    if rc == 0 || (off == data.len() && !data.is_empty()) {
                        return complete(urb, UrbStatus::Ok, Vec::new(), off);
                    }
                    if rc == LIBUSB_ERROR_TIMEOUT && endless {
                        if dev.take_cancel(urb.urb_id) {
                            return complete(urb, UrbStatus::Cancelled, Vec::new(), off);
                        }
                        continue;
                    }
                    return complete(urb, status_of_rc(rc), Vec::new(), off);
                }
            }
        }
    }
}

fn worker(dev: Arc<Attached>, rx: std_mpsc::Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        let done = if dev.take_cancel(job.urb.urb_id) {
            complete(&job.urb, UrbStatus::Cancelled, Vec::new(), 0)
        } else {
            execute(&dev, &job.urb)
        };
        dev.inflight.lock().remove(&job.urb.urb_id);
        dev.cancelled.lock().remove(&job.urb.urb_id);
        let _ = job.reply.send(done);
    }
}

struct Inner {
    ctx: Context,
    attached: Mutex<HashMap<u32, Arc<Attached>>>,
}

/// Local USB devices through libusb. Cheap to clone.
#[derive(Clone)]
pub struct UsbHost(Arc<Inner>);

impl std::fmt::Debug for UsbHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbHost")
            .field(
                "attached",
                &self.0.attached.lock().keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl UsbHost {
    pub fn new() -> Result<Self> {
        Ok(Self(Arc::new(Inner {
            ctx: Context::new()?,
            attached: Mutex::new(HashMap::new()),
        })))
    }

    fn find(&self, id: u32) -> Result<Device<Context>> {
        self.0
            .ctx
            .devices()?
            .iter()
            .find(|d| device_id(d) == id)
            .ok_or(UsbError::NoDevice(id))
    }

    /// Enumerate devices (hubs excluded). Strings are best-effort. Blocking.
    pub fn list_devices_blocking(&self) -> Result<Vec<UsbDeviceInfo>> {
        let mut out = Vec::new();
        for dev in self.0.ctx.devices()?.iter() {
            match dev.device_descriptor() {
                Ok(d) if d.class_code() == 0x09 => continue, // hub
                Ok(_) => {}
                Err(_) => continue,
            }
            match info_of(&dev) {
                Ok(i) => out.push(i),
                Err(e) => tracing::debug!("usb: skipping {:#x}: {e}", device_id(&dev)),
            }
        }
        out.sort_by_key(|d| d.device_id);
        Ok(out)
    }

    /// Open the device, detach kernel drivers where supported, claim all interfaces of the
    /// active configuration. Blocking.
    pub fn attach_blocking(&self, device_id: u32) -> Result<()> {
        if self.0.attached.lock().contains_key(&device_id) {
            return Ok(());
        }
        let dev = self.find(device_id)?;
        let handle = dev.open()?;
        match handle.set_auto_detach_kernel_driver(true) {
            Ok(()) | Err(rusb::Error::NotSupported) => {}
            Err(e) => tracing::debug!("usb: auto-detach kernel driver: {e}"),
        }
        let att = Arc::new(Attached {
            device_id,
            handle,
            claimed: Mutex::new(Vec::new()),
            workers: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            cancelled: Mutex::new(HashSet::new()),
        });
        att.claim_active()?;
        self.0.attached.lock().insert(device_id, att);
        Ok(())
    }

    /// Release the device. Queued URBs complete as `Cancelled`; an in-flight URB without
    /// timeout is cancelled within one slice (250 ms).
    pub fn detach_device(&self, device_id: u32) -> Result<()> {
        let att = self
            .0
            .attached
            .lock()
            .remove(&device_id)
            .ok_or(UsbError::NotAttached(device_id))?;
        {
            let inflight = att.inflight.lock();
            att.cancelled.lock().extend(inflight.iter().copied());
        }
        att.workers.lock().clear(); // workers exit after their current job
        Ok(())
    }

    /// Queue a URB on its endpoint worker; resolves when it completes.
    pub fn submit_urb(&self, device_id: u32, urb: UrbSubmit) -> oneshot::Receiver<UrbComplete> {
        let (tx, rx) = oneshot::channel();
        let Some(att) = self.0.attached.lock().get(&device_id).cloned() else {
            let _ = tx.send(complete(&urb, UrbStatus::NoDevice, Vec::new(), 0));
            return rx;
        };
        let key = if urb.transfer_type == UsbTransferType::Control {
            0
        } else {
            urb.endpoint
        };
        att.inflight.lock().insert(urb.urb_id);
        let mut workers = att.workers.lock();
        let sender = workers.entry(key).or_insert_with(|| {
            let (wtx, wrx) = std_mpsc::channel();
            let a = att.clone();
            let _ = std::thread::Builder::new()
                .name(format!("usb-{device_id:x}-ep{key:02x}"))
                .spawn(move || worker(a, wrx));
            wtx
        });
        if let Err(std_mpsc::SendError(job)) = sender.send(Job { urb, reply: tx }) {
            att.inflight.lock().remove(&job.urb.urb_id);
            let _ = job
                .reply
                .send(complete(&job.urb, UrbStatus::NoDevice, Vec::new(), 0));
        }
        rx
    }

    /// Request cancellation of a queued or in-flight URB.
    pub fn unlink_urb(&self, device_id: u32, urb_id: u64) {
        if let Some(att) = self.0.attached.lock().get(&device_id)
            && att.inflight.lock().contains(&urb_id)
        {
            att.cancelled.lock().insert(urb_id);
        }
    }
}

#[async_trait]
impl UsbBackend for UsbHost {
    async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        let h = self.clone();
        tokio::task::spawn_blocking(move || h.list_devices_blocking())
            .await
            .map_err(|e| UsbError::Backend(e.to_string()))?
    }

    async fn attach(&self, device_id: u32) -> Result<()> {
        let h = self.clone();
        tokio::task::spawn_blocking(move || h.attach_blocking(device_id))
            .await
            .map_err(|e| UsbError::Backend(e.to_string()))?
    }

    async fn detach(&self, device_id: u32) -> Result<()> {
        self.detach_device(device_id)
    }

    async fn submit(&self, device_id: u32, urb: UrbSubmit) -> UrbComplete {
        let fallback = complete(&urb, UrbStatus::NoDevice, Vec::new(), 0);
        self.submit_urb(device_id, urb).await.unwrap_or(fallback)
    }

    async fn unlink(&self, device_id: u32, urb_id: u64) {
        self.unlink_urb(device_id, urb_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::ConfigDescriptor;

    #[test]
    fn bcd_encoding() {
        assert_eq!(bcd(rusb::Version(2, 1, 0)), 0x0210);
        assert_eq!(bcd(rusb::Version(3, 2, 0)), 0x0320);
        assert_eq!(bcd(rusb::Version(12, 3, 4)), 0x1234);
    }

    /// Runs against whatever is plugged in; passes with zero devices. On macOS enumeration
    /// needs no special permission.
    #[test]
    fn enumerate_local_devices() {
        let host = match UsbHost::new() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("libusb unavailable here: {e}");
                return;
            }
        };
        let devices = host.list_devices_blocking().expect("enumeration");
        eprintln!("found {} USB device(s)", devices.len());
        for d in &devices {
            eprintln!(
                "  {:#06x} {:04x}:{:04x} {:?} {:?} {:?}",
                d.device_id, d.vendor_id, d.product_id, d.speed, d.manufacturer, d.product
            );
            let dd =
                DeviceDescriptor::parse(&d.device_descriptor).expect("valid device descriptor");
            assert_eq!((dd.vendor_id, dd.product_id), (d.vendor_id, d.product_id));
            for c in &d.config_descriptors {
                ConfigDescriptor::parse(c).expect("valid config descriptor");
            }
        }
    }

    #[test]
    #[ignore = "needs a USB device that can be claimed (set AETHER_USB_TEST_DEVICE=<device_id hex>)"]
    fn attach_and_get_descriptor() {
        let id = u32::from_str_radix(
            &std::env::var("AETHER_USB_TEST_DEVICE").expect("AETHER_USB_TEST_DEVICE"),
            16,
        )
        .unwrap();
        let host = UsbHost::new().unwrap();
        host.attach_blocking(id).expect("attach");
        let urb = UrbSubmit {
            urb_id: 1,
            endpoint: 0x80,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::In,
            setup: Some([0x80, 6, 0, 1, 0, 0, 18, 0]),
            data: vec![],
            length: 18,
            timeout_ms: 1000,
        };
        let done = host.submit_urb(id, urb).blocking_recv().unwrap();
        assert_eq!(done.status, UrbStatus::Ok);
        DeviceDescriptor::parse(&done.data).unwrap();
        host.detach_device(id).unwrap();
    }
}
