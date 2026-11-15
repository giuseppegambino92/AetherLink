//! Pure UI Automation mapping tables used by the Windows provider ([`crate::windows`]).
//!
//! Kept free of any Windows API so the mapping is unit-tested on every OS. Control type ids
//! are the numeric `UIA_*ControlTypeId` constants from `UIAutomationClient.h` (a compile-time
//! test on Windows checks them against the `windows` crate).

use aether_protocol::{DomAction, DomRole};

/// `UIA_*ControlTypeId` values.
pub mod control_type {
    pub const BUTTON: i32 = 50000;
    pub const CALENDAR: i32 = 50001;
    pub const CHECK_BOX: i32 = 50002;
    pub const COMBO_BOX: i32 = 50003;
    pub const EDIT: i32 = 50004;
    pub const HYPERLINK: i32 = 50005;
    pub const IMAGE: i32 = 50006;
    pub const LIST_ITEM: i32 = 50007;
    pub const LIST: i32 = 50008;
    pub const MENU: i32 = 50009;
    pub const MENU_BAR: i32 = 50010;
    pub const MENU_ITEM: i32 = 50011;
    pub const PROGRESS_BAR: i32 = 50012;
    pub const RADIO_BUTTON: i32 = 50013;
    pub const SCROLL_BAR: i32 = 50014;
    pub const SLIDER: i32 = 50015;
    pub const SPINNER: i32 = 50016;
    pub const STATUS_BAR: i32 = 50017;
    pub const TAB: i32 = 50018;
    pub const TAB_ITEM: i32 = 50019;
    pub const TEXT: i32 = 50020;
    pub const TOOL_BAR: i32 = 50021;
    pub const TOOL_TIP: i32 = 50022;
    pub const TREE: i32 = 50023;
    pub const TREE_ITEM: i32 = 50024;
    pub const CUSTOM: i32 = 50025;
    pub const GROUP: i32 = 50026;
    pub const THUMB: i32 = 50027;
    pub const DATA_GRID: i32 = 50028;
    pub const DATA_ITEM: i32 = 50029;
    pub const DOCUMENT: i32 = 50030;
    pub const SPLIT_BUTTON: i32 = 50031;
    pub const WINDOW: i32 = 50032;
    pub const PANE: i32 = 50033;
    pub const HEADER: i32 = 50034;
    pub const HEADER_ITEM: i32 = 50035;
    pub const TABLE: i32 = 50036;
    pub const TITLE_BAR: i32 = 50037;
    pub const SEPARATOR: i32 = 50038;
    pub const SEMANTIC_ZOOM: i32 = 50039;
    pub const APP_BAR: i32 = 50040;
}

/// Base [`DomRole`] for a UIA control type (before [`refine_role`]).
pub fn role_for_control_type(id: i32) -> DomRole {
    use control_type::*;
    match id {
        BUTTON => DomRole::Button,
        CHECK_BOX => DomRole::CheckBox,
        COMBO_BOX => DomRole::ComboBox,
        EDIT => DomRole::TextField,
        HYPERLINK => DomRole::Link,
        IMAGE => DomRole::Image,
        LIST_ITEM => DomRole::ListItem,
        LIST => DomRole::List,
        MENU => DomRole::Menu,
        MENU_BAR => DomRole::MenuBar,
        MENU_ITEM => DomRole::MenuItem,
        RADIO_BUTTON => DomRole::RadioButton,
        SLIDER | SPINNER => DomRole::Slider,
        TAB => DomRole::TabGroup,
        TAB_ITEM => DomRole::Tab,
        TEXT => DomRole::StaticText,
        TOOL_BAR | APP_BAR => DomRole::Toolbar,
        TREE => DomRole::Tree,
        TREE_ITEM => DomRole::TreeItem,
        DATA_GRID | TABLE => DomRole::Table,
        DATA_ITEM => DomRole::Row,
        // A Document is a text editor only when it exposes a writable Value (see refine_role);
        // browsers expose web pages as Document.
        DOCUMENT => DomRole::WebArea,
        SPLIT_BUTTON => DomRole::PopUpButton,
        WINDOW => DomRole::Window,
        // Column headers are clickable (sort) buttons.
        HEADER_ITEM => DomRole::Button,
        GROUP | PANE | HEADER | CALENDAR | STATUS_BAR | SEMANTIC_ZOOM => DomRole::Group,
        _ => DomRole::Other, // Custom, ProgressBar, ScrollBar, Thumb, ToolTip, TitleBar, Separator
    }
}

