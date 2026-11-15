//! Linux backlight: read `/sys/class/backlight`, write through systemd-logind.
//!
//! * Reading needs no privileges: `brightness`, `max_brightness` and `type` are world-readable.
//! * Writing `brightness` in sysfs needs root (or a udev rule), so changes go through
//!   `org.freedesktop.login1.Session.SetBrightness(subsystem, name, value)` on the system bus
//!   (systemd ≥ 243), which logind grants to the active local session; the object path
//!   `/org/freedesktop/login1/session/auto` resolves to the caller's session. When logind is
//!   unreachable or refuses, a direct sysfs write is attempted before giving up.
//! * Laptops often expose the same panel twice (`acpi_video0` = firmware, `intel_backlight` =
//!   raw); following the kernel ABI documentation only devices of the highest-priority type
//!   present are used (`firmware` > `platform` > `raw`).
//!
//! Everything takes the sysfs root and the writer as parameters so the logic is tested against
//! a fake sysfs tree and a mock logind service.

use crate::brightness::{BrightnessSnapshot, stable_id, to_level, to_raw};
use crate::{DisplayError, Result};
use std::path::{Path, PathBuf};

pub(crate) const SYSFS_BACKLIGHT: &str = "/sys/class/backlight";
const LOGIND_DEST: &str = "org.freedesktop.login1";
const LOGIND_SESSION_PATH: &str = "/org/freedesktop/login1/session/auto";
const LOGIND_SESSION_IFACE: &str = "org.freedesktop.login1.Session";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Raw,
    Platform,
    Firmware,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Device {
    pub name: String,
    pub brightness: u32,
    pub max: u32,
    pub kind: Kind,
}

impl Device {
    pub fn id(&self) -> u32 {
        stable_id(&self.name)
    }
}

/// Sets the raw `brightness` value of a backlight device.
pub(crate) trait Writer {
    fn set(&self, name: &str, value: u32) -> Result<()>;
}

