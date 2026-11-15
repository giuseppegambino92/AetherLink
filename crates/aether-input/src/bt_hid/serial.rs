//! Host side of the ESP32-S3 HID dongle: port detection, handshake, report
//! forwarding with sequence numbers / acks ([`super::frame`]), status.
//!
//! Portable (macOS, Linux without libudev, Windows): `serialport` is built
//! with default features off; Linux enumeration falls back to sysfs.

use super::frame::{
    AckOutcome, AckStatus, AckTracker, DongleInfo, DongleStatus, Frame, FrameDecoder, MsgType, PairingOp, Target,
    PROTOCOL_VERSION,
};
use super::{LinkState, PairedDevice, PeripheralStatus, Transport};
use crate::hid_report::{HidReport, KeyboardLeds};
use crate::{InputError, Result};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Espressif USB vendor ID (ESP32-S2/S3 native USB, TinyUSB and USB-Serial-JTAG).
pub const ESPRESSIF_VID: u16 = 0x303A;
/// USB-UART bridges found on ESP32-S3 dev boards ("UART" port): Silicon Labs
/// CP210x, WCH CH34x, FTDI. Listed so a dongle driven through its UART port
/// (needed when the native USB port is the HID link to the target) is found.
pub const UART_BRIDGE_VIDS: [u16; 3] = [0x10C4, 0x1A86, 0x0403];
pub const DEFAULT_BAUD: u32 = 921_600;
pub const FALLBACK_BAUD: u32 = 115_200;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(700);
const ACK_TIMEOUT: Duration = Duration::from_millis(150);
const STATUS_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDongle {
    pub port: String,
    pub vid: u16,
    pub pid: u16,
    pub product: Option<String>,
    pub serial_number: Option<String>,
    /// `true` for the Espressif VID (native USB), `false` for a generic USB-UART bridge.
    pub native_usb: bool,
}

/// Whether a port name looks like a USB CDC/serial device on its OS.
pub fn is_candidate_port_name(name: &str) -> bool {
    name.starts_with("/dev/cu.usbmodem")
        || name.starts_with("/dev/cu.usbserial")
        || name.starts_with("/dev/cu.wchusbserial")
        || name.starts_with("/dev/cu.SLAB_USBtoUART")
        || name.starts_with("/dev/ttyACM")
        || name.starts_with("/dev/ttyUSB")
        || (name.len() > 3 && name[..3].eq_ignore_ascii_case("COM") && name[3..].chars().all(|c| c.is_ascii_digit()))
}

/// Filter + order enumerated ports: Espressif first; on macOS drop the
/// `/dev/tty.*` twin of every `/dev/cu.*` (the call-in device blocks on DCD).
pub fn select_dongles(ports: Vec<DetectedDongle>) -> Vec<DetectedDongle> {
    let mut v: Vec<DetectedDongle> = ports
        .into_iter()
        .filter(|d| !d.port.starts_with("/dev/tty."))
        .filter(|d| d.vid == ESPRESSIF_VID || (UART_BRIDGE_VIDS.contains(&d.vid) && is_candidate_port_name(&d.port)))
        .map(|mut d| {
            d.native_usb = d.vid == ESPRESSIF_VID;
            d
        })
        .collect();
    v.sort_by(|a, b| b.native_usb.cmp(&a.native_usb).then_with(|| a.port.cmp(&b.port)));
    v
}

/// Enumerate candidate dongles (USB VID 303A, plus common USB-UART bridges).
/// Does not open any port.
pub fn detect_dongles() -> Vec<DetectedDongle> {
    let Ok(ports) = serialport::available_ports() else {
        return Vec::new();
    };
    select_dongles(
        ports
            .into_iter()
            .filter_map(|p| match p.port_type {
                serialport::SerialPortType::UsbPort(u) => Some(DetectedDongle {
                    port: p.port_name,
                    vid: u.vid,
                    pid: u.pid,
                    product: u.product,
                    serial_number: u.serial_number,
                    native_usb: false,
                }),
                _ => None,
            })
            .collect(),
    )
}

/// State shared between the API side and the reader thread.
struct Shared {
    tracker: Mutex<AckTracker>,
    status: Mutex<PeripheralStatus>,
    stop: AtomicBool,
}

