//! Semantic UI-tree mirroring (the `Cmd+K` remote action palette).

use crate::Rect;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomRole {
    Window,
    Button,
    Link,
    TextField,
    TextArea,
    SearchField,
    CheckBox,
    RadioButton,
    Switch,
    Slider,
    ComboBox,
    PopUpButton,
    Menu,
    MenuItem,
    MenuBar,
    Tab,
    TabGroup,
    List,
    ListItem,
    Table,
    Row,
    Cell,
    Tree,
    TreeItem,
    Toolbar,
    Image,
    StaticText,
    Heading,
    Group,
    ScrollArea,
    WebArea,
    Dialog,
    Other,
}

impl DomRole {
    /// Whether the palette should list nodes of this role as actionable targets.
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            DomRole::Button
                | DomRole::Link
                | DomRole::TextField
                | DomRole::TextArea
                | DomRole::SearchField
                | DomRole::CheckBox
                | DomRole::RadioButton
                | DomRole::Switch
                | DomRole::Slider
                | DomRole::ComboBox
                | DomRole::PopUpButton
                | DomRole::MenuItem
                | DomRole::Tab
                | DomRole::ListItem
                | DomRole::TreeItem
                | DomRole::Row
        )
    }

    pub fn accepts_text(self) -> bool {
        matches!(self, DomRole::TextField | DomRole::TextArea | DomRole::SearchField | DomRole::ComboBox)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomAction {
    /// AXPress / UIA Invoke / AT-SPI "click" / ACTION_CLICK.
    Press,
    Focus,
    ShowMenu,
    Increment,
    Decrement,
    Expand,
    Collapse,
    Toggle,
    ScrollIntoView,
    Select,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomNode {
    /// Stable within a window for the life of the element.
    pub id: u64,
    pub parent: Option<u64>,
    pub role: DomRole,
    /// Platform role string (e.g. `AXButton`), for debugging / richer UIs.
    pub native_role: String,
    pub name: String,
    pub value: Option<String>,
    pub description: Option<String>,
    /// Keyboard shortcut if exposed (e.g. "⌘S").
    pub shortcut: Option<String>,
    pub bounds: Rect,
    pub actions: Vec<DomAction>,
    pub enabled: bool,
    pub focused: bool,
    pub children: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomSnapshot {
    pub revision: u64,
    pub app_name: String,
    pub window_title: String,
    pub pid: u32,
    pub root: u64,
    pub nodes: Vec<DomNode>,
}

/// Incremental update from `base_revision` to `revision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomTreeDiff {
    pub base_revision: u64,
    pub revision: u64,
    /// New or changed nodes (full replacement by id).
    pub upserted: Vec<DomNode>,
    pub removed: Vec<u64>,
    pub root: Option<u64>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DomMessage {
    /// Client → host: start streaming the focused window's tree.
    Subscribe { max_depth: u16, interactive_only: bool },
    Unsubscribe,
    Snapshot(DomSnapshot),
    Diff(DomTreeDiff),
    /// Client asks for a fresh snapshot (e.g. after a revision gap).
    Resync,
    PerformAction { request_id: u64, node_id: u64, action: DomAction },
    SetValue { request_id: u64, node_id: u64, value: String },
    ActionResult { request_id: u64, ok: bool, error: Option<String> },
}
