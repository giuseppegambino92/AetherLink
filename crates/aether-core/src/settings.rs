//! Persisted user settings (shared by desktop and mobile front-ends).

use crate::Result;
use aether_protocol::ClipboardMode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Language {
    Auto,
    It,
    En,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub language: Language,
    pub device_name: String,
    pub clipboard_mode: ClipboardMode,
    /// Allow unattended access from trusted devices.
    pub allow_unattended: bool,
    /// Require TouchID / Windows Hello before accepting a trusted session.
    pub require_biometric: bool,
    /// Ask the local user to accept incoming sessions instead of using the one-time password.
    pub require_consent: bool,
    /// `host:port` of the self-hosted rendezvous/relay server.
    pub relay_server: Option<String>,
    pub stun_servers: Vec<String>,
    pub advertise_on_lan: bool,
    pub hud_hotkey: String,
    pub palette_hotkey: String,
    pub ghost_hotkey: String,
    pub password_lifetime_secs: u64,
    pub max_fps: u16,
    pub prefer_444: bool,
    pub lossless_text: bool,
    pub dvr_enabled: bool,
    pub dvr_window_secs: u32,
    /// Ports the host allows to be tunneled (empty = any loopback port).
    pub tunnel_allowed_ports: Vec<u16>,
    pub mount_root: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::Auto,
            device_name: default_device_name(),
            clipboard_mode: ClipboardMode::Bidirectional,
            allow_unattended: true,
            require_biometric: false,
            require_consent: false,
            relay_server: None,
            stun_servers: vec!["stun.l.google.com:19302".into(), "stun.cloudflare.com:3478".into()],
            advertise_on_lan: true,
            hud_hotkey: "CmdOrCtrl+Shift+H".into(),
            palette_hotkey: "CmdOrCtrl+K".into(),
            ghost_hotkey: "CmdOrCtrl+Shift+G".into(),
            password_lifetime_secs: 30 * 60,
            max_fps: 60,
            prefer_444: true,
            lossless_text: true,
            dvr_enabled: true,
            dvr_window_secs: 180,
            tunnel_allowed_ports: vec![],
            mount_root: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Effective UI language for a given OS locale string.
    pub fn resolved_language(&self, os_locale: &str) -> Language {
        match self.language {
            Language::Auto if os_locale.to_ascii_lowercase().starts_with("it") => Language::It,
            Language::Auto => Language::En,
            other => other,
        }
    }
}

/// Platform config directory: `~/Library/Application Support/AetherLink` etc.
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("AetherLink")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("AetherLink")
}

fn default_device_name() -> String {
    crate::sysinfo::System::host_name()
        .map(|h| h.trim_end_matches(".local").to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "AetherLink".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());

        let s = Settings { language: Language::It, clipboard_mode: ClipboardMode::PushOnly, ..Settings::default() };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);

        // Older/partial files fill in defaults.
        std::fs::write(&path, br#"{"language":"en","maxFps":120}"#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.language, Language::En);
        assert_eq!(s.max_fps, 120);
        assert_eq!(s.dvr_window_secs, 180);
    }

    #[test]
    fn language_resolution() {
        let s = Settings::default();
        assert_eq!(s.resolved_language("it-IT"), Language::It);
        assert_eq!(s.resolved_language("en_US"), Language::En);
        assert_eq!(s.resolved_language("de"), Language::En);
        let forced = Settings { language: Language::It, ..Settings::default() };
        assert_eq!(forced.resolved_language("en"), Language::It);
    }
}
