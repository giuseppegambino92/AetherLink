//! Engine → UI notifications and byte sinks.

use serde::Serialize;
use std::sync::Arc;

/// Receives engine events. The desktop app forwards them as Tauri events
/// (`name` is the event name, `payload` is camelCase JSON).
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, name: &str, payload: serde_json::Value);
}

/// Binary stream to the UI (video frames, terminal output).
pub type ByteSink = Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>;

pub(crate) fn emit<T: Serialize>(sink: &dyn EventSink, name: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(v) => sink.emit(name, v),
        Err(e) => tracing::warn!("event {name} not serializable: {e}"),
    }
}

pub mod names {
    pub const SESSION_STATE: &str = "session-state";
    pub const SESSION_TELEMETRY: &str = "session-telemetry";
    pub const SESSION_DISPLAYS: &str = "session-displays";
    pub const DOM_UPDATED: &str = "dom-updated";
    pub const TRANSFER_PROGRESS: &str = "transfer-progress";
    pub const INCOMING_REQUEST: &str = "incoming-request";
    pub const LAN_PEERS_CHANGED: &str = "lan-peers-changed";
    pub const HOST_SESSIONS_CHANGED: &str = "host-sessions-changed";
    pub const GHOST_MODE: &str = "ghost-mode";
    pub const CLIPBOARD_SYNCED: &str = "clipboard-synced";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Connecting,
    Authenticating,
    Connected,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateEvent {
    pub session_id: u64,
    pub state: SessionState,
    /// i18n key (`errors.*`) when failed/closed with an error.
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub peer_name: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgressEvent {
    pub session_id: u64,
    pub transfer_id: u64,
    pub direction: &'static str,
    pub name: Option<String>,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub state: &'static str,
    pub error: Option<String>,
}