/// Programmatic name of a UIA control type (used as `DomNode::native_role`).
pub fn control_type_name(id: i32) -> &'static str {
    use control_type::*;
    match id {
        BUTTON => "Button",
        CALENDAR => "Calendar",
        CHECK_BOX => "CheckBox",
        COMBO_BOX => "ComboBox",
        EDIT => "Edit",
        HYPERLINK => "Hyperlink",
        IMAGE => "Image",
        LIST_ITEM => "ListItem",
        LIST => "List",
        MENU => "Menu",
        MENU_BAR => "MenuBar",
        MENU_ITEM => "MenuItem",
        PROGRESS_BAR => "ProgressBar",
        RADIO_BUTTON => "RadioButton",
        SCROLL_BAR => "ScrollBar",
        SLIDER => "Slider",
        SPINNER => "Spinner",
        STATUS_BAR => "StatusBar",
        TAB => "Tab",
        TAB_ITEM => "TabItem",
        TEXT => "Text",
        TOOL_BAR => "ToolBar",
        TOOL_TIP => "ToolTip",
        TREE => "Tree",
        TREE_ITEM => "TreeItem",
        CUSTOM => "Custom",
        GROUP => "Group",
        THUMB => "Thumb",
        DATA_GRID => "DataGrid",
        DATA_ITEM => "DataItem",
        DOCUMENT => "Document",
        SPLIT_BUTTON => "SplitButton",
        WINDOW => "Window",
        PANE => "Pane",
        HEADER => "Header",
        HEADER_ITEM => "HeaderItem",
        TABLE => "Table",
        TITLE_BAR => "TitleBar",
        SEPARATOR => "Separator",
        SEMANTIC_ZOOM => "SemanticZoom",
        APP_BAR => "AppBar",
        _ => "Unknown",
    }
}

/// Control patterns (and pattern-derived state) an element supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Patterns {
    pub invoke: bool,
    pub toggle: bool,
    pub expand_collapse: bool,
    pub selection_item: bool,
    pub legacy: bool,
    /// LegacyIAccessible exposes a non-empty default action.
    pub legacy_default_action: bool,
    pub range_value: bool,
    pub scroll_item: bool,
    pub value: bool,
    pub value_read_only: bool,
    pub keyboard_focusable: bool,
}

/// Inputs to [`refine_role`] beyond the control type.
#[derive(Debug, Clone, Copy, Default)]
pub struct RoleHints<'a> {
    pub is_dialog: bool,
    /// `UIA_HeadingLevelPropertyId` is set (not `HeadingLevel_None`).
    pub is_heading: bool,
    pub automation_id: &'a str,
    pub class_name: &'a str,
    pub localized_type: &'a str,
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

/// Final role: control type + patterns + hints (dialogs, headings, toggle buttons,
/// editable documents, search boxes, menu buttons).
pub fn refine_role(control_type: i32, p: &Patterns, h: &RoleHints<'_>) -> DomRole {
    use control_type::*;
    let base = role_for_control_type(control_type);
    match control_type {
        WINDOW if h.is_dialog || h.class_name == "#32770" => DomRole::Dialog,
        TEXT if h.is_heading => DomRole::Heading,
        BUTTON if p.toggle && !p.invoke => DomRole::Switch,
        // Menu buttons (drop-down without a direct action) behave like pop-up buttons.
        BUTTON if p.expand_collapse && !p.invoke => DomRole::PopUpButton,
        DOCUMENT if p.value && !p.value_read_only => DomRole::TextArea,
        EDIT if contains_ci(h.automation_id, "search")
            || contains_ci(h.class_name, "search")
            || contains_ci(h.localized_type, "search") =>
        {
            DomRole::SearchField
        }
        _ => base,
    }
}

/// Palette actions offered for an element.
pub fn actions_for(role: DomRole, p: &Patterns, enabled: bool) -> Vec<DomAction> {
    let mut out = Vec::new();
    if !enabled {
        return out;
    }
    if p.invoke || (p.legacy_default_action && !p.toggle && !p.selection_item) {
        out.push(DomAction::Press);
    }
    if p.toggle {
        out.push(DomAction::Toggle);
    }
    if p.selection_item {
        out.push(DomAction::Select);
    }
    if p.expand_collapse {
        out.push(DomAction::Expand);
        out.push(DomAction::Collapse);
        if matches!(role, DomRole::PopUpButton | DomRole::ComboBox | DomRole::MenuItem) {
            out.push(DomAction::ShowMenu);
        }
    }
    if p.range_value && !p.value_read_only {
        out.push(DomAction::Increment);
        out.push(DomAction::Decrement);
    }
    if p.scroll_item {
        out.push(DomAction::ScrollIntoView);
    }
    if p.keyboard_focusable || role.accepts_text() || role == DomRole::Window {
        out.push(DomAction::Focus);
    }
    out
}

