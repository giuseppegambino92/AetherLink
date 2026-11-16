//! Session handshake & authentication messages (carried on the Control stream).
//!
//! Transport security is TLS 1.3 inside QUIC with each side presenting a
//! certificate bound to its Ed25519 identity key. On top of that:
//! * **Trusted fleet / paired LAN**: the peer's identity key is pinned → [`AuthMethod::TrustedKey`].
//! * **Ephemeral session ID / first LAN pairing**: a SPAKE2 password exchange
//!   bound to the TLS exporter secret → [`AuthMethod::Password`]. A relay or
//!   MITM cannot learn the password nor splice the session.

use crate::{DeviceId, OsKind};
use serde::{Deserialize, Serialize};

/// How the client is reaching the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectMode {
    /// Direct on the local network (mDNS discovered).
    LanDirect,
    /// Unattended access to a previously paired device.
    TrustedFleet,
    /// Ephemeral 9-digit ID + one-time password.
    EphemeralId,
}

/// Feature flags a peer supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub video_h264: bool,
    pub video_hevc: bool,
    pub video_hevc_444: bool,
    pub video_av1: bool,
    pub lossless_tiles: bool,
    pub audio: bool,
    pub input_injection: bool,
    pub clipboard: bool,
    pub file_transfer: bool,
    pub vfs_mount: bool,
    pub semantic_dom: bool,
    pub port_tunnels: bool,
    pub terminal: bool,
    pub usb_redirect: bool,
    pub ghost_mode: bool,
    pub time_travel: bool,
}

/// First message sent by each side on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub device_id: DeviceId,
    pub device_name: String,
    pub os: OsKind,
    pub os_version: String,
    pub app_version: String,
    /// Ed25519 identity public key.
    pub public_key: [u8; 32],
    pub capabilities: Capabilities,
    pub mode: ConnectMode,
    /// BCP-47 UI language of the sender ("it", "en") — used for localized error text.
    pub locale: String,
}

/// Authentication method the host requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// Client key is already trusted; optional local biometric gate already passed on the host.
    TrustedKey,
    /// SPAKE2 with the rotating one-time password or the 6-digit pairing PIN.
    Password,
    /// Host user must click "Accept" in an on-screen prompt.
    InteractiveConsent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMessage {
    /// Host → client: which method is required next.
    Required { method: AuthMethod },
    /// SPAKE2 message from either side (side A = client, side B = host).
    Spake2 { message: Vec<u8> },
    /// Key-confirmation MAC: BLAKE3-keyed(spake_key, transcript || tls_exporter).
    Confirm { mac: [u8; 32] },
    /// Final verdict from host.
    Result { accepted: bool, reason: Option<AuthRejectReason>, persist_trust: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthRejectReason {
    WrongPassword,
    UnknownKey,
    DeniedByUser,
    TooManyAttempts,
    VersionMismatch,
    HostBusy,
    /// The host requires local owner verification (Touch ID / Windows Hello / password) for
    /// trusted devices, and it failed, timed out or is unavailable (fail-closed).
    OwnerVerificationFailed,
}
