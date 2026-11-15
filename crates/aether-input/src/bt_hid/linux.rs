//! Linux backend: Bluetooth Classic HID *device* emulation with BlueZ.
//!
//! * SDP: `org.bluez.ProfileManager1.RegisterProfile` with UUID 0x1124 and
//!   our `ServiceRecord` XML ([`super::sdp::to_bluez_xml`]), over the system
//!   D-Bus with pure-Rust `zbus` (no libdbus).
//! * Transport: raw `AF_BLUETOOTH`/`BTPROTO_L2CAP` `SOCK_SEQPACKET` sockets on
//!   PSM 0x11 (control) and 0x13 (interrupt), listening and/or connecting to
//!   a bonded host, carrying HIDP ([`super::hidp`]).
//! * Pairing: an `org.bluez.Agent1` registered with `AgentManager1`
//!   (NoInputNoOutput by default) that only accepts pairing while
//!   [`HidPeripheral::set_pairing`](super::HidPeripheral::set_pairing) has a
//!   window open.
//! * Class of Device: optional, via the kernel mgmt socket
//!   (`MGMT_OP_SET_DEV_CLASS`, needs `CAP_NET_ADMIN`); the persistent
//!   alternative is `Class = 0x0005C0` in `/etc/bluetooth/main.conf`.
//!
//! Requirements (reported precisely by [`preflight`] and by `start`):
//! * PSMs below 0x1001 need **`CAP_NET_BIND_SERVICE`** (root, or
//!   `setcap cap_net_bind_service+ep <binary>`); otherwise `bind` → `EACCES`.
//! * bluetoothd's **`input` plugin** binds PSM 0x11/0x13 itself (it is the HID
//!   host); run bluetoothd with `-P input` (systemd drop-in:
//!   `ExecStart=` + `ExecStart=/usr/libexec/bluetooth/bluetoothd -P input`),
//!   otherwise `bind` → `EADDRINUSE`.

use super::hidp::{HidpDevice, HostEvent, PSM_HID_CONTROL, PSM_HID_INTERRUPT};
use super::sdp::{self, HidSdpConfig};
use super::{
    format_bdaddr, parse_bdaddr, BackendInfo, HidPeripheralBackend, HidPeripheralConfig, LinkState, PairedDevice,
    PeripheralStatus, Transport,
};
use crate::hid_report::HidReport;
use crate::{InputError, Result};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

pub const HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";
const PROFILE_PATH: &str = "/org/aetherlink/hid_profile";
const AGENT_PATH: &str = "/org/aetherlink/hid_agent";

// ------------------------------------------------------------- preflight

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPluginState {
    /// bluetoothd runs with `-P input` / `--noplugin=…input…`.
    Disabled,
    /// bluetoothd runs with the input plugin (it owns PSM 0x11/0x13).
    Enabled,
    /// bluetoothd not found / command line unreadable.
    Unknown,
}

/// Environment checks, read-only (procfs / sysfs only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    /// `hci0`, `hci1`, … from `/sys/class/bluetooth`.
    pub adapters: Vec<String>,
    pub cap_net_bind_service: bool,
    pub cap_net_admin: bool,
    pub bluetoothd_running: bool,
    pub input_plugin: InputPluginState,
}

impl Preflight {
    /// Problems that make `start` fail, as user-facing sentences.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.adapters.is_empty() {
            p.push("no Bluetooth adapter (/sys/class/bluetooth is empty)".to_string());
        }
        if !self.bluetoothd_running {
            p.push("bluetoothd is not running".to_string());
        }
        if !self.cap_net_bind_service {
            p.push("missing CAP_NET_BIND_SERVICE: binding HID PSM 0x11/0x13 needs root or `setcap cap_net_bind_service+ep <binary>`".to_string());
        }
        if self.input_plugin == InputPluginState::Enabled {
            p.push("bluetoothd's input plugin owns PSM 0x11/0x13: restart bluetoothd with `-P input`".to_string());
        }
        p
    }
}

/// `CapEff` from `/proc/self/status` content.
pub fn parse_cap_eff(status: &str) -> Option<u64> {
    status.lines().find_map(|l| l.strip_prefix("CapEff:")).and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
}

pub const CAP_NET_BIND_SERVICE: u32 = 10;
pub const CAP_NET_ADMIN: u32 = 12;