/// Stable 64-bit id from a UIA `RuntimeId` (FNV-1a over the little-endian `i32`s), never 0.
pub fn runtime_id_hash(runtime_id: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in runtime_id {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    // Ids are JSON-serialised by the desktop bridge as strings, but keep them within
    // 2^53 anyway so naive JS consumers stay exact.
    let h = h & ((1u64 << 53) - 1);
    if h == 0 { 1 } else { h }
}

/// Truncate to `max` characters (char-boundary safe), adding `…` when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_type::*;

    #[test]
    fn base_roles() {
        assert_eq!(role_for_control_type(BUTTON), DomRole::Button);
        assert_eq!(role_for_control_type(HYPERLINK), DomRole::Link);
        assert_eq!(role_for_control_type(EDIT), DomRole::TextField);
        assert_eq!(role_for_control_type(TAB), DomRole::TabGroup);
        assert_eq!(role_for_control_type(TAB_ITEM), DomRole::Tab);
        assert_eq!(role_for_control_type(DATA_ITEM), DomRole::Row);
        assert_eq!(role_for_control_type(DATA_GRID), DomRole::Table);
        assert_eq!(role_for_control_type(SPLIT_BUTTON), DomRole::PopUpButton);
        assert_eq!(role_for_control_type(PANE), DomRole::Group);
        assert_eq!(role_for_control_type(TEXT), DomRole::StaticText);
        assert_eq!(role_for_control_type(CUSTOM), DomRole::Other);
        assert_eq!(role_for_control_type(12345), DomRole::Other);
        for id in BUTTON..=APP_BAR {
            assert_ne!(control_type_name(id), "Unknown", "{id}");
        }
    }

    #[test]
    fn refined_roles() {
        let none = Patterns::default();
        let h = RoleHints::default();
        assert_eq!(refine_role(WINDOW, &none, &RoleHints { is_dialog: true, ..h }), DomRole::Dialog);
        assert_eq!(refine_role(WINDOW, &none, &RoleHints { class_name: "#32770", ..h }), DomRole::Dialog);
        assert_eq!(refine_role(WINDOW, &none, &h), DomRole::Window);
        assert_eq!(refine_role(TEXT, &none, &RoleHints { is_heading: true, ..h }), DomRole::Heading);
        let toggle = Patterns { toggle: true, ..none };
        assert_eq!(refine_role(BUTTON, &toggle, &h), DomRole::Switch);
        let menu_button = Patterns { expand_collapse: true, ..none };
        assert_eq!(refine_role(BUTTON, &menu_button, &h), DomRole::PopUpButton);
        let both = Patterns { invoke: true, toggle: true, ..none };
        assert_eq!(refine_role(BUTTON, &both, &h), DomRole::Button);
        let editable = Patterns { value: true, ..none };
        assert_eq!(refine_role(DOCUMENT, &editable, &h), DomRole::TextArea);
        let ro = Patterns { value: true, value_read_only: true, ..none };
        assert_eq!(refine_role(DOCUMENT, &ro, &h), DomRole::WebArea);
        assert_eq!(refine_role(EDIT, &none, &RoleHints { automation_id: "SearchTextBox", ..h }), DomRole::SearchField);
        assert_eq!(refine_role(EDIT, &none, &h), DomRole::TextField);
    }

    #[test]
    fn actions() {
        let p = Patterns { invoke: true, keyboard_focusable: true, ..Default::default() };
        assert_eq!(actions_for(DomRole::Button, &p, true), vec![DomAction::Press, DomAction::Focus]);
        assert!(actions_for(DomRole::Button, &p, false).is_empty());
        let cb = Patterns { toggle: true, legacy_default_action: true, ..Default::default() };
        assert_eq!(actions_for(DomRole::CheckBox, &cb, true), vec![DomAction::Toggle]);
        let combo = Patterns { expand_collapse: true, value: true, ..Default::default() };
        assert_eq!(
            actions_for(DomRole::ComboBox, &combo, true),
            vec![DomAction::Expand, DomAction::Collapse, DomAction::ShowMenu, DomAction::Focus]
        );
        let slider = Patterns { range_value: true, ..Default::default() };
        assert_eq!(actions_for(DomRole::Slider, &slider, true), vec![DomAction::Increment, DomAction::Decrement]);
        let ro_slider = Patterns { range_value: true, value_read_only: true, ..Default::default() };
        assert!(actions_for(DomRole::Slider, &ro_slider, true).is_empty());
        let legacy = Patterns { legacy_default_action: true, scroll_item: true, ..Default::default() };
        assert_eq!(
            actions_for(DomRole::ListItem, &legacy, true),
            vec![DomAction::Press, DomAction::ScrollIntoView]
        );
        let item = Patterns { selection_item: true, legacy_default_action: true, ..Default::default() };
        assert_eq!(actions_for(DomRole::ListItem, &item, true), vec![DomAction::Select]);
    }

    #[test]
    fn runtime_ids() {
        let a = runtime_id_hash(&[42, 0x1234, 7]);
        assert_eq!(a, runtime_id_hash(&[42, 0x1234, 7]));
        assert_ne!(a, runtime_id_hash(&[42, 0x1234, 8]));
        assert_ne!(runtime_id_hash(&[1, 2]), runtime_id_hash(&[2, 1]));
        assert!(a < (1 << 53) && a != 0);
        assert_ne!(runtime_id_hash(&[]), 0);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("perché", 5), "perch…");
        assert_eq!(truncate_chars("", 0), "");
    }
}
