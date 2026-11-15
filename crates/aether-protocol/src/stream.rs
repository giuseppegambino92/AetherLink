//! Stream multiplexing: each QUIC stream opens with a [`StreamHeader`].

use crate::{AudioPacket, CursorUpdate, InputEvent, TelemetrySample, TunnelProtocol};
use serde::{Deserialize, Serialize};

/// What a QUIC stream carries. After the header, frames on the stream are:
///
/// | kind            | direction | frame type                    |
/// |-----------------|-----------|-------------------------------|
/// | `Control`       | bi        | [`crate::ControlMessage`] (after `Hello`/`AuthMessage`) |
/// | `Video`         | uni h→c   | [`crate::VideoMessage`]        |
/// | `Audio`         | uni h→c   | [`crate::AudioPacket`]         |
/// | `Input`         | uni c→h   | [`crate::InputEvent`] (reliable: keys, buttons, text) |
/// | `Clipboard`     | bi        | [`crate::ClipboardMessage`]    |
/// | `Vfs`           | bi        | [`crate::VfsEnvelope`] of request/response |
/// | `FileTransfer`  | uni       | [`crate::TransferMessage`]     |
/// | `Dom`           | bi        | [`crate::DomMessage`]          |
/// | `PortTunnel`    | bi        | raw TCP bytes after [`crate::PortTunnelOpen`] reply |
/// | `PortControl`   | bi        | [`crate::PortTunnelMessage`]   |
/// | `Terminal`      | bi        | [`crate::TerminalMessage`]     |
/// | `Usb`           | bi        | [`crate::UsbMessage`]          |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Control,
    Video { display_id: u32 },
    Audio,
    Input,
    Clipboard,
    Vfs,
    FileTransfer { transfer_id: u64 },
    Dom,
    PortTunnel { tunnel_id: u64, remote_port: u16, protocol: TunnelProtocol },
    PortControl,
    Terminal { terminal_id: u64 },
    Usb { device_id: u32 },
}

/// First frame on every stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHeader {
    pub kind: StreamKind,
}

/// Unreliable QUIC datagrams for latency-critical, superseding data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Datagram {
    /// Pointer motion / scroll — newer values supersede lost ones.
    Input { seq: u64, event: InputEvent },
    /// Host cursor position (shape changes go reliably on the video stream).
    Cursor(CursorUpdate),
    Ping { nonce: u64, sent_us: u64 },
    Pong { nonce: u64, sent_us: u64 },
    Telemetry(TelemetrySample),
    /// Host system audio (Opus, ≤ 20 ms per packet). Late packets are concealed, never retransmitted.
    Audio(AudioPacket),
}