/// Whether a bluetoothd command line disables the `input` plugin.
pub fn bluetoothd_disables_input(args: &[String]) -> bool {
    let lists_input = |v: &str| v.split(',').any(|p| p.trim() == "input");
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let val = if a == "-P" || a == "--noplugin" {
            it.next().map(String::as_str)
        } else {
            a.strip_prefix("--noplugin=").or_else(|| a.strip_prefix("-P"))
        };
        if val.is_some_and(lists_input) {
            return true;
        }
    }
    false
}

fn bluetoothd_cmdline() -> Option<Vec<String>> {
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        if !e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<String> =
            raw.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        if args.first().is_some_and(|a| a.rsplit('/').next() == Some("bluetoothd")) {
            return Some(args);
        }
    }
    None
}

pub fn preflight() -> Preflight {
    let mut adapters: Vec<String> = std::fs::read_dir("/sys/class/bluetooth")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("hci") && !n.contains(':')).collect())
        .unwrap_or_default();
    adapters.sort();
    let caps = std::fs::read_to_string("/proc/self/status").ok().as_deref().and_then(parse_cap_eff).unwrap_or(0);
    let bd = bluetoothd_cmdline();
    Preflight {
        adapters,
        cap_net_bind_service: caps & (1 << CAP_NET_BIND_SERVICE) != 0,
        cap_net_admin: caps & (1 << CAP_NET_ADMIN) != 0,
        bluetoothd_running: bd.is_some(),
        input_plugin: match &bd {
            Some(a) if bluetoothd_disables_input(a) => InputPluginState::Disabled,
            Some(_) => InputPluginState::Enabled,
            None => InputPluginState::Unknown,
        },
    }
}

pub(crate) fn backend_info() -> BackendInfo {
    let p = preflight();
    let problems = p.problems();
    BackendInfo {
        backend: HidPeripheralBackend::LinuxBlueZ,
        available: problems.is_empty(),
        detail: if problems.is_empty() {
            format!("BlueZ Classic HID on {}", p.adapters.join(", "))
        } else {
            problems.join("; ")
        },
    }
}

// --------------------------------------------------------------- sockets

const BTPROTO_L2CAP: libc::c_int = 0;
const BTPROTO_HCI: libc::c_int = 1;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
/// Encrypted link (authenticated where the IO capabilities allow).
const BT_SECURITY_MEDIUM: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    /// Little-endian (reversed display order).
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

#[repr(C)]
struct BtSecurity {
    level: u8,
    key_size: u8,
}

fn sockaddr_l2(addr: [u8; 6], psm: u16) -> SockaddrL2 {
    let mut le = addr;
    le.reverse();
    SockaddrL2 { l2_family: libc::AF_BLUETOOTH as _, l2_psm: psm.to_le(), l2_bdaddr: le, l2_cid: 0, l2_bdaddr_type: 0 }
}

/// Map L2CAP bind errors to the precise cause.
fn bind_error(psm: u16, e: std::io::Error) -> InputError {
    match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => InputError::PermissionDenied(
            "binding HID L2CAP PSM 0x11/0x13 needs CAP_NET_BIND_SERVICE (run as root or `setcap cap_net_bind_service+ep <binary>`)",
        ),
        Some(libc::EADDRINUSE) => InputError::Os(format!(
            "L2CAP PSM {psm:#06x} is already bound — bluetoothd's input plugin owns the HID PSMs; restart bluetoothd with `-P input`"
        )),
        Some(libc::EAFNOSUPPORT) | Some(libc::EPROTONOSUPPORT) => {
            InputError::Unsupported("the kernel has no Bluetooth L2CAP socket support")
        }
        _ => InputError::Os(format!("L2CAP PSM {psm:#06x}: {e}")),
    }
}

#[derive(Debug)]
struct L2capSocket(OwnedFd);

