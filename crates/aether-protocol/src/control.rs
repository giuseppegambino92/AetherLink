//! Session control messages.

use crate::{ClipboardMode, Rect, VideoCodec};
use serde::{Deserialize, Serialize};

/// A physical or virtual display on the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Position & size in the host's global desktop space (physical pixels).
    pub bounds: Rect,
    /// Backing scale factor (2.0 on Retina).
    pub scale: f32,
    pub refresh_hz: f32,
    pub primary: bool,
    /// True for AetherLink-created headless displays (Ghost Mode).
    pub is_virtual: bool,
}

/// A capturable window on the host (for single-window sessions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_name: String,
    pub title: String,
    pub bounds: Rect,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTarget {
    Display { display_id: u32 },
    Window { window_id: u64 },
}

/// Client's streaming preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualitySettings {
    pub max_fps: u16,
    /// Target bitrate; `None` = adaptive.
    pub bitrate_kbps: Option<u32>,
    pub preferred_codecs: Vec<VideoCodec>,
    /// Request 4:4:4 chroma when the encoder supports it.
    pub prefer_444: bool,
    /// Enable the hybrid lossless dirty-tile refinement for static regions.
    pub lossless_text: bool,
    /// Downscale factor applied on the host (1.0 = native).
    pub scale: f32,
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            max_fps: 60,
            bitrate_kbps: None,
            preferred_codecs: vec![VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Tiles],
            prefer_444: true,
            lossless_text: true,
            scale: 1.0,
        }
    }
}

/// Live performance numbers, shown in the HUD and recorded by the DVR.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TelemetrySample {
    pub pts_us: u64,
    pub rtt_ms: f32,
    pub fps: f32,
    pub bitrate_kbps: u32,
    pub encode_ms: f32,
    pub decode_ms: f32,
    pub packet_loss: f32,
    pub host_cpu_percent: f32,
    pub host_mem_percent: f32,
    pub host_net_tx_kbps: u32,
    pub host_net_rx_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Host → client after authentication.
    SessionReady { displays: Vec<DisplayInfo>, windows: Vec<WindowInfo>, active: CaptureTarget },
    DisplaysChanged { displays: Vec<DisplayInfo> },
    SelectTarget { target: CaptureTarget },
    SetQuality(QualitySettings),
    RequestKeyframe { display_id: u32 },
    /// Privacy curtain on the host's physical screen.
    SetGhostMode { enabled: bool },
    /// Create/remove a headless virtual display on the host.
    SetVirtualDisplay { enabled: bool, width: u32, height: u32, refresh_hz: u32 },
    SetClipboardMode { mode: ClipboardMode },
    SetAudio { enabled: bool },
    Ping { nonce: u64, sent_us: u64 },
    Pong { nonce: u64, sent_us: u64 },
    Telemetry(TelemetrySample),
    /// Human-readable, localizable error. `code` is a stable i18n key, e.g. `"error.capture.permission"`.
    Error { code: String, detail: String },
    Bye,
    /// Host → client after `SessionReady`: what this session may do. Requests outside it are
    /// refused by the host with `Error { code: "errors.permission.<capability>" }`.
    Permissions(SessionPermissions),
}

/// Capabilities a host grants to a remote session. Viewing the screen is always allowed once
/// authenticated; everything else is opt-in per capability (least privilege).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionPermissions {
    /// Keyboard and mouse injection.
    pub input: bool,
    /// Clipboard synchronization.
    pub clipboard: bool,
    /// Files dropped onto the session window (pushed to the host's Downloads).
    pub file_transfer: bool,
    /// Remote drives (the host's shared folders, mounted on the client).
    pub drives: bool,
    /// ⌘K semantic actions on the host's accessibility tree.
    pub semantic_ui: bool,
    /// System-audio streaming.
    pub audio: bool,
    /// Remote shell (Quake terminal).
    pub terminal: bool,
    /// TCP tunnels to the host's loopback services (only to `tunnel_allowed_ports`).
    pub port_forwarding: bool,
    /// USB device redirection.
    pub usb: bool,
    /// Ghost Mode (darkens the host's physical screen).
    pub ghost_mode: bool,
    /// Headless virtual displays on the host.
    pub virtual_display: bool,
}

impl Default for SessionPermissions {
    /// Remote assistance: view, control, clipboard, files, drives, ⌘K, audio and virtual
    /// displays. Terminal, port forwarding, USB and Ghost Mode must be enabled explicitly.
    fn default() -> Self {
        Self {
            input: true,
            clipboard: true,
            file_transfer: true,
            drives: true,
            semantic_ui: true,
            audio: true,
            terminal: false,
            port_forwarding: false,
            usb: false,
            ghost_mode: false,
            virtual_display: true,
        }
    }
}

impl SessionPermissions {
    /// Nothing but viewing the screen.
    pub const VIEW_ONLY: Self = Self {
        input: false,
        clipboard: false,
        file_transfer: false,
        drives: false,
        semantic_ui: false,
        audio: false,
        terminal: false,
        port_forwarding: false,
        usb: false,
        ghost_mode: false,
        virtual_display: false,
    };
}