pub struct SerialDongle {
    port_name: String,
    writer: Arc<Mutex<Box<dyn serialport::SerialPort>>>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

fn io_err(port: &str, e: impl std::fmt::Display) -> InputError {
    InputError::Os(format!("serial dongle {port}: {e}"))
}

fn open_port(port: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>> {
    let mut p = serialport::new(port, baud).timeout(Duration::from_millis(50)).open().map_err(|e| io_err(port, e))?;
    // Native USB CDC: DTR marks the host as present; keep RTS low so an
    // ESP32 auto-reset circuit / USB-Serial-JTAG does not reset the chip.
    let _ = p.write_request_to_send(false);
    let _ = p.write_data_terminal_ready(true);
    let _ = p.clear(serialport::ClearBuffer::Input);
    Ok(p)
}

/// Send Hello and wait for Info. Returns the info and any bytes already
/// decoded after it.
fn handshake(p: &mut dyn serialport::SerialPort, port: &str) -> Result<(DongleInfo, FrameDecoder)> {
    p.write_all(&Frame::new(MsgType::Hello, 0, &[PROTOCOL_VERSION]).encode()).map_err(|e| io_err(port, e))?;
    let mut dec = FrameDecoder::new();
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut buf = [0u8; 256];
    let mut version_mismatch = false;
    while Instant::now() < deadline {
        match p.read(&mut buf) {
            Ok(n) => dec.push(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(io_err(port, e)),
        }
        while let Some(f) = dec.next_frame() {
            match f.kind() {
                Some(MsgType::Info) => {
                    if let Some(info) = DongleInfo::decode(&f.payload) {
                        if info.protocol_version != PROTOCOL_VERSION {
                            return Err(io_err(
                                port,
                                format!("firmware speaks protocol v{} (host v{PROTOCOL_VERSION}); reflash", info.protocol_version),
                            ));
                        }
                        return Ok((info, dec));
                    }
                }
                Some(MsgType::Ack) if f.payload.get(1) == Some(&(AckStatus::VersionMismatch as u8)) => {
                    version_mismatch = true;
                }
                _ => {}
            }
        }
    }
    Err(io_err(
        port,
        if version_mismatch {
            "dongle rejected the protocol version; reflash the firmware".to_string()
        } else {
            "no answer to Hello (not an AetherLink dongle, wrong baud rate, or firmware not running)".to_string()
        },
    ))
}

fn link_state(s: &DongleStatus) -> LinkState {
    let usb = s.usb_mounted && matches!(s.target, Some(Target::Usb | Target::Both));
    let ble = s.ble_connected && matches!(s.target, Some(Target::Ble | Target::Both));
    match (usb, ble) {
        (true, true) => LinkState::Connected("USB+BLE".into()),
        (true, false) => LinkState::Connected("USB".into()),
        (false, true) => LinkState::Connected("BLE".into()),
        _ => LinkState::Listening,
    }
}

/// Apply one frame received from the dongle to the shared state.
fn apply_frame(shared: &Shared, f: &Frame) {
    match f.kind() {
        Some(MsgType::Ack) if f.payload.len() == 2 => {
            let out = shared.tracker.lock().expect("tracker lock").on_ack(f.payload[0], f.payload[1]);
            if let AckOutcome::Acked { msg_type, status } = out
                && status != AckStatus::Ok
            {
                let mut st = shared.status.lock().expect("status lock");
                if status == AckStatus::NotConnected {
                    st.reports_dropped += 1;
                } else {
                    st.last_error = Some(format!("dongle rejected {msg_type:?}: {status:?}"));
                }
            }
        }
        Some(MsgType::Info) => {
            if let Some(i) = DongleInfo::decode(&f.payload) {
                shared.status.lock().expect("status lock").dongle = Some(i);
            }
        }
        Some(MsgType::Status) => {
            if let Some(s) = DongleStatus::decode(&f.payload) {
                let mut st = shared.status.lock().expect("status lock");
                let new_state = link_state(&s);
                if matches!(new_state, LinkState::Connected(_)) && !matches!(st.state, LinkState::Connected(_)) {
                    st.connection_epoch += 1;
                }
                st.state = new_state;
                st.leds = KeyboardLeds::from_byte(s.leds);
                st.dongle_status = Some(s);
            }
        }
        Some(MsgType::Leds) if f.payload.len() == 1 => {
            shared.status.lock().expect("status lock").leds = KeyboardLeds::from_byte(f.payload[0]);
        }
        _ => {}
    }
}

impl SerialDongle {
    /// Open `port`, handshake at 921600 then 115200 baud, select `target`.
    pub fn open(port: &str, target: Target) -> Result<Self> {
        let mut last = None;
        for baud in [DEFAULT_BAUD, FALLBACK_BAUD] {
            let mut p = open_port(port, baud)?;
            match handshake(p.as_mut(), port) {
                Ok((info, dec)) => return Self::run(port, p, info, dec, target),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io_err(port, "handshake failed")))
    }

    fn run(
        port: &str,
        p: Box<dyn serialport::SerialPort>,
        info: DongleInfo,
        dec: FrameDecoder,
        target: Target,
    ) -> Result<Self> {
        let reader_port = p.try_clone().map_err(|e| io_err(port, e))?;
        let shared = Arc::new(Shared {
            tracker: Mutex::new(AckTracker::new(16, ACK_TIMEOUT, 3)),
            status: Mutex::new(PeripheralStatus { state: LinkState::Listening, dongle: Some(info), ..Default::default() }),
            stop: AtomicBool::new(false),
        });
        let writer = Arc::new(Mutex::new(p));
        let reader = {
            let (shared, writer, name) = (shared.clone(), writer.clone(), port.to_string());
            std::thread::Builder::new()
                .name("aether-hid-dongle".into())
                .spawn(move || reader_loop(reader_port, dec, shared, writer, name))
                .map_err(InputError::Io)?
        };
        let mut d = Self { port_name: port.to_string(), writer, shared, reader: Some(reader) };
        d.command(MsgType::SetTarget, &[target as u8])?;
        d.command(MsgType::GetStatus, &[])?;
        Ok(d)
    }

    fn command(&mut self, t: MsgType, payload: &[u8]) -> Result<()> {
        let (_, bytes) = self.shared.tracker.lock().expect("tracker lock").prepare(t, payload, Instant::now());
        let mut w = self.writer.lock().expect("writer lock");
        w.write_all(&bytes).map_err(|e| {
            let err = io_err(&self.port_name, e);
            self.shared.status.lock().expect("status lock").last_error = Some(err.to_string());
            err
        })
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

fn reader_loop(
    mut port: Box<dyn serialport::SerialPort>,
    mut dec: FrameDecoder,
    shared: Arc<Shared>,
    writer: Arc<Mutex<Box<dyn serialport::SerialPort>>>,
    name: String,
) {
    let mut buf = [0u8; 512];
    let mut last_poll = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => dec.push(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                let mut st = shared.status.lock().expect("status lock");
                st.state = LinkState::Stopped;
                st.last_error = Some(format!("serial dongle {name}: {e} (unplugged?)"));
                return;
            }
        }
        while let Some(f) = dec.next_frame() {
            apply_frame(&shared, &f);
        }
        let now = Instant::now();
        let mut out = shared.tracker.lock().expect("tracker lock").poll_timeouts(now);
        if now.duration_since(last_poll) >= STATUS_POLL {
            last_poll = now;
            out.push(shared.tracker.lock().expect("tracker lock").prepare(MsgType::GetStatus, &[], now).1);
        }
        if !out.is_empty() {
            let mut w = writer.lock().expect("writer lock");
            for b in out {
                let _ = w.write_all(&b);
            }
        }
    }
}

impl Transport for SerialDongle {
    fn send_report(&mut self, r: &HidReport) -> Result<()> {
        let t = MsgType::for_report_id(r.id)
            .ok_or_else(|| InputError::InvalidArgument(format!("report id {} has no dongle message", r.id)))?;
        if Some(r.data.len()) != t.fixed_len() {
            return Err(InputError::InvalidArgument(format!("report {} has {} bytes", r.id, r.data.len())));
        }
        self.command(t, &r.data)?;
        self.shared.status.lock().expect("status lock").reports_sent += 1;
        Ok(())
    }

    fn status(&self) -> PeripheralStatus {
        let mut s = self.shared.status.lock().expect("status lock").clone();
        let t = self.shared.tracker.lock().expect("tracker lock");
        s.reports_dropped += t.dropped;
        s
    }

    fn paired_devices(&self) -> Result<Vec<PairedDevice>> {
        Err(InputError::Unsupported("the dongle only reports a bond count (PeripheralStatus::dongle_status)"))
    }

    fn connect(&mut self, _address: &str) -> Result<()> {
        Err(InputError::Unsupported(
            "a BLE HID peripheral cannot initiate connections; the bonded host reconnects when the dongle advertises",
        ))
    }

    fn disconnect(&mut self) -> Result<()> {
        self.command(MsgType::Pairing, &[PairingOp::Disconnect as u8, 0])
    }

    fn set_pairing(&mut self, enabled: bool, timeout: Duration) -> Result<()> {
        let op = if enabled { PairingOp::StartAdvertising } else { PairingOp::StopAdvertising };
        self.command(MsgType::Pairing, &[op as u8, timeout.as_secs().min(255) as u8])
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        self.shared.status.lock().expect("status lock").state = LinkState::Stopped;
    }
}

impl Drop for SerialDongle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(port: &str, vid: u16) -> DetectedDongle {
        DetectedDongle { port: port.into(), vid, pid: 0x1001, product: None, serial_number: None, native_usb: false }
    }

    #[test]
    fn port_selection() {
        let v = select_dongles(vec![
            d("/dev/tty.usbmodem1101", 0x303A),
            d("/dev/cu.usbmodem1101", 0x303A),
            d("/dev/cu.usbserial-0001", 0x10C4),
            d("/dev/cu.Bluetooth-Incoming-Port", 0x10C4),
            d("/dev/ttyACM0", 0x2341), // Arduino, not ours
            d("COM7", 0x303A),
        ]);
        let names: Vec<&str> = v.iter().map(|d| d.port.as_str()).collect();
        assert_eq!(names, vec!["/dev/cu.usbmodem1101", "COM7", "/dev/cu.usbserial-0001"]);
        assert!(v[0].native_usb && !v[2].native_usb);
        assert!(is_candidate_port_name("/dev/ttyACM0"));
        assert!(is_candidate_port_name("com12"));
        assert!(!is_candidate_port_name("COMX"));
        assert!(!is_candidate_port_name("/dev/ttyS0"));
    }

    #[test]
    fn status_frames_update_shared_state() {
        let shared = Shared {
            tracker: Mutex::new(AckTracker::new(8, ACK_TIMEOUT, 3)),
            status: Mutex::new(PeripheralStatus::default()),
            stop: AtomicBool::new(false),
        };
        let s = DongleStatus { target: Some(Target::Ble), ble_connected: true, leds: 0x02, ..Default::default() };
        apply_frame(&shared, &Frame::new(MsgType::Status, 0, &s.encode()));
        {
            let st = shared.status.lock().unwrap();
            assert_eq!(st.state, LinkState::Connected("BLE".into()));
            assert_eq!(st.connection_epoch, 1);
            assert!(st.leds.caps_lock);
        }
        // Same state again: no new epoch. USB mounted but target BLE: still BLE only.
        let s2 = DongleStatus { usb_mounted: true, ..s };
        apply_frame(&shared, &Frame::new(MsgType::Status, 0, &s2.encode()));
        assert_eq!(shared.status.lock().unwrap().connection_epoch, 1);
        assert_eq!(link_state(&DongleStatus { target: Some(Target::Both), ..s2 }), LinkState::Connected("USB+BLE".into()));
        assert_eq!(link_state(&DongleStatus::default()), LinkState::Listening);

        // Ack bookkeeping: NotConnected counts as dropped, other errors are surfaced.
        let (seq, _) = shared.tracker.lock().unwrap().prepare(MsgType::Keyboard, &[0; 8], Instant::now());
        apply_frame(&shared, &Frame::new(MsgType::Ack, 0, &[seq, AckStatus::NotConnected as u8]));
        let (seq, _) = shared.tracker.lock().unwrap().prepare(MsgType::SetTarget, &[9], Instant::now());
        apply_frame(&shared, &Frame::new(MsgType::Ack, 0, &[seq, AckStatus::BadArgument as u8]));
        let st = shared.status.lock().unwrap();
        assert_eq!(st.reports_dropped, 1);
        assert!(st.last_error.as_deref().unwrap().contains("SetTarget"));
        assert_eq!(shared.tracker.lock().unwrap().in_flight(), 0);
    }

    #[test]
    fn detection_does_not_panic() {
        let _ = detect_dongles();
    }
}