impl L2capSocket {
    fn new() -> std::io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, BTPROTO_L2CAP) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let s = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        let sec = BtSecurity { level: BT_SECURITY_MEDIUM, key_size: 0 };
        let r = unsafe {
            libc::setsockopt(
                fd,
                SOL_BLUETOOTH,
                BT_SECURITY,
                (&sec as *const BtSecurity).cast(),
                std::mem::size_of::<BtSecurity>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(s)
    }

    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    fn listen(psm: u16) -> Result<Self> {
        let s = Self::new().map_err(|e| bind_error(psm, e))?;
        let sa = sockaddr_l2([0; 6], psm);
        let r = unsafe {
            libc::bind(s.fd(), (&sa as *const SockaddrL2).cast(), std::mem::size_of::<SockaddrL2>() as libc::socklen_t)
        };
        if r < 0 {
            return Err(bind_error(psm, std::io::Error::last_os_error()));
        }
        if unsafe { libc::listen(s.fd(), 1) } < 0 {
            return Err(bind_error(psm, std::io::Error::last_os_error()));
        }
        Ok(s)
    }

    fn accept(&self) -> std::io::Result<(Self, [u8; 6])> {
        let mut sa = sockaddr_l2([0; 6], 0);
        let mut len = std::mem::size_of::<SockaddrL2>() as libc::socklen_t;
        let fd = unsafe { libc::accept4(self.fd(), (&mut sa as *mut SockaddrL2).cast(), &mut len, libc::SOCK_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut addr = sa.l2_bdaddr;
        addr.reverse();
        Ok((Self(unsafe { OwnedFd::from_raw_fd(fd) }), addr))
    }

    fn connect(addr: [u8; 6], psm: u16) -> Result<Self> {
        let s = Self::new().map_err(|e| bind_error(psm, e))?;
        let sa = sockaddr_l2(addr, psm);
        let r = unsafe {
            libc::connect(s.fd(), (&sa as *const SockaddrL2).cast(), std::mem::size_of::<SockaddrL2>() as libc::socklen_t)
        };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            return Err(InputError::Os(format!(
                "connecting HID PSM {psm:#06x} to {}: {e} (host must be paired and in range)",
                format_bdaddr(&addr)
            )));
        }
        Ok(s)
    }

    fn send(&self, b: &[u8]) -> std::io::Result<()> {
        let n = unsafe { libc::send(self.fd(), b.as_ptr().cast(), b.len(), libc::MSG_NOSIGNAL) };
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    /// `Ok(None)` = peer closed.
    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 1024];
        let n = unsafe { libc::recv(self.fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        match n {
            n if n < 0 => Err(std::io::Error::last_os_error()),
            0 => Ok(None),
            n => Ok(Some(buf[..n as usize].to_vec())),
        }
    }
}

// ----------------------------------------------------------- mgmt (CoD)

pub const MGMT_OP_SET_DEV_CLASS: u16 = 0x000E;
const MGMT_EV_CMD_COMPLETE: u16 = 0x0001;
const MGMT_EV_CMD_STATUS: u16 = 0x0002;
const HCI_DEV_NONE: u16 = 0xFFFF;
const HCI_CHANNEL_CONTROL: u16 = 3;

/// `MGMT_OP_SET_DEV_CLASS` packet.
pub fn mgmt_set_dev_class_cmd(index: u16, major: u8, minor: u8) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..2].copy_from_slice(&MGMT_OP_SET_DEV_CLASS.to_le_bytes());
    p[2..4].copy_from_slice(&index.to_le_bytes());
    p[4..6].copy_from_slice(&2u16.to_le_bytes());
    p[6] = major;
    p[7] = minor;
    p
}

/// Parse a mgmt event: `(opcode, status)` for Command Complete / Status.
pub fn parse_mgmt_reply(b: &[u8]) -> Option<(u16, u8)> {
    if b.len() < 9 {
        return None;
    }
    let ev = u16::from_le_bytes([b[0], b[1]]);
    if ev != MGMT_EV_CMD_COMPLETE && ev != MGMT_EV_CMD_STATUS {
        return None;
    }
    Some((u16::from_le_bytes([b[6], b[7]]), b[8]))
}

#[repr(C)]
struct SockaddrHci {
    hci_family: libc::sa_family_t,
    hci_dev: u16,
    hci_channel: u16,
}

/// Set the adapter's Class of Device to Peripheral / keyboard + pointing.
pub fn set_class_of_device(adapter_index: u16) -> Result<()> {
    let fd = unsafe { libc::socket(libc::AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, BTPROTO_HCI) };
    if fd < 0 {
        return Err(InputError::Os(format!("mgmt socket: {}", std::io::Error::last_os_error())));
    }
    let s = unsafe { OwnedFd::from_raw_fd(fd) };
    let sa = SockaddrHci { hci_family: libc::AF_BLUETOOTH as _, hci_dev: HCI_DEV_NONE, hci_channel: HCI_CHANNEL_CONTROL };
    if unsafe { libc::bind(s.as_raw_fd(), (&sa as *const SockaddrHci).cast(), std::mem::size_of::<SockaddrHci>() as _) } < 0 {
        let e = std::io::Error::last_os_error();
        return Err(if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
            InputError::PermissionDenied("setting the Class of Device needs CAP_NET_ADMIN (or set Class = 0x0005C0 in /etc/bluetooth/main.conf)")
        } else {
            InputError::Os(format!("mgmt bind: {e}"))
        });
    }
    let cmd = mgmt_set_dev_class_cmd(adapter_index, sdp::MAJOR_CLASS_PERIPHERAL, sdp::MINOR_CLASS_KEYBOARD_POINTING);
    if unsafe { libc::write(s.as_raw_fd(), cmd.as_ptr().cast(), cmd.len()) } < 0 {
        return Err(InputError::Os(format!("mgmt write: {}", std::io::Error::last_os_error())));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 512];
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 200) } <= 0 {
            continue;
        }
        let n = unsafe { libc::read(s.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            continue;
        }
        if let Some((op, status)) = parse_mgmt_reply(&buf[..n as usize])
            && op == MGMT_OP_SET_DEV_CLASS
        {
            return if status == 0 {
                Ok(())
            } else {
                Err(InputError::Os(format!("MGMT_OP_SET_DEV_CLASS failed with mgmt status {status:#04x}")))
            };
        }
    }
    Err(InputError::Os("MGMT_OP_SET_DEV_CLASS: no reply from the kernel".into()))
}