/// Every readable backlight device under `root`, sorted by name.
pub(crate) fn all_devices(root: &Path) -> Result<Vec<Device>> {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(DisplayError::Io(format!("{}: {e}", root.display()))),
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // sysfs entries are symlinks into /sys/devices; joining follows them.
        let dir = root.join(&name);
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok().and_then(|s| s.trim().parse::<u32>().ok());
        let (Some(brightness), Some(max)) = (read("brightness"), read("max_brightness")) else { continue };
        if max == 0 {
            continue;
        }
        let kind = match std::fs::read_to_string(dir.join("type")).unwrap_or_default().trim() {
            "firmware" => Kind::Firmware,
            "platform" => Kind::Platform,
            _ => Kind::Raw,
        };
        out.push(Device { name, brightness, max, kind });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The devices Ghost Mode drives: those of the highest-priority type present.
pub(crate) fn devices(root: &Path) -> Result<Vec<Device>> {
    let mut all = all_devices(root)?;
    let best = all.iter().map(|d| d.kind).max();
    all.retain(|d| Some(d.kind) == best);
    Ok(all)
}

pub(crate) fn read_all_at(root: &Path) -> Result<BrightnessSnapshot> {
    Ok(BrightnessSnapshot { levels: devices(root)?.iter().map(|d| (d.id(), to_level(d.brightness, 0, d.max))).collect() })
}

pub(crate) fn set_at(root: &Path, writer: &dyn Writer, id: u32, level: f32) -> Result<()> {
    let devs = all_devices(root)?;
    let d = devs
        .iter()
        .find(|d| d.id() == id)
        .ok_or_else(|| DisplayError::Os(format!("backlight device {id:#010x} is no longer present")))?;
    writer.set(&d.name, to_raw(level, 0, d.max))
}

/// `SetBrightness` through systemd-logind.
pub(crate) struct Logind {
    conn: zbus::blocking::Connection,
}

impl Logind {
    pub fn system() -> Result<Self> {
        zbus::blocking::Connection::system()
            .map(Self::with_connection)
            .map_err(|e| DisplayError::Os(format!("system D-Bus: {e}")))
    }

    pub fn with_connection(conn: zbus::blocking::Connection) -> Self {
        Self { conn }
    }
}

impl Writer for Logind {
    fn set(&self, name: &str, value: u32) -> Result<()> {
        self.conn
            .call_method(Some(LOGIND_DEST), LOGIND_SESSION_PATH, Some(LOGIND_SESSION_IFACE), "SetBrightness", &("backlight", name, value))
            .map(|_| ())
            .map_err(|e| DisplayError::Os(format!("logind SetBrightness({name}, {value}): {e}")))
    }
}

/// Direct sysfs write (root, or a udev rule granting the user write access).
pub(crate) struct Sysfs(pub PathBuf);

impl Writer for Sysfs {
    fn set(&self, name: &str, value: u32) -> Result<()> {
        let p = self.0.join(name).join("brightness");
        std::fs::write(&p, value.to_string()).map_err(|e| DisplayError::Io(format!("{}: {e}", p.display())))
    }
}

/// logind first, sysfs second.
struct Auto;

impl Writer for Auto {
    fn set(&self, name: &str, value: u32) -> Result<()> {
        let logind = Logind::system().and_then(|l| l.set(name, value));
        match logind {
            Ok(()) => Ok(()),
            Err(e) => Sysfs(PathBuf::from(SYSFS_BACKLIGHT))
                .set(name, value)
                .map_err(|e2| DisplayError::Os(format!("{e}; direct sysfs write failed too: {e2}"))),
        }
    }
}

pub fn read_all() -> Result<BrightnessSnapshot> {
    read_all_at(Path::new(SYSFS_BACKLIGHT))
}

pub fn set(id: u32, level: f32) -> Result<()> {
    set_at(Path::new(SYSFS_BACKLIGHT), &Auto, id, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn fake_device(root: &Path, name: &str, kind: &str, brightness: u32, max: u32) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("brightness"), format!("{brightness}\n")).unwrap();
        std::fs::write(d.join("max_brightness"), format!("{max}\n")).unwrap();
        std::fs::write(d.join("type"), format!("{kind}\n")).unwrap();
    }

    fn raw_value(root: &Path, name: &str) -> u32 {
        std::fs::read_to_string(root.join(name).join("brightness")).unwrap().trim().parse().unwrap()
    }

    #[test]
    fn missing_sysfs_dir_means_no_devices() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_all_at(&dir.path().join("nope")).unwrap(), BrightnessSnapshot::default());
    }

    #[test]
    fn prefers_firmware_over_raw_and_skips_broken_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_device(root, "intel_backlight", "raw", 9000, 19393);
        fake_device(root, "acpi_video0", "firmware", 40, 100);
        fake_device(root, "zero_max", "firmware", 0, 0);
        std::fs::create_dir_all(root.join("garbage")).unwrap();
        std::fs::write(root.join("garbage/brightness"), "x").unwrap();
        let snap = read_all_at(root).unwrap();
        assert_eq!(snap.levels, vec![(stable_id("acpi_video0"), 0.4)]);
        // `zero_max` (max_brightness = 0) and `garbage` (no max_brightness) are broken: skipped.
        assert_eq!(all_devices(root).unwrap().len(), 2);

        std::fs::remove_dir_all(root.join("acpi_video0")).unwrap();
        std::fs::remove_dir_all(root.join("zero_max")).unwrap();
        fake_device(root, "ddcci5", "raw", 30, 100);
        let snap = read_all_at(root).unwrap();
        assert_eq!(snap.levels.len(), 2, "all devices of the best type: {snap:?}");
    }

    #[test]
    fn dim_and_exact_restore_through_sysfs_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_device(root, "intel_backlight", "raw", 9001, 19393);
        fake_device(root, "ddcci5", "raw", 37, 100);
        let w = Sysfs(root.to_path_buf());
        let before = read_all_at(root).unwrap();
        for (id, _) in &before.levels {
            set_at(root, &w, *id, 0.0).unwrap();
        }
        assert_eq!((raw_value(root, "intel_backlight"), raw_value(root, "ddcci5")), (0, 0));
        assert!(read_all_at(root).unwrap().levels.iter().all(|(_, v)| *v == 0.0));
        // Restore through a JSON round trip, as the crash-restore file does.
        let restored: BrightnessSnapshot = serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
        for (id, v) in &restored.levels {
            set_at(root, &w, *id, *v).unwrap();
        }
        assert_eq!((raw_value(root, "intel_backlight"), raw_value(root, "ddcci5")), (9001, 37));
        assert!(matches!(set_at(root, &w, 12345, 0.5), Err(DisplayError::Os(_))));
    }

    /// A stand-in for systemd-logind's session object.
    struct MockSession {
        root: PathBuf,
        calls: Arc<Mutex<Vec<(String, String, u32)>>>,
    }

    #[zbus::interface(name = "org.freedesktop.login1.Session")]
    impl MockSession {
        fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::fdo::Result<()> {
            self.calls.lock().push((subsystem.into(), name.into(), brightness));
            if subsystem != "backlight" {
                return Err(zbus::fdo::Error::InvalidArgs(format!("subsystem {subsystem}")));
            }
            Sysfs(self.root.clone()).set(name, brightness).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
        }
    }

    #[test]
    #[ignore = "needs a D-Bus session bus (dbus-run-session, see ci/linux-test.sh)"]
    fn logind_set_brightness_over_dbus_against_mock_service() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fake_device(&root, "intel_backlight", "raw", 512, 1024);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _server = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name(LOGIND_DEST)
            .unwrap()
            .serve_at(LOGIND_SESSION_PATH, MockSession { root: root.clone(), calls: calls.clone() })
            .unwrap()
            .build()
            .unwrap();
        let logind = Logind::with_connection(zbus::blocking::Connection::session().unwrap());
        let id = stable_id("intel_backlight");
        let before = read_all_at(&root).unwrap();
        assert_eq!(before.levels, vec![(id, 0.5)]);
        set_at(&root, &logind, id, 0.0).unwrap();
        assert_eq!(raw_value(&root, "intel_backlight"), 0);
        set_at(&root, &logind, id, 0.5).unwrap();
        assert_eq!(raw_value(&root, "intel_backlight"), 512);
        assert_eq!(
            *calls.lock(),
            vec![("backlight".into(), "intel_backlight".into(), 0), ("backlight".into(), "intel_backlight".into(), 512)]
        );
        // Errors from logind surface as DisplayError::Os with the D-Bus error name.
        let err = logind.set("does_not_exist", 1).unwrap_err().to_string();
        assert!(err.contains("does_not_exist"), "{err}");
    }

    /// Real hardware: reads the levels and writes the *same* values back through logind.
    #[test]
    #[ignore = "needs a real backlight device and an active logind session"]
    fn real_backlight_read_and_rewrite_same_levels() {
        let snap = read_all().unwrap();
        assert!(!snap.levels.is_empty(), "no backlight devices");
        for (id, v) in &snap.levels {
            set(*id, *v).unwrap();
        }
        assert_eq!(read_all().unwrap(), snap);
    }
}
