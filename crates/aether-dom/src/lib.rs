//! # aether-dom
//!
//! Semantic UI tree for the remote action palette (`Cmd+K`):
//!
//! * [`DomProvider`] / [`create_provider`] — reads the frontmost window's
//!   accessibility tree on the host (macOS `AXUIElement`), performs actions
//!   and sets values.
//! * [`DomTree`] — revisioned mirror (snapshot + diffs) with breadcrumbs and
//!   case/diacritics-insensitive fuzzy [`DomTree::search`].
//! * [`diff`] — id-keyed snapshot diff (`apply_diff(old, diff(old, new)) == new`).
//! * [`context::active_working_directory`] — "where is the user working"
//!   (Finder window, terminal cwd, IDE project) for the Quake terminal.

use aether_protocol::{DomAction, DomSnapshot};

pub mod context;
mod diff;
pub mod fuzzy;
mod tree;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(any(target_os = "windows", test))]
mod windows_context;
#[cfg(any(target_os = "windows", test))]
pub mod windows_roles;

pub use diff::{diff, is_noop, needs_snapshot};
pub use tree::{
    derive_container_names, label, prune_to_interactive, DomTree, SearchHit, ACTIONABLE_BONUS, BREADCRUMB_SEPARATOR,
};

#[derive(Debug, thiserror::Error)]
pub enum DomError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("permission denied: {0}")]
    PermissionDenied(&'static str),
    /// A diff arrived before any snapshot: request a resync.
    #[error("no snapshot received yet")]
    NoSnapshot,
    /// A diff does not apply to the current revision: request a resync.
    #[error("revision mismatch: tree is at {expected}, diff is based on {got}")]
    RevisionMismatch { expected: u64, got: u64 },
    #[error("invalid diff: {0}")]
    InvalidDiff(String),
    /// The node id is unknown (stale palette entry or element destroyed).
    #[error("unknown node {0}")]
    UnknownNode(u64),
    #[error("no focused application")]
    NoFocusedApp,
    #[error("action {action:?} not supported by node {node}")]
    ActionUnsupported { node: u64, action: DomAction },
    /// Raw accessibility error (`AXError` on macOS).
    #[error("accessibility error {code}: {context}")]
    Accessibility { code: i32, context: &'static str },
    #[error("os error: {0}")]
    Os(String),
}

impl DomError {
    /// The receiver should drop its tree and send `DomMessage::Resync`.
    pub fn needs_resync(&self) -> bool {
        matches!(self, DomError::NoSnapshot | DomError::RevisionMismatch { .. } | DomError::InvalidDiff(_))
    }
}

pub type Result<T> = std::result::Result<T, DomError>;

/// Host-side accessibility tree source for the frontmost window.
pub trait DomProvider: Send {
    /// Snapshot of the focused application's menu bar + focused window.
    /// Revisions increase monotonically; node ids are stable across calls for
    /// the same UI element so consecutive snapshots diff small.
    fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot>;
    fn perform(&mut self, node: u64, action: DomAction) -> Result<()>;
    fn set_value(&mut self, node: u64, value: &str) -> Result<()>;
}

/// Create the provider for this OS.
///
/// * macOS: AXUIElement (needs the Accessibility permission; creation never
///   prompts, `snapshot` returns [`DomError::PermissionDenied`] until granted).
/// * Windows: UI Automation (`windows::UiaProvider`): the foreground
///   window's Control-view tree read through one `CacheRequest` per element,
///   `RuntimeId`-derived stable ids, actions through the Invoke / Toggle /
///   ExpandCollapse / SelectionItem / RangeValue / ScrollItem /
///   LegacyIAccessible patterns and values through ValuePattern.
/// * Linux: AT-SPI2 over D-Bus (`linux::AtspiProvider`, `atspi` + `zbus` on
///   a private worker thread): the active top-level window found through the
///   registry, BFS over `org.a11y.atspi.Accessible` with node/time budgets and
///   per-call timeouts, (bus name, object path)-hashed stable ids, actions via
///   the Action / Component / Value / Selection interfaces and values via
///   EditableText / Value. Fails with [`DomError::Os`] when the accessibility
///   bus (`at-spi-bus-launcher`) is unavailable.
/// * Android: lives in the app's AccessibilityService.
pub fn create_provider() -> Result<Box<dyn DomProvider>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::AxProvider::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::UiaProvider::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::AtspiProvider::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(DomError::Unsupported("no accessibility tree provider for this OS"))
    }
}