// ----------------------------------------------------------------- D-Bus

struct HidProfile;

#[zbus::interface(name = "org.bluez.Profile1")]
impl HidProfile {
    fn release(&self) {}

    /// Not used for HID (we own the L2CAP sockets); the fd is closed on drop.
    fn new_connection(
        &self,
        _device: OwnedObjectPath,
        _fd: zbus::zvariant::OwnedFd,
        _props: HashMap<String, OwnedValue>,
    ) {
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// Pairing agent capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCapability {
    /// "Just Works" pairing.
    NoInputNoOutput,
    /// The host shows / asks for a passkey; displayed codes appear in
    /// [`PeripheralStatus::pairing_code`].
    KeyboardDisplay,
}

impl AgentCapability {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoInputNoOutput => "NoInputNoOutput",
            Self::KeyboardDisplay => "KeyboardDisplay",
        }
    }
}

struct Agent {
    /// Pairing allowed until this instant.
    window: Arc<Mutex<Option<Instant>>>,
    status: Arc<Mutex<PeripheralStatus>>,
}

impl Agent {
    fn allow(&self, what: &str) -> std::result::Result<(), AgentError> {
        let open = self.window.lock().map(|w| w.is_some_and(|t| Instant::now() < t)).unwrap_or(false);
        if open { Ok(()) } else { Err(AgentError::Rejected(format!("{what}: pairing window closed"))) }
    }

    fn show(&self, code: String) {
        if let Ok(mut s) = self.status.lock() {
            s.pairing_code = Some(code);
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, _device: OwnedObjectPath) -> std::result::Result<String, AgentError> {
        Err(AgentError::Rejected("legacy PIN pairing is not supported".into()))
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, pincode: String) -> std::result::Result<(), AgentError> {
        self.allow("DisplayPinCode")?;
        self.show(pincode);
        Ok(())
    }

    fn request_passkey(&self, _device: OwnedObjectPath) -> std::result::Result<u32, AgentError> {
        Err(AgentError::Rejected("passkey entry on this side is not supported".into()))
    }

    fn display_passkey(&self, _device: OwnedObjectPath, passkey: u32, _entered: u16) {
        self.show(format!("{passkey:06}"));
    }

    fn request_confirmation(&self, _device: OwnedObjectPath, passkey: u32) -> std::result::Result<(), AgentError> {
        self.allow("RequestConfirmation")?;
        self.show(format!("{passkey:06}"));
        Ok(())
    }

    fn request_authorization(&self, _device: OwnedObjectPath) -> std::result::Result<(), AgentError> {
        self.allow("RequestAuthorization")
    }

    fn authorize_service(&self, _device: OwnedObjectPath, uuid: String) -> std::result::Result<(), AgentError> {
        if uuid.eq_ignore_ascii_case(HID_UUID) {
            Ok(())
        } else {
            Err(AgentError::Rejected(format!("service {uuid} not offered by AetherLink")))
        }
    }

    fn cancel(&self) {}
}

fn dbus_err(what: &str, e: impl std::fmt::Display) -> InputError {
    InputError::Os(format!("BlueZ {what}: {e}"))
}

fn system_bus() -> Result<zbus::blocking::Connection> {
    zbus::blocking::Connection::system().map_err(|e| dbus_err("system D-Bus unavailable", e))
}

type Managed = HashMap<OwnedObjectPath, HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>>;

fn managed_objects(conn: &zbus::blocking::Connection) -> Result<Managed> {
    let om = zbus::blocking::fdo::ObjectManagerProxy::builder(conn)
        .destination("org.bluez")
        .and_then(|b| b.path("/"))
        .and_then(|b| b.build())
        .map_err(|e| dbus_err("ObjectManager", e))?;
    om.get_managed_objects().map_err(|e| dbus_err("GetManagedObjects (is bluetoothd running?)", e))
}

fn prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| v.try_clone().ok()).and_then(|v| T::try_from(v).ok())
}

fn iface<'a>(
    ifaces: &'a HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>,
    name: &str,
) -> Option<&'a HashMap<String, OwnedValue>> {
    ifaces.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| v)
}

fn first_adapter(m: &Managed) -> Option<OwnedObjectPath> {
    let mut v: Vec<&OwnedObjectPath> = m.iter().filter(|(_, i)| iface(i, "org.bluez.Adapter1").is_some()).map(|(p, _)| p).collect();
    v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    v.first().map(|p| (*p).clone())
}

/// Bonded devices from BlueZ (read-only).
pub fn paired_devices() -> Result<Vec<PairedDevice>> {
    let conn = system_bus()?;
    let mut v: Vec<PairedDevice> = managed_objects(&conn)?
        .values()
        .filter_map(|i| iface(i, "org.bluez.Device1"))
        .filter(|d| prop::<bool>(d, "Paired").unwrap_or(false))
        .map(|d| PairedDevice {
            address: prop::<String>(d, "Address").unwrap_or_default().to_ascii_lowercase(),
            name: prop::<String>(d, "Alias").or_else(|| prop::<String>(d, "Name")),
            connected: prop::<bool>(d, "Connected").unwrap_or(false),
        })
        .collect();
    v.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(v)
}

fn set_adapter_prop(conn: &zbus::blocking::Connection, adapter: &OwnedObjectPath, name: &str, value: Value<'_>) -> Result<()> {
    conn.call_method(
        Some("org.bluez"),
        adapter.as_str(),
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &("org.bluez.Adapter1", name, value),
    )
    .map(|_| ())
    .map_err(|e| dbus_err(&format!("set Adapter1.{name}"), e))
}

// ---------------------------------------------------------------- device

struct Link {
    hidp: HidpDevice,
    control: Option<L2capSocket>,
    interrupt: Option<L2capSocket>,
    peer: Option<[u8; 6]>,
}

struct Shared {
    link: Mutex<Link>,
    status: Arc<Mutex<PeripheralStatus>>,
    stop: AtomicBool,
}

impl Shared {
    fn set_status(&self, f: impl FnOnce(&mut PeripheralStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn refresh(&self, link: &Link) {
        let connected = link.control.is_some() && link.interrupt.is_some();
        let (protocol, leds, peer) = (link.hidp.protocol, link.hidp.leds, link.peer);
        self.set_status(|s| {
            let was = matches!(s.state, LinkState::Connected(_));
            if connected {
                if !was {
                    s.connection_epoch += 1;
                }
                s.state = LinkState::Connected(peer.map(|p| format_bdaddr(&p)).unwrap_or_else(|| "host".into()));
            } else if !matches!(s.state, LinkState::Connecting(_) | LinkState::Stopped) {
                s.state = LinkState::Listening;
            }
            s.protocol = protocol;
            s.leds = leds;
        });
    }
}

pub struct BluezHidDevice {
    shared: Arc<Shared>,
    conn: zbus::blocking::Connection,
    adapter: OwnedObjectPath,
    pairing_window: Arc<Mutex<Option<Instant>>>,
    made_discoverable: bool,
    worker: Option<JoinHandle<()>>,
    stopped: bool,
}

fn worker_loop(shared: Arc<Shared>, listeners: Option<(L2capSocket, L2capSocket)>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let mut fds = Vec::with_capacity(4);
        {
            let link = shared.link.lock().expect("link lock");
            if let Some((c, i)) = &listeners {
                fds.push(c.fd());
                fds.push(i.fd());
            }
            fds.extend(link.control.as_ref().map(L2capSocket::fd));
            fds.extend(link.interrupt.as_ref().map(L2capSocket::fd));
        }
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 50) };
        if n <= 0 {
            continue;
        }
        let ready: Vec<(RawFd, i16)> = pfds.iter().filter(|p| p.revents != 0).map(|p| (p.fd, p.revents)).collect();
        let mut link = shared.link.lock().expect("link lock");
        for (fd, revents) in ready {
            if let Some((lc, li)) = &listeners
                && (fd == lc.fd() || fd == li.fd())
            {
                let is_control = fd == lc.fd();
                match (if is_control { lc } else { li }).accept() {
                    Ok((sock, addr)) => {
                        if is_control {
                            link.hidp.on_connect();
                            link.peer = Some(addr);
                            link.control = Some(sock);
                        } else if link.peer.is_none_or(|p| p == addr) {
                            link.interrupt = Some(sock);
                        } // else: interrupt from a different host than control — drop it.
                    }
                    Err(e) => shared.set_status(|s| s.last_error = Some(format!("L2CAP accept: {e}"))),
                }
                continue;
            }
            let is_control = link.control.as_ref().is_some_and(|c| c.fd() == fd);
            let sock = if is_control { link.control.as_ref() } else { link.interrupt.as_ref() };
            let Some(sock) = sock else { continue };
            let msg = if revents & (libc::POLLHUP | libc::POLLERR) != 0 && revents & libc::POLLIN == 0 {
                Ok(None)
            } else {
                sock.recv()
            };
            match msg {
                Ok(Some(bytes)) if is_control => {
                    let reply = link.hidp.handle_control(&bytes);
                    if let (Some(resp), Some(c)) = (reply.response, link.control.as_ref())
                        && let Err(e) = c.send(&resp)
                    {
                        shared.set_status(|s| s.last_error = Some(format!("HID control reply: {e}")));
                    }
                    if reply.event == Some(HostEvent::VirtualCableUnplug) {
                        link.control = None;
                        link.interrupt = None;
                        link.peer = None;
                    }
                }
                Ok(Some(bytes)) => {
                    link.hidp.handle_interrupt(&bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Ok(None) | Err(_) => {
                    // Either channel gone: the HID link is over.
                    link.control = None;
                    link.interrupt = None;
                    link.peer = None;
                }
            }
        }
        shared.refresh(&link);
    }
}

impl BluezHidDevice {
    pub fn start(cfg: &HidPeripheralConfig) -> Result<Self> {
        Self::start_with_agent(cfg, AgentCapability::NoInputNoOutput)
    }

    pub fn start_with_agent(cfg: &HidPeripheralConfig, capability: AgentCapability) -> Result<Self> {
        let pre = preflight();
        if pre.adapters.is_empty() {
            return Err(InputError::Unsupported("no Bluetooth adapter (/sys/class/bluetooth is empty)"));
        }
        // Bind first: the precise CAP_NET_BIND_SERVICE / input-plugin errors come from here.
        let listeners = if cfg.listen {
            let c = L2capSocket::listen(PSM_HID_CONTROL)?;
            let i = L2capSocket::listen(PSM_HID_INTERRUPT)?;
            Some((c, i))
        } else {
            None
        };
        let conn = system_bus()?;
        let adapter = first_adapter(&managed_objects(&conn)?)
            .ok_or_else(|| InputError::Os("BlueZ reports no org.bluez.Adapter1".into()))?;
        let status = Arc::new(Mutex::new(PeripheralStatus::default()));
        let window = Arc::new(Mutex::new(None));

        {
            let srv = conn.object_server();
            srv.at(PROFILE_PATH, HidProfile).map_err(|e| dbus_err("export Profile1", e))?;
            srv.at(AGENT_PATH, Agent { window: window.clone(), status: status.clone() })
                .map_err(|e| dbus_err("export Agent1", e))?;
        }

        let xml = sdp::to_bluez_xml(&sdp::hid_service_record(&HidSdpConfig {
            service_name: cfg.device_name.clone(),
            ..HidSdpConfig::default()
        }));
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("ServiceRecord", Value::from(xml.as_str()));
        opts.insert("Role", Value::from("server"));
        opts.insert("Name", Value::from(cfg.device_name.as_str()));
        opts.insert("RequireAuthentication", Value::from(true));
        opts.insert("RequireAuthorization", Value::from(false));
        let profile = ObjectPath::try_from(PROFILE_PATH).expect("valid path");
        conn.call_method(
            Some("org.bluez"),
            "/org/bluez",
            Some("org.bluez.ProfileManager1"),
            "RegisterProfile",
            &(&profile, HID_UUID, opts),
        )
        .map_err(|e| dbus_err("RegisterProfile (HID UUID already registered by the input plugin?)", e))?;

        let agent = ObjectPath::try_from(AGENT_PATH).expect("valid path");
        let mut dev = Self {
            shared: Arc::new(Shared {
                link: Mutex::new(Link { hidp: HidpDevice::new(), control: None, interrupt: None, peer: None }),
                status,
                stop: AtomicBool::new(false),
            }),
            conn,
            adapter,
            pairing_window: window,
            made_discoverable: false,
            worker: None,
            stopped: false,
        };
        // From here on, Drop cleans up (UnregisterProfile etc.).
        dev.conn
            .call_method(
                Some("org.bluez"),
                "/org/bluez",
                Some("org.bluez.AgentManager1"),
                "RegisterAgent",
                &(&agent, capability.as_str()),
            )
            .map_err(|e| dbus_err("RegisterAgent", e))?;
        if let Err(e) = dev.conn.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.AgentManager1"), "RequestDefaultAgent", &(&agent,)) {
            dev.shared.set_status(|s| s.last_error = Some(format!("RequestDefaultAgent: {e} (another agent stays default)")));
        }
        if cfg.set_class_of_device {
            let idx = dev.adapter.as_str().rsplit("hci").next().and_then(|n| n.parse().ok()).unwrap_or(0);
            set_class_of_device(idx)?;
        }
        let shared = dev.shared.clone();
        dev.worker = Some(
            std::thread::Builder::new()
                .name("aether-hid-bluez".into())
                .spawn(move || worker_loop(shared, listeners))
                .map_err(InputError::Io)?,
        );
        dev.shared.set_status(|s| s.state = LinkState::Listening);
        Ok(dev)
    }
}

impl Transport for BluezHidDevice {
    fn send_report(&mut self, r: &HidReport) -> Result<()> {
        let mut link = self.shared.link.lock().expect("link lock");
        if link.interrupt.is_none() {
            self.shared.set_status(|s| s.reports_dropped += 1);
            return Err(InputError::Os("no HID host connected".into()));
        }
        let pkts = link.hidp.encode_input(r);
        for p in pkts {
            let res = link.interrupt.as_ref().map(|s| s.send(&p)).unwrap_or(Ok(()));
            match res {
                Ok(()) => self.shared.set_status(|s| s.reports_sent += 1),
                Err(e) => {
                    self.shared.set_status(|s| {
                        s.reports_dropped += 1;
                        s.last_error = Some(format!("HID interrupt send: {e}"));
                    });
                    return Err(InputError::Io(e));
                }
            }
        }
        Ok(())
    }

    fn status(&self) -> PeripheralStatus {
        self.shared.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
        paired_devices()
    }

    fn connect(&mut self, address: &str) -> Result<()> {
        let addr = parse_bdaddr(address)?;
        {
            let link = self.shared.link.lock().expect("link lock");
            if link.control.is_some() && link.interrupt.is_some() {
                return Err(InputError::InvalidArgument("already connected; disconnect first".into()));
            }
        }
        self.shared.set_status(|s| s.state = LinkState::Connecting(address.to_string()));
        let res = L2capSocket::connect(addr, PSM_HID_CONTROL)
            .and_then(|c| L2capSocket::connect(addr, PSM_HID_INTERRUPT).map(|i| (c, i)));
        self.shared.set_status(|s| s.state = LinkState::Listening);
        let mut link = self.shared.link.lock().expect("link lock");
        match res {
            Ok((c, i)) => {
                link.hidp.on_connect();
                link.control = Some(c);
                link.interrupt = Some(i);
                link.peer = Some(addr);
                self.shared.refresh(&link);
                Ok(())
            }
            Err(e) => {
                self.shared.refresh(&link);
                Err(e)
            }
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        let mut link = self.shared.link.lock().expect("link lock");
        // Interrupt first, then control (HID spec §8.4).
        link.interrupt = None;
        link.control = None;
        link.peer = None;
        self.shared.refresh(&link);
        Ok(())
    }

    fn set_pairing(&mut self, enabled: bool, timeout: Duration) -> Result<()> {
        let secs = timeout.as_secs().clamp(1, 600) as u32;
        if let Ok(mut w) = self.pairing_window.lock() {
            *w = enabled.then(|| Instant::now() + Duration::from_secs(secs as u64));
        }
        if enabled {
            set_adapter_prop(&self.conn, &self.adapter, "PairableTimeout", Value::from(secs))?;
            set_adapter_prop(&self.conn, &self.adapter, "Pairable", Value::from(true))?;
            set_adapter_prop(&self.conn, &self.adapter, "DiscoverableTimeout", Value::from(secs))?;
            set_adapter_prop(&self.conn, &self.adapter, "Discoverable", Value::from(true))?;
            self.made_discoverable = true;
        } else {
            set_adapter_prop(&self.conn, &self.adapter, "Discoverable", Value::from(false))?;
            self.made_discoverable = false;
            self.shared.set_status(|s| s.pairing_code = None);
        }
        Ok(())
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        let _ = self.disconnect();
        if self.made_discoverable {
            let _ = set_adapter_prop(&self.conn, &self.adapter, "Discoverable", Value::from(false));
        }
        let agent = ObjectPath::try_from(AGENT_PATH).expect("valid path");
        let profile = ObjectPath::try_from(PROFILE_PATH).expect("valid path");
        let _ = self.conn.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.AgentManager1"), "UnregisterAgent", &(&agent,));
        let _ = self.conn.call_method(Some("org.bluez"), "/org/bluez", Some("org.bluez.ProfileManager1"), "UnregisterProfile", &(&profile,));
        let srv = self.conn.object_server();
        let _ = srv.remove::<HidProfile, _>(PROFILE_PATH);
        let _ = srv.remove::<Agent, _>(AGENT_PATH);
        self.shared.set_status(|s| s.state = LinkState::Stopped);
    }
}

impl Drop for BluezHidDevice {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_eff_parsing() {
        let st = "Name:\tcargo\nCapInh:\t0000000000000000\nCapEff:\t0000000000000400\nCapBnd:\t000001ffffffffff\n";
        let c = parse_cap_eff(st).unwrap();
        assert!(c & (1 << CAP_NET_BIND_SERVICE) != 0);
        assert!(c & (1 << CAP_NET_ADMIN) == 0);
        assert_eq!(parse_cap_eff("CapEff:\tzz"), None);
    }

    #[test]
    fn input_plugin_detection() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(bluetoothd_disables_input(&a("/usr/libexec/bluetooth/bluetoothd -P input")));
        assert!(bluetoothd_disables_input(&a("/usr/sbin/bluetoothd -Pinput")));
        assert!(bluetoothd_disables_input(&a("bluetoothd --noplugin=sap,input -n")));
        assert!(bluetoothd_disables_input(&a("bluetoothd --noplugin input")));
        assert!(!bluetoothd_disables_input(&a("bluetoothd -P sap")));
        assert!(!bluetoothd_disables_input(&a("bluetoothd --noplugin=inputx")));
        assert!(!bluetoothd_disables_input(&a("bluetoothd")));
    }

    #[test]
    fn mgmt_packets() {
        assert_eq!(mgmt_set_dev_class_cmd(0, 0x05, 0xC0), [0x0E, 0x00, 0x00, 0x00, 0x02, 0x00, 0x05, 0xC0]);
        // Command Complete for SET_DEV_CLASS, status 0, followed by 3-byte class.
        let ev = [0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x0E, 0x00, 0x00, 0xC0, 0x05, 0x00];
        assert_eq!(parse_mgmt_reply(&ev), Some((MGMT_OP_SET_DEV_CLASS, 0)));
        let st = [0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x0E, 0x00, 0x14];
        assert_eq!(parse_mgmt_reply(&st), Some((MGMT_OP_SET_DEV_CLASS, 0x14)));
        assert_eq!(parse_mgmt_reply(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn sockaddr_layout() {
        assert_eq!(std::mem::size_of::<SockaddrL2>(), 14);
        let sa = sockaddr_l2([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], 0x11);
        assert_eq!(sa.l2_bdaddr, [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(u16::from_le(sa.l2_psm), 0x11);
        assert_eq!(std::mem::size_of::<SockaddrHci>(), 6);
    }

    #[test]
    fn preflight_reports_problems() {
        // In CI containers there is usually no adapter; must not panic either way.
        let p = preflight();
        if p.adapters.is_empty() {
            assert!(!p.problems().is_empty());
            assert!(!backend_info().available);
        }
    }

    /// Without CAP_NET_BIND_SERVICE the bind error must name the capability.
    #[test]
    fn bind_errors_are_precise() {
        assert!(matches!(bind_error(0x11, std::io::Error::from_raw_os_error(libc::EACCES)), InputError::PermissionDenied(m) if m.contains("CAP_NET_BIND_SERVICE")));
        assert!(bind_error(0x11, std::io::Error::from_raw_os_error(libc::EADDRINUSE)).to_string().contains("-P input"));
        assert!(matches!(bind_error(0x11, std::io::Error::from_raw_os_error(libc::EAFNOSUPPORT)), InputError::Unsupported(_)));
    }

    #[test]
    #[ignore = "binds real HID PSMs and registers a BlueZ profile/agent"]
    fn start_and_stop_bluez() {
        let d = BluezHidDevice::start(&HidPeripheralConfig::default());
        println!("{:?}", d.as_ref().map(|d| d.status()));
    }
}
