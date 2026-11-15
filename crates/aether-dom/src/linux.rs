//! Linux accessibility provider (AT-SPI2 over D-Bus, `atspi` + `zbus`).
//!
//! * Bus: the accessibility bus (`$AT_SPI_BUS_ADDRESS`, else
//!   `org.a11y.Bus.GetAddress` on the session bus — launched by
//!   `at-spi-bus-launcher`). Every method call carries a timeout
//!   ([`METHOD_TIMEOUT`]) so a hung application cannot stall the host.
//! * Threading: the provider API is synchronous while zbus is async, so a
//!   dedicated worker thread owns the connection and drives the futures with
//!   `zbus::block_on` (zbus's own `async-io` backend — no tokio runtime is
//!   created, and nothing runs on the caller's thread); the provider talks to
//!   it over channels.
//! * Target: registry root → applications → their top-level windows; the
//!   window with `State::Active` wins, else a focused one, else a showing
//!   (modal first) window of the most recently registered application. The
//!   snapshot is that window under a synthetic application root (GTK/Qt menu
//!   bars live inside the frame).
//! * Traversal: breadth-first, depth-limited, node budget
//!   ([`DEFAULT_NODE_BUDGET`]) and wall-clock budget; nodes of a BFS batch are
//!   fetched concurrently (pipelined D-Bus calls). Non-showing subtrees are
//!   skipped except inside menus (closed menus stay listed for the palette).
//! * Stable ids: a deterministic 53-bit hash of (bus name, object path)
//!   ([`node_id`]), so ids survive snapshots (and even pruning); the id map
//!   keeps the D-Bus address of recently seen nodes for `perform`/`set_value`.
//! * Bounds: `Component.GetExtents(ATSPI_COORD_TYPE_SCREEN)` (screen pixels;
//!   meaningless under Wayland, where toolkits report window-relative values).
//! * Password text never exposes its value.

use crate::{derive_container_names, prune_to_interactive, DomError, DomProvider, Result};
use aether_protocol::{DomAction, DomNode, DomRole, DomSnapshot, Rect};
use atspi::{Role, State, StateSet};
use futures::future::join_all;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type, Value};
use zbus::Connection;

pub const DEFAULT_NODE_BUDGET: usize = 5000;
pub const DEFAULT_TIME_BUDGET: Duration = Duration::from_millis(1500);
/// Per-call D-Bus timeout.
pub const METHOD_TIMEOUT: Duration = Duration::from_millis(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VALUE_CHARS: usize = 256;
/// Snapshots an unseen node's D-Bus address is remembered for.
const ID_RETENTION_GENERATIONS: u64 = 8;
/// Nodes fetched concurrently per BFS batch.
const BATCH: usize = 32;
/// Ids fit in 53 bits so they survive a round-trip through JavaScript numbers.
const ID_MASK: u64 = (1 << 53) - 1;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const NULL_PATH: &str = "/org/a11y/atspi/null";
const I_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const I_ACTION: &str = "org.a11y.atspi.Action";
const I_COMPONENT: &str = "org.a11y.atspi.Component";
const I_TEXT: &str = "org.a11y.atspi.Text";
const I_EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
const I_VALUE: &str = "org.a11y.atspi.Value";
const I_SELECTION: &str = "org.a11y.atspi.Selection";
const I_PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// `ATSPI_RELATION_LABELLED_BY`.
const RELATION_LABELLED_BY: u32 = 2;

// Interface bit mask (subset we care about).
const IF_ACTION: u32 = 1;
const IF_COMPONENT: u32 = 2;
const IF_TEXT: u32 = 4;
const IF_EDITABLE_TEXT: u32 = 8;
const IF_VALUE: u32 = 16;
const IF_SELECTION: u32 = 32;

// ---------------------------------------------------------------------------
// Pure mappings
// ---------------------------------------------------------------------------

/// Bit mask of the interfaces in a `GetInterfaces` reply.
pub fn interface_mask(names: &[String]) -> u32 {
    names.iter().fold(0, |m, n| {
        m | match n.as_str() {
            I_ACTION => IF_ACTION,
            I_COMPONENT => IF_COMPONENT,
            I_TEXT => IF_TEXT,
            I_EDITABLE_TEXT => IF_EDITABLE_TEXT,
            I_VALUE => IF_VALUE,
            I_SELECTION => IF_SELECTION,
            _ => 0,
        }
    })
}

/// 64-bit state bits from a `GetState` reply (`au`, two words).
pub fn state_bits(words: &[u32]) -> u64 {
    u64::from(words.first().copied().unwrap_or(0)) | (u64::from(words.get(1).copied().unwrap_or(0)) << 32)
}

pub fn has_state(bits: u64, s: State) -> bool {
    bits & StateSet::new(s).bits() != 0
}

/// Map an AT-SPI role (+ the `MultiLine` state for plain `text`) to the portable role.
pub fn map_role(role: Role, multi_line: bool) -> DomRole {
    use Role as R;
    match role {
        R::Frame | R::Window | R::DesktopFrame | R::InternalFrame => DomRole::Window,
        R::Dialog | R::Alert | R::FileChooser | R::ColorChooser | R::FontChooser => DomRole::Dialog,
        R::Button => DomRole::Button,
        R::PushButtonMenu => DomRole::PopUpButton,
        R::ToggleButton => DomRole::Switch,
        R::CheckBox => DomRole::CheckBox,
        R::RadioButton => DomRole::RadioButton,
        R::Link => DomRole::Link,
        R::Text if multi_line => DomRole::TextArea,
        R::Text | R::Entry | R::PasswordText | R::Editbar => DomRole::TextField,
        R::Terminal | R::DocumentText => DomRole::TextArea,
        R::ComboBox | R::Autocomplete => DomRole::ComboBox,
        R::Slider | R::SpinButton | R::Dial => DomRole::Slider,
        R::MenuBar => DomRole::MenuBar,
        R::Menu | R::PopupMenu => DomRole::Menu,
        R::MenuItem | R::CheckMenuItem | R::RadioMenuItem | R::TearoffMenuItem => DomRole::MenuItem,
        R::PageTab => DomRole::Tab,
        R::PageTabList => DomRole::TabGroup,
        R::List | R::ListBox | R::DescriptionList => DomRole::List,
        R::ListItem => DomRole::ListItem,
        R::Table => DomRole::Table,
        R::Tree | R::TreeTable => DomRole::Tree,
        R::TableRow => DomRole::Row,
        R::TableCell | R::ColumnHeader | R::RowHeader | R::TableColumnHeader | R::TableRowHeader => DomRole::Cell,
        R::TreeItem => DomRole::TreeItem,
        R::ToolBar => DomRole::Toolbar,
        R::Image | R::Icon | R::DesktopIcon | R::Animation | R::ImageMap => DomRole::Image,
        R::Label | R::Static | R::Caption | R::AcceleratorLabel | R::Paragraph | R::StatusBar => DomRole::StaticText,
        R::Heading => DomRole::Heading,
        R::ScrollPane => DomRole::ScrollArea,
        R::DocumentWeb | R::HTMLContainer => DomRole::WebArea,
        R::Panel
        | R::Filler
        | R::Grouping
        | R::Section
        | R::Form
        | R::Viewport
        | R::RootPane
        | R::LayeredPane
        | R::GlassPane
        | R::SplitPane
        | R::OptionPane
        | R::Canvas
        | R::Embedded
        | R::Header
        | R::Footer
        | R::Article
        | R::Landmark
        | R::BlockQuote
        | R::Application
        | R::Page
        | R::DocumentFrame
        | R::DocumentEmail
        | R::DocumentSpreadsheet
        | R::DocumentPresentation
        | R::InfoBar
        | R::Notification
        | R::Log
        | R::Calendar
        | R::DirectoryPane
        | R::DateEditor
        | R::Comment
        | R::Footnote
        | R::Definition => DomRole::Group,
        _ => DomRole::Other,
    }
}

/// [`map_role`] on the raw `GetRole` value; unknown roles → `Other`.
pub fn map_role_raw(raw: u32, multi_line: bool) -> DomRole {
    Role::try_from(raw).map_or(DomRole::Other, |r| map_role(r, multi_line))
}

/// AT-SPI role name ("push button") for `native_role`.
pub fn role_name(raw: u32) -> String {
    Role::try_from(raw).map_or_else(|_| format!("role {raw}"), |r| r.name().to_string())
}

fn norm_action(n: &str) -> String {
    n.trim().to_lowercase().replace(['_', '-'], " ")
}

const PRESS_NAMES: &[&str] = &["click", "press", "activate", "jump", "open", "invoke", "push"];

/// Context for [`map_actions`] beyond the Action interface's names.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionContext {
    /// Component interface present and `State::Focusable`.
    pub focusable: bool,
    /// `State::Expandable` / `State::Expanded`.
    pub expandable: bool,
    pub expanded: bool,
    /// Value interface on an adjustable role (slider, spin button, scroll bar, dial).
    pub adjustable: bool,
    /// The parent implements the Selection interface.
    pub parent_selection: bool,
    /// Component present and inside a scroll pane.
    pub scrollable: bool,
}

/// Map AT-SPI action names (`Action.GetName(i)`) to portable actions.
pub fn map_actions(names: &[String], role: DomRole, cx: ActionContext) -> Vec<DomAction> {
    let mut out = Vec::new();
    let mut push = |a: DomAction| {
        if !out.contains(&a) {
            out.push(a);
        }
    };
    let mut pressable = false;
    for n in names {
        let n = norm_action(n);
        match n.as_str() {
            n if PRESS_NAMES.contains(&n) => {
                pressable = true;
                push(DomAction::Press);
                if matches!(role, DomRole::CheckBox | DomRole::Switch | DomRole::RadioButton) {
                    push(DomAction::Toggle);
                }
            }
            "toggle" => push(DomAction::Toggle),
            "expand or contract" | "expand or collapse" => {
                push(if cx.expanded { DomAction::Collapse } else { DomAction::Expand })
            }
            "expand" => push(DomAction::Expand),
            "collapse" | "contract" => push(DomAction::Collapse),
            "showmenu" | "show menu" | "menu" | "popup" | "open menu" => push(DomAction::ShowMenu),
            "increment" | "increase" => push(DomAction::Increment),
            "decrement" | "decrease" => push(DomAction::Decrement),
            "select" => push(DomAction::Select),
            _ => {}
        }
    }
    if cx.expandable && pressable {
        push(if cx.expanded { DomAction::Collapse } else { DomAction::Expand });
    }
    if cx.focusable {
        push(DomAction::Focus);
    }
    if cx.adjustable {
        push(DomAction::Increment);
        push(DomAction::Decrement);
    }
    if cx.parent_selection
        && matches!(role, DomRole::Row | DomRole::TreeItem | DomRole::ListItem | DomRole::Tab | DomRole::Cell)
    {
        push(DomAction::Select);
    }
    if cx.scrollable && role.is_interactive() {
        push(DomAction::ScrollIntoView);
    }
    out
}

/// Index of the AT-SPI action implementing `action`, by name preference.
pub fn action_index(names: &[String], action: DomAction) -> Option<usize> {
    let want: &[&str] = match action {
        DomAction::Press => PRESS_NAMES,
        DomAction::Toggle => &["toggle", "click", "press", "activate"],
        DomAction::Expand => &["expand", "expand or contract", "expand or collapse", "activate", "click", "open"],
        DomAction::Collapse => &["collapse", "contract", "expand or contract", "expand or collapse", "activate", "click"],
        DomAction::ShowMenu => &["showmenu", "show menu", "menu", "popup", "open menu"],
        DomAction::Increment => &["increment", "increase"],
        DomAction::Decrement => &["decrement", "decrease"],
        DomAction::Select => &["select", "activate", "click", "press"],
        DomAction::Focus | DomAction::ScrollIntoView => &[],
    };
    let normed: Vec<String> = names.iter().map(|n| norm_action(n)).collect();
    want.iter().find_map(|w| normed.iter().position(|n| n == w))
}

/// Human-readable shortcut from an AT-SPI key binding. GTK reports
/// `"mnemonic;sequence;accelerator"` (`"<Alt>f;;<Control>o"`); the
/// accelerator wins, a modifier-bearing mnemonic is the fallback.
/// `<Control><Shift>s` → `Ctrl+Shift+S`.
pub fn format_keybinding(kb: &str) -> Option<String> {
    let parts: Vec<&str> = kb.split(';').map(str::trim).collect();
    let pick = parts
        .get(2)
        .filter(|s| !s.is_empty())
        .or_else(|| parts.iter().find(|s| s.starts_with('<') || s.contains('+')))?;
    let mut mods: Vec<&str> = Vec::new();
    let mut rest = *pick;
    while let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?;
        let m = match stripped[..end].to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" | "ctl" => "Ctrl",
            "shift" => "Shift",
            "alt" | "mod1" | "meta" => "Alt",
            "super" | "mod4" | "hyper" => "Super",
            _ => "",
        };
        if !m.is_empty() && !mods.contains(&m) {
            mods.push(m);
        }
        rest = &stripped[end + 1..];
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    // Already "Ctrl+S"-style (Qt / GTK4)?
    if mods.is_empty() && rest.contains('+') && rest.len() > 1 {
        return Some(rest.to_string());
    }
    let key = match rest {
        "Return" | "KP_Enter" => "Enter".to_string(),
        "Escape" => "Esc".to_string(),
        "Delete" | "KP_Delete" => "Del".to_string(),
        "BackSpace" => "Backspace".to_string(),
        "space" => "Space".to_string(),
        "Page_Up" => "PgUp".to_string(),
        "Page_Down" => "PgDn".to_string(),
        "plus" => "+".to_string(),
        "minus" => "-".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        k if k.chars().count() == 1 => k.to_uppercase(),
        k => k.to_string(),
    };
    mods.push(&key);
    Some(mods.join("+"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn format_number(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Window choice score (0 = not a candidate): active ≫ focused ≫ showing
/// (modal dialogs first). Desktop-shell applications only count when active.
pub fn window_score(states: u64, role: u32, app_name: &str) -> u32 {
    const SHELLS: &[&str] = &["gnome-shell", "plasmashell", "mutter-x11-frames", "xfce4-panel", "xfdesktop", "budgie-panel"];
    if has_state(states, State::Defunct) {
        return 0;
    }
    if has_state(states, State::Active) {
        return 100;
    }
    if SHELLS.iter().any(|s| app_name.eq_ignore_ascii_case(s)) {
        return 0;
    }
    if has_state(states, State::Focused) {
        return 80;
    }
    if !has_state(states, State::Showing) || has_state(states, State::Iconified) {
        return 0;
    }
    let windowish = matches!(map_role_raw(role, false), DomRole::Window | DomRole::Dialog);
    match (has_state(states, State::Modal), windowish) {
        (true, _) => 30,
        (false, true) => 20,
        (false, false) => 10,
    }
}

// ---------------------------------------------------------------------------
// Stable ids
// ---------------------------------------------------------------------------

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash_key(bus: &str, path: &str, salt: u64) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in bus.bytes().chain([0xff]).chain(path.bytes()).chain(salt.to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    match splitmix64(h) & ID_MASK {
        0 => 1,
        id => id,
    }
}

/// Deterministic, non-zero, 53-bit id of the accessible at (bus name, object path).
pub fn node_id(bus: &str, path: &str) -> u64 {
    hash_key(bus, path, 0)
}

/// D-Bus address of an accessible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Obj {
    pub bus: String,
    pub path: String,
}

impl Obj {
    pub fn new(bus: impl Into<String>, path: impl Into<String>) -> Self {
        Self { bus: bus.into(), path: path.into() }
    }

    fn from_ref(r: &(String, OwnedObjectPath)) -> Option<Self> {
        (!r.0.is_empty() && r.1.as_str() != NULL_PATH).then(|| Obj::new(r.0.clone(), r.1.as_str()))
    }
}

struct Entry {
    obj: Obj,
    parent: Option<u64>,
    last_seen: u64,
}

/// id ↔ (bus name, object path) with retention generations. Ids are
/// [`node_id`] hashes (re-salted on the astronomically unlikely collision).
#[derive(Default)]
pub struct IdMap {
    by_key: HashMap<Obj, u64>,
    by_id: HashMap<u64, Entry>,
    generation: u64,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id_for(&mut self, obj: &Obj, parent: Option<u64>) -> u64 {
        let gen_ = self.generation;
        if let Some(&id) = self.by_key.get(obj) {
            if let Some(e) = self.by_id.get_mut(&id) {
                e.last_seen = gen_;
                e.parent = parent;
            }
            return id;
        }
        let mut salt = 0;
        let id = loop {
            let id = hash_key(&obj.bus, &obj.path, salt);
            if !self.by_id.contains_key(&id) {
                break id;
            }
            salt += 1;
        };
        self.by_key.insert(obj.clone(), id);
        self.by_id.insert(id, Entry { obj: obj.clone(), parent, last_seen: gen_ });
        id
    }

    pub fn lookup(&self, id: u64) -> Option<(&Obj, Option<u64>)> {
        self.by_id.get(&id).map(|e| (&e.obj, e.parent))
    }

    pub fn begin(&mut self) {
        self.generation += 1;
    }

    /// Forget nodes not seen in the last `keep` generations.
    pub fn prune(&mut self, keep: u64) {
        let min = self.generation.saturating_sub(keep);
        let by_key = &mut self.by_key;
        self.by_id.retain(|_, e| {
            let keep = e.last_seen >= min;
            if !keep {
                by_key.remove(&e.obj);
            }
            keep
        });
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// D-Bus helpers
// ---------------------------------------------------------------------------

type ObjRef = (String, OwnedObjectPath);

async fn call<B, R>(conn: &Connection, obj: &Obj, iface: &str, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::Serialize + DynamicType,
    R: DeserializeOwned + Type,
{
    let msg = conn.call_method(Some(obj.bus.as_str()), obj.path.as_str(), Some(iface), method, body).await?;
    msg.body().deserialize::<R>()
}

async fn get_prop(conn: &Connection, obj: &Obj, iface: &str, name: &str) -> zbus::Result<OwnedValue> {
    call(conn, obj, I_PROPERTIES, "Get", &(iface, name)).await
}

async fn set_prop(conn: &Connection, obj: &Obj, iface: &str, name: &str, v: Value<'_>) -> zbus::Result<()> {
    conn.call_method(Some(obj.bus.as_str()), obj.path.as_str(), Some(I_PROPERTIES), "Set", &(iface, name, v))
        .await
        .map(|_| ())
}

fn value_string(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::Value(b) => value_string(b),
        _ => None,
    }
}

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::F64(f) => Some(*f),
        Value::I32(i) => Some(f64::from(*i)),
        Value::U32(i) => Some(f64::from(*i)),
        Value::I16(i) => Some(f64::from(*i)),
        Value::U16(i) => Some(f64::from(*i)),
        Value::U8(i) => Some(f64::from(*i)),
        Value::I64(i) => Some(*i as f64),
        Value::U64(i) => Some(*i as f64),
        Value::Value(b) => value_f64(b),
        _ => None,
    }
}

fn prop_string(r: zbus::Result<OwnedValue>) -> String {
    r.ok().and_then(|v| value_string(&v)).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The remote object is gone (or never existed).
fn is_stale(e: &zbus::Error) -> bool {
    const STALE: &[&str] = &[
        "org.freedesktop.DBus.Error.UnknownObject",
        "org.freedesktop.DBus.Error.UnknownMethod",
        "org.freedesktop.DBus.Error.UnknownInterface",
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => STALE.contains(&name.as_str()),
        zbus::Error::FDO(f) => matches!(
            **f,
            zbus::fdo::Error::UnknownObject(_)
                | zbus::fdo::Error::UnknownMethod(_)
                | zbus::fdo::Error::UnknownInterface(_)
                | zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::NameHasNoOwner(_)
        ),
        _ => false,
    }
}

/// The connection itself is broken (reconnect on the next request).
fn is_transport(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::InputOutput(_) | zbus::Error::Connection(..))
}

fn unavailable(detail: impl std::fmt::Display) -> DomError {
    DomError::Os(format!(
        "AT-SPI accessibility bus unavailable ({detail}); is at-spi-bus-launcher (at-spi2-core) running \
         and toolkit accessibility enabled (gsettings set org.gnome.desktop.interface toolkit-accessibility true)?"
    ))
}

/// Connect to the accessibility bus (per-call timeouts applied).
async fn connect() -> Result<Connection> {
    let addr = match std::env::var("AT_SPI_BUS_ADDRESS") {
        Ok(a) if !a.trim().is_empty() => a,
        _ => {
            let session = Connection::session().await.map_err(|e| unavailable(format!("session bus: {e}")))?;
            let bus = atspi::proxy::bus::BusProxy::new(&session).await.map_err(|e| unavailable(format!("org.a11y.Bus: {e}")))?;
            bus.get_address().await.map_err(|e| unavailable(format!("org.a11y.Bus.GetAddress: {e}")))?
        }
    };
    zbus::connection::Builder::address(addr.as_str())
        .map_err(|e| unavailable(format!("bad bus address {addr:?}: {e}")))?
        .method_timeout(METHOD_TIMEOUT)
        .build()
        .await
        .map_err(|e| unavailable(format!("connect {addr}: {e}")))
}

/// Runtime-agnostic timeout (`async-io` timer).
async fn with_timeout<F: std::future::Future>(d: Duration, f: F) -> Option<F::Output> {
    use futures::future::{select, Either};
    match select(std::pin::pin!(f), async_io::Timer::after(d)).await {
        Either::Left((v, _)) => Some(v),
        Either::Right(_) => None,
    }
}

async fn connect_with_timeout() -> Result<Connection> {
    with_timeout(CONNECT_TIMEOUT, connect()).await.ok_or_else(|| unavailable("connection timed out"))?
}

async fn pid_of(conn: &Connection, bus: &str) -> zbus::Result<u32> {
    let dbus = Obj::new("org.freedesktop.DBus", "/org/freedesktop/DBus");
    call(conn, &dbus, "org.freedesktop.DBus", "GetConnectionUnixProcessID", &bus).await
}

// ---------------------------------------------------------------------------
// Target selection
// ---------------------------------------------------------------------------

/// The window a snapshot is taken of.
#[derive(Debug, Clone)]
pub struct Target {
    pub app: Obj,
    pub app_name: String,
    pub window: Obj,
    pub window_title: String,
    pub pid: u32,
}

/// Find the focused top-level window (see module docs). `app_filter`
/// restricts candidates to applications whose name contains it
/// (case-insensitive).
async fn find_target(conn: &Connection, app_filter: Option<&str>) -> Result<Target> {
    let root = Obj::new(REGISTRY, ROOT_PATH);
    let apps: Vec<ObjRef> =
        call(conn, &root, I_ACCESSIBLE, "GetChildren", &()).await.map_err(|e| DomError::Os(format!("AT-SPI registry: {e}")))?;
    let own = std::process::id();
    let filter = app_filter.map(str::to_lowercase);
    let filter = filter.as_deref();
    let per_app = apps.iter().filter_map(Obj::from_ref).enumerate().map(|(order, app)| async move {
        let (name, pid, wins) = futures::join!(
            get_prop(conn, &app, I_ACCESSIBLE, "Name"),
            pid_of(conn, &app.bus),
            call::<_, Vec<ObjRef>>(conn, &app, I_ACCESSIBLE, "GetChildren", &()),
        );
        let name = prop_string(name);
        if filter.is_some_and(|f| !name.to_lowercase().contains(f)) {
            return Vec::new();
        }
        let pid = pid.unwrap_or(0);
        if pid == own {
            return Vec::new();
        }
        let wins: Vec<Obj> = wins.unwrap_or_default().iter().filter_map(Obj::from_ref).collect();
        let infos = join_all(wins.into_iter().map(|w| async move {
            let (st, role, title) = futures::join!(
                call::<_, Vec<u32>>(conn, &w, I_ACCESSIBLE, "GetState", &()),
                call::<_, u32>(conn, &w, I_ACCESSIBLE, "GetRole", &()),
                get_prop(conn, &w, I_ACCESSIBLE, "Name"),
            );
            (w, st.map(|s| state_bits(&s)).unwrap_or(0), role.unwrap_or(0), prop_string(title))
        }))
        .await;
        infos
            .into_iter()
            .enumerate()
            .map(|(idx, (w, st, role, title))| {
                let score = window_score(st, role, &name);
                // Ties: later applications and later (newer) top-levels win — without a
                // window manager nothing is Active and the newest window is on top.
                (score, (order, idx), Target { app: app.clone(), app_name: name.clone(), window: w, window_title: title, pid })
            })
            .collect::<Vec<_>>()
    });
    let mut best: Option<(u32, (usize, usize), Target)> = None;
    for (score, order, t) in join_all(per_app).await.into_iter().flatten() {
        if score > 0 && best.as_ref().is_none_or(|(s, o, _)| (score, order) > (*s, *o)) {
            best = Some((score, order, t));
        }
    }
    best.map(|b| b.2).ok_or(DomError::NoFocusedApp)
}

// ---------------------------------------------------------------------------
// Node fetching
// ---------------------------------------------------------------------------

struct Fetched {
    role: u32,
    name: String,
    description: String,
    states: u64,
    ifaces: u32,
    children: Vec<Obj>,
    extents: Option<(i32, i32, i32, i32)>,
    actions: Vec<(String, String, String)>,
    text: Option<String>,
    value: Option<f64>,
    label: Option<String>,
}

struct Pending {
    obj: Obj,
    id: u64,
    parent: u64,
    depth: u16,
    parent_selection: bool,
    in_menu: bool,
    in_scroll: bool,
}

fn is_text_role(r: DomRole) -> bool {
    r.accepts_text()
}

/// All attributes of one node: one round of concurrent calls, then the
/// interface-dependent ones.
async fn fetch_node(conn: &Connection, obj: &Obj, want_children: bool) -> std::result::Result<Fetched, zbus::Error> {
    let (role, name, desc, states, ifaces, children) = futures::join!(
        call::<_, u32>(conn, obj, I_ACCESSIBLE, "GetRole", &()),
        get_prop(conn, obj, I_ACCESSIBLE, "Name"),
        get_prop(conn, obj, I_ACCESSIBLE, "Description"),
        call::<_, Vec<u32>>(conn, obj, I_ACCESSIBLE, "GetState", &()),
        call::<_, Vec<String>>(conn, obj, I_ACCESSIBLE, "GetInterfaces", &()),
        async {
            if want_children {
                call::<_, Vec<ObjRef>>(conn, obj, I_ACCESSIBLE, "GetChildren", &()).await
            } else {
                Ok(Vec::new())
            }
        },
    );
    let role = role?;
    let states = state_bits(&states?);
    let ifaces = interface_mask(&ifaces.unwrap_or_default());
    let name = prop_string(name);
    let description = prop_string(desc);
    let children = children.unwrap_or_default().iter().filter_map(Obj::from_ref).collect();

    let dom_role = map_role_raw(role, has_state(states, State::MultiLine));
    let secure = role == Role::PasswordText as u32;
    let want_text = ifaces & IF_TEXT != 0
        && !secure
        && (is_text_role(dom_role)
            || (name.is_empty() && matches!(dom_role, DomRole::StaticText | DomRole::Heading | DomRole::Link)));
    // Single-line fields: the whole text; multi-line / documents: bounded.
    let text_end = if dom_role == DomRole::TextField { -1 } else { MAX_VALUE_CHARS as i32 + 1 };
    let want_label = name.is_empty()
        && (dom_role.accepts_text() || matches!(dom_role, DomRole::Slider | DomRole::CheckBox | DomRole::Switch));
    let (extents, actions, text, value, label) = futures::join!(
        async {
            if ifaces & IF_COMPONENT != 0 {
                call::<_, (i32, i32, i32, i32)>(conn, obj, I_COMPONENT, "GetExtents", &(atspi::CoordType::Screen as u32))
                    .await
                    .ok()
            } else {
                None
            }
        },
        async {
            if ifaces & IF_ACTION != 0 {
                call::<_, Vec<(String, String, String)>>(conn, obj, I_ACTION, "GetActions", &()).await.unwrap_or_default()
            } else {
                Vec::new()
            }
        },
        async {
            if want_text {
                call::<_, String>(conn, obj, I_TEXT, "GetText", &(0i32, text_end)).await.ok()
            } else {
                None
            }
        },
        async {
            if ifaces & IF_VALUE != 0 {
                get_prop(conn, obj, I_VALUE, "CurrentValue").await.ok().and_then(|v| value_f64(&v))
            } else {
                None
            }
        },
        async {
            if !want_label {
                return None;
            }
            let rels: Vec<(u32, Vec<ObjRef>)> = call(conn, obj, I_ACCESSIBLE, "GetRelationSet", &()).await.ok()?;
            let target = rels.iter().find(|(t, _)| *t == RELATION_LABELLED_BY)?.1.iter().find_map(Obj::from_ref)?;
            Some(prop_string(get_prop(conn, &target, I_ACCESSIBLE, "Name").await)).filter(|s| !s.is_empty())
        },
    );
    Ok(Fetched { role, name, description, states, ifaces, children, extents, actions, text, value, label })
}

fn build_node(id: u64, parent: Option<u64>, f: &Fetched, p: &Pending) -> DomNode {
    let st = |s| has_state(f.states, s);
    let role = map_role_raw(f.role, st(State::MultiLine));
    let secure = f.role == Role::PasswordText as u32;
    let mut name = f.name.clone();
    let mut name_from_text = false;
    if name.is_empty()
        && let Some(l) = &f.label
    {
        name = l.clone();
    }
    if name.is_empty() && !role.accepts_text() && f.text.is_some() {
        name_from_text = true;
        name = f.text.clone().unwrap_or_default().trim().to_string();
    }
    let value = if secure {
        None
    } else if matches!(role, DomRole::CheckBox | DomRole::Switch | DomRole::RadioButton) {
        Some(if st(State::Checked) || st(State::Pressed) { "1" } else { "0" }.to_string())
    } else if let (Some(t), false) = (&f.text, name_from_text) {
        Some(truncate_chars(t, MAX_VALUE_CHARS))
    } else {
        f.value.map(format_number)
    };
    let description = Some(f.description.clone()).filter(|d| !d.is_empty() && *d != name);
    let bounds = match f.extents {
        Some((x, y, w, h)) if w > 0 || h > 0 => Rect::new(x, y, w.max(0) as u32, h.max(0) as u32),
        _ => Rect::default(),
    };
    let names: Vec<String> = f.actions.iter().map(|a| a.0.clone()).collect();
    let component = f.ifaces & IF_COMPONENT != 0;
    let adjustable_role = Role::try_from(f.role)
        .is_ok_and(|r| matches!(r, Role::Slider | Role::SpinButton | Role::ScrollBar | Role::Dial));
    let cx = ActionContext {
        focusable: component && st(State::Focusable),
        expandable: st(State::Expandable),
        expanded: st(State::Expanded),
        adjustable: f.ifaces & IF_VALUE != 0 && adjustable_role,
        parent_selection: p.parent_selection,
        scrollable: component && p.in_scroll,
    };
    let shortcut = f.actions.iter().find_map(|a| format_keybinding(&a.2));
    DomNode {
        id,
        parent,
        role,
        native_role: role_name(f.role),
        name,
        value,
        description,
        shortcut,
        bounds,
        actions: map_actions(&names, role, cx),
        enabled: st(State::Enabled) || st(State::Sensitive),
        focused: st(State::Focused),
        children: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

enum Request {
    Snapshot { max_depth: u16, interactive_only: bool, reply: mpsc::Sender<Result<DomSnapshot>> },
    Perform { node: u64, action: DomAction, reply: mpsc::Sender<Result<()>> },
    SetValue { node: u64, value: String, reply: mpsc::Sender<Result<()>> },
    SetTarget { app: Option<String>, reply: mpsc::Sender<Result<()>> },
}

struct Worker {
    conn: Option<Connection>,
    ids: IdMap,
    revision: u64,
    node_budget: usize,
    time_budget: Duration,
    app_filter: Option<String>,
}

fn dbus_err(node: u64, action: DomAction, e: zbus::Error) -> DomError {
    if is_stale(&e) {
        DomError::UnknownNode(node)
    } else {
        DomError::Os(format!("AT-SPI {action:?} on node {node}: {e}"))
    }
}

impl Worker {
    async fn conn(&mut self) -> Result<Connection> {
        if let Some(c) = &self.conn {
            return Ok(c.clone());
        }
        let c = connect_with_timeout().await?;
        self.conn = Some(c.clone());
        Ok(c)
    }

    fn note_error(&mut self, e: &zbus::Error) {
        if is_transport(e) {
            self.conn = None;
        }
    }

    async fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        let deadline = Instant::now() + self.time_budget;
        let conn = self.conn().await?;
        let target = match find_target(&conn, self.app_filter.as_deref()).await {
            Ok(t) => t,
            Err(e) => {
                if matches!(e, DomError::Os(_)) {
                    self.conn = None; // registry unreachable: reconnect next time
                }
                return Err(e);
            }
        };
        self.ids.begin();
        let root_id = self.ids.id_for(&target.app, None);
        let mut root = DomNode {
            id: root_id,
            parent: None,
            role: DomRole::Group,
            native_role: "application".into(),
            name: target.app_name.clone(),
            value: None,
            description: None,
            shortcut: None,
            bounds: Rect::default(),
            actions: Vec::new(),
            enabled: true,
            focused: false,
            children: Vec::new(),
        };
        let mut nodes: Vec<DomNode> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::from([root_id]);
        let mut queue: VecDeque<Pending> = VecDeque::new();
        let mut allocated = 1usize;
        if max_depth >= 1 {
            let id = self.ids.id_for(&target.window, Some(root_id));
            if seen.insert(id) {
                root.children.push(id);
                queue.push_back(Pending {
                    obj: target.window.clone(),
                    id,
                    parent: root_id,
                    depth: 1,
                    parent_selection: false,
                    in_menu: false,
                    in_scroll: false,
                });
                allocated += 1;
            }
        }
        nodes.push(root);
        while !queue.is_empty() && Instant::now() < deadline {
            let batch: Vec<Pending> = queue.drain(..queue.len().min(BATCH)).collect();
            let fetched = join_all(batch.iter().map(|p| fetch_node(&conn, &p.obj, p.depth < max_depth))).await;
            for (p, f) in batch.into_iter().zip(fetched) {
                let f = match f {
                    Ok(f) => f,
                    Err(e) => {
                        self.note_error(&e);
                        continue; // vanished / timed out: dropped below
                    }
                };
                if p.depth >= 2 && !p.in_menu && !has_state(f.states, State::Showing) {
                    continue;
                }
                let mut node = build_node(p.id, Some(p.parent), &f, &p);
                let child_cx = (
                    f.ifaces & IF_SELECTION != 0,
                    p.in_menu || matches!(node.role, DomRole::Menu | DomRole::MenuBar | DomRole::MenuItem),
                    p.in_scroll || node.role == DomRole::ScrollArea,
                );
                if p.depth < max_depth {
                    for k in &f.children {
                        if allocated >= self.node_budget {
                            break;
                        }
                        let kid = self.ids.id_for(k, Some(p.id));
                        if seen.insert(kid) {
                            node.children.push(kid);
                            queue.push_back(Pending {
                                obj: k.clone(),
                                id: kid,
                                parent: p.id,
                                depth: p.depth + 1,
                                parent_selection: child_cx.0,
                                in_menu: child_cx.1,
                                in_scroll: child_cx.2,
                            });
                            allocated += 1;
                        }
                    }
                }
                nodes.push(node);
            }
        }
        let present: HashSet<u64> = nodes.iter().map(|n| n.id).collect();
        for n in &mut nodes {
            n.children.retain(|c| present.contains(c));
        }
        derive_container_names(&mut nodes);
        if interactive_only {
            nodes = prune_to_interactive(nodes, root_id);
        }
        self.ids.prune(ID_RETENTION_GENERATIONS);
        self.revision += 1;
        Ok(DomSnapshot {
            revision: self.revision,
            app_name: target.app_name,
            window_title: target.window_title,
            pid: target.pid,
            root: root_id,
            nodes,
        })
    }

    fn lookup(&self, node: u64) -> Result<(Obj, Option<Obj>)> {
        let (obj, parent) = self.ids.lookup(node).ok_or(DomError::UnknownNode(node))?;
        let parent = parent.and_then(|p| self.ids.lookup(p)).map(|(o, _)| o.clone());
        Ok((obj.clone(), parent))
    }

    async fn perform(&mut self, node: u64, action: DomAction) -> Result<()> {
        let (obj, parent) = self.lookup(node)?;
        let conn = self.conn().await?;
        let r = perform_on(&conn, node, action, &obj, parent.as_ref()).await;
        if let Err(DomError::Os(_)) = &r {
            // A broken transport surfaces as Os; probe cheaply to decide on reconnecting.
            if let Err(e) = call::<_, u32>(&conn, &obj, I_ACCESSIBLE, "GetRole", &()).await {
                self.note_error(&e);
            }
        }
        r
    }

    async fn set_value(&mut self, node: u64, value: &str) -> Result<()> {
        let (obj, _) = self.lookup(node)?;
        let conn = self.conn().await?;
        // `ActionUnsupported` needs an action; `Focus` stands for "edit".
        let act = DomAction::Focus;
        let err = |e| dbus_err(node, act, e);
        let ifaces: Vec<String> = call(&conn, &obj, I_ACCESSIBLE, "GetInterfaces", &()).await.map_err(err)?;
        let mask = interface_mask(&ifaces);
        if mask & IF_EDITABLE_TEXT != 0 {
            let ok: bool = call(&conn, &obj, I_EDITABLE_TEXT, "SetTextContents", &value).await.map_err(err)?;
            return if ok { Ok(()) } else { Err(DomError::Os(format!("SetTextContents refused by node {node}"))) };
        }
        if mask & IF_VALUE != 0
            && let Ok(v) = value.trim().parse::<f64>()
        {
            return set_prop(&conn, &obj, I_VALUE, "CurrentValue", Value::F64(v)).await.map_err(err);
        }
        Err(DomError::ActionUnsupported { node, action: act })
    }
}

async fn perform_on(conn: &Connection, node: u64, action: DomAction, obj: &Obj, parent: Option<&Obj>) -> Result<()> {
    let err = |e| dbus_err(node, action, e);
    let unsupported = DomError::ActionUnsupported { node, action };
    let ifaces: Vec<String> = call(conn, obj, I_ACCESSIBLE, "GetInterfaces", &()).await.map_err(err)?;
    let mask = interface_mask(&ifaces);
    let refused = |what: &str| DomError::Os(format!("AT-SPI {what} refused by node {node}"));
    let do_named = |names: Vec<(String, String, String)>, want: DomAction| async move {
        let names: Vec<String> = names.into_iter().map(|a| a.0).collect();
        let i = action_index(&names, want).ok_or(DomError::ActionUnsupported { node, action })?;
        let ok: bool = call(conn, obj, I_ACTION, "DoAction", &(i as i32)).await.map_err(err)?;
        if ok { Ok(()) } else { Err(refused(&format!("DoAction({})", names[i]))) }
    };
    let actions = || async {
        if mask & IF_ACTION != 0 {
            call::<_, Vec<(String, String, String)>>(conn, obj, I_ACTION, "GetActions", &()).await.map_err(err)
        } else {
            Ok(Vec::new())
        }
    };
    match action {
        DomAction::Press | DomAction::Toggle | DomAction::ShowMenu | DomAction::Expand | DomAction::Collapse => {
            do_named(actions().await?, action).await
        }
        DomAction::Focus => {
            if mask & IF_COMPONENT == 0 {
                return Err(unsupported);
            }
            let ok: bool = call(conn, obj, I_COMPONENT, "GrabFocus", &()).await.map_err(err)?;
            if ok { Ok(()) } else { Err(refused("GrabFocus")) }
        }
        DomAction::ScrollIntoView => {
            if mask & IF_COMPONENT == 0 {
                return Err(unsupported);
            }
            let ok: bool =
                call(conn, obj, I_COMPONENT, "ScrollTo", &(atspi::ScrollType::Anywhere as u32)).await.map_err(err)?;
            if ok { Ok(()) } else { Err(refused("ScrollTo")) }
        }
        DomAction::Increment | DomAction::Decrement => {
            if mask & IF_VALUE == 0 {
                return do_named(actions().await?, action).await;
            }
            let (cur, step, min, max) = futures::join!(
                get_prop(conn, obj, I_VALUE, "CurrentValue"),
                get_prop(conn, obj, I_VALUE, "MinimumIncrement"),
                get_prop(conn, obj, I_VALUE, "MinimumValue"),
                get_prop(conn, obj, I_VALUE, "MaximumValue"),
            );
            let cur = cur.map_err(err)?;
            let cur = value_f64(&cur).ok_or_else(|| refused("CurrentValue"))?;
            let f = |r: zbus::Result<OwnedValue>| r.ok().and_then(|v| value_f64(&v));
            let step = f(step).filter(|s| s.is_finite() && *s > 0.0).unwrap_or(1.0);
            let mut next = if action == DomAction::Increment { cur + step } else { cur - step };
            if let Some(max) = f(max).filter(|m| m.is_finite()) {
                next = next.min(max);
            }
            if let Some(min) = f(min).filter(|m| m.is_finite()) {
                next = next.max(min);
            }
            set_prop(conn, obj, I_VALUE, "CurrentValue", Value::F64(next)).await.map_err(err)
        }
        DomAction::Select => {
            if let Some(parent) = parent {
                let pif: Vec<String> = call(conn, parent, I_ACCESSIBLE, "GetInterfaces", &()).await.unwrap_or_default();
                if interface_mask(&pif) & IF_SELECTION != 0 {
                    let idx: i32 = call(conn, obj, I_ACCESSIBLE, "GetIndexInParent", &()).await.map_err(err)?;
                    if idx >= 0 {
                        let ok: bool = call(conn, parent, I_SELECTION, "SelectChild", &idx).await.map_err(err)?;
                        if ok {
                            return Ok(());
                        }
                    }
                }
            }
            do_named(actions().await?, action).await
        }
    }
}

fn worker_main(rx: mpsc::Receiver<Request>, ready: mpsc::Sender<Result<()>>, node_budget: usize, time_budget: Duration) {
    let mut w = Worker { conn: None, ids: IdMap::new(), revision: 0, node_budget, time_budget, app_filter: None };
    let first = zbus::block_on(w.conn()).map(|_| ());
    let ok = first.is_ok();
    let _ = ready.send(first);
    if !ok {
        return;
    }
    while let Ok(req) = rx.recv() {
        match req {
            Request::Snapshot { max_depth, interactive_only, reply } => {
                let _ = reply.send(zbus::block_on(w.snapshot(max_depth, interactive_only)));
            }
            Request::Perform { node, action, reply } => {
                let _ = reply.send(zbus::block_on(w.perform(node, action)));
            }
            Request::SetValue { node, value, reply } => {
                let _ = reply.send(zbus::block_on(w.set_value(node, &value)));
            }
            Request::SetTarget { app, reply } => {
                w.app_filter = app;
                let _ = reply.send(Ok(()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// AT-SPI2 [`DomProvider`]; all D-Bus work happens on a private worker thread.
pub struct AtspiProvider {
    tx: mpsc::Sender<Request>,
    time_budget: Duration,
}

fn worker_gone() -> DomError {
    DomError::Os("AT-SPI worker thread exited".into())
}

impl AtspiProvider {
    /// Connect to the accessibility bus; fails with [`DomError::Os`] when it
    /// is unavailable.
    pub fn new() -> Result<Self> {
        Self::with_limits(DEFAULT_NODE_BUDGET, DEFAULT_TIME_BUDGET)
    }

    pub fn with_limits(node_budget: usize, time_budget: Duration) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let node_budget = node_budget.max(2);
        std::thread::Builder::new()
            .name("aether-dom-atspi".into())
            .spawn(move || worker_main(rx, ready_tx, node_budget, time_budget))
            .map_err(|e| DomError::Os(format!("spawn AT-SPI worker: {e}")))?;
        match ready_rx.recv_timeout(CONNECT_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(Self { tx, time_budget }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(unavailable("connection timed out")),
        }
    }

    /// Snapshot a specific application (name contains `app`, case-insensitive)
    /// instead of the focused one; `None` restores focus tracking.
    pub fn set_target_app(&mut self, app: Option<&str>) -> Result<()> {
        let app = app.map(str::to_string);
        self.roundtrip(|reply| Request::SetTarget { app, reply }, Duration::from_secs(5))
    }

    fn roundtrip<T>(&self, build: impl FnOnce(mpsc::Sender<Result<T>>) -> Request, wait: Duration) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(build(tx)).map_err(|_| worker_gone())?;
        rx.recv_timeout(wait).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => DomError::Os("AT-SPI request timed out".into()),
            mpsc::RecvTimeoutError::Disconnected => worker_gone(),
        })?
    }

    fn call_wait(&self) -> Duration {
        self.time_budget + CONNECT_TIMEOUT + Duration::from_secs(5)
    }
}

impl DomProvider for AtspiProvider {
    fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot> {
        self.roundtrip(|reply| Request::Snapshot { max_depth, interactive_only, reply }, self.call_wait())
    }

    fn perform(&mut self, node: u64, action: DomAction) -> Result<()> {
        self.roundtrip(|reply| Request::Perform { node, action, reply }, self.call_wait())
    }

    fn set_value(&mut self, node: u64, value: &str) -> Result<()> {
        let value = value.to_string();
        self.roundtrip(|reply| Request::SetValue { node, value, reply }, self.call_wait())
    }
}

/// The focused top-level window as AT-SPI sees it (for `context`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWindow {
    pub pid: u32,
    pub app_name: String,
    pub title: String,
}

/// Query the active window on a short-lived worker thread (safe to call from
/// inside an async runtime). `None` when AT-SPI is unavailable or nothing is
/// focused within `timeout`.
pub fn active_window(timeout: Duration) -> Option<ActiveWindow> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aether-dom-atspi-ctx".into())
        .spawn(move || {
            let r = zbus::block_on(async {
                with_timeout(timeout, async {
                    let conn = connect().await.ok()?;
                    let t = find_target(&conn, None).await.ok()?;
                    Some(ActiveWindow { pid: t.pid, app_name: t.app_name, title: t.window_title })
                })
                .await
                .flatten()
            });
            let _ = tx.send(r);
        })
        .ok()?;
    rx.recv_timeout(timeout + Duration::from_millis(500)).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn roles() {
        assert_eq!(map_role(Role::Button, false), DomRole::Button);
        assert_eq!(map_role(Role::ToggleButton, false), DomRole::Switch);
        assert_eq!(map_role(Role::CheckBox, false), DomRole::CheckBox);
        assert_eq!(map_role(Role::Text, false), DomRole::TextField);
        assert_eq!(map_role(Role::Text, true), DomRole::TextArea);
        assert_eq!(map_role(Role::PasswordText, false), DomRole::TextField);
        assert_eq!(map_role(Role::DocumentText, false), DomRole::TextArea);
        assert_eq!(map_role(Role::CheckMenuItem, false), DomRole::MenuItem);
        assert_eq!(map_role(Role::PageTab, false), DomRole::Tab);
        assert_eq!(map_role(Role::PageTabList, false), DomRole::TabGroup);
        assert_eq!(map_role(Role::TreeTable, false), DomRole::Tree);
        assert_eq!(map_role(Role::TableCell, false), DomRole::Cell);
        assert_eq!(map_role(Role::Label, false), DomRole::StaticText);
        assert_eq!(map_role(Role::Filler, false), DomRole::Group);
        assert_eq!(map_role(Role::DocumentWeb, false), DomRole::WebArea);
        assert_eq!(map_role(Role::Alert, false), DomRole::Dialog);
        assert_eq!(map_role(Role::FileChooser, false), DomRole::Dialog);
        assert_eq!(map_role(Role::Frame, false), DomRole::Window);
        assert_eq!(map_role(Role::SpinButton, false), DomRole::Slider);
        assert_eq!(map_role(Role::Separator, false), DomRole::Other);
        // Raw values follow the AT-SPI enum (ATSPI_ROLE_PUSH_BUTTON = 43, FRAME = 23).
        assert_eq!(map_role_raw(43, false), DomRole::Button);
        assert_eq!(map_role_raw(23, false), DomRole::Window);
        assert_eq!(map_role_raw(9999, false), DomRole::Other);
        assert_eq!(role_name(43), "button");
        assert_eq!(role_name(9999), "role 9999");
    }

    #[test]
    fn actions() {
        let cx = ActionContext::default();
        assert_eq!(map_actions(&s(&["click"]), DomRole::Button, cx), vec![DomAction::Press]);
        assert_eq!(map_actions(&s(&["click"]), DomRole::CheckBox, cx), vec![DomAction::Press, DomAction::Toggle]);
        assert_eq!(map_actions(&s(&["toggle"]), DomRole::Switch, cx), vec![DomAction::Toggle]);
        assert_eq!(
            map_actions(&s(&["activate", "expand or contract"]), DomRole::TreeItem, ActionContext { expanded: true, ..cx }),
            vec![DomAction::Press, DomAction::Collapse]
        );
        assert_eq!(map_actions(&s(&["expand or contract"]), DomRole::TreeItem, cx), vec![DomAction::Expand]);
        assert_eq!(map_actions(&s(&["ShowMenu", "Jump"]), DomRole::Link, cx), vec![DomAction::ShowMenu, DomAction::Press]);
        assert_eq!(
            map_actions(&[], DomRole::Slider, ActionContext { adjustable: true, focusable: true, ..cx }),
            vec![DomAction::Focus, DomAction::Increment, DomAction::Decrement]
        );
        assert_eq!(
            map_actions(&[], DomRole::Cell, ActionContext { parent_selection: true, ..cx }),
            vec![DomAction::Select]
        );
        assert_eq!(
            map_actions(&s(&["activate"]), DomRole::Switch, ActionContext { expandable: true, ..cx }),
            vec![DomAction::Press, DomAction::Toggle, DomAction::Expand]
        );
        assert_eq!(
            map_actions(&s(&["click"]), DomRole::Button, ActionContext { scrollable: true, ..cx }),
            vec![DomAction::Press, DomAction::ScrollIntoView]
        );
        assert!(map_actions(&s(&["custom"]), DomRole::Group, ActionContext { scrollable: true, ..cx }).is_empty());
    }

    #[test]
    fn action_lookup() {
        let n = s(&["activate", "expand or contract", "toggle"]);
        assert_eq!(action_index(&n, DomAction::Press), Some(0));
        assert_eq!(action_index(&n, DomAction::Toggle), Some(2));
        assert_eq!(action_index(&n, DomAction::Expand), Some(1));
        assert_eq!(action_index(&n, DomAction::Collapse), Some(1));
        assert_eq!(action_index(&s(&["click"]), DomAction::Toggle), Some(0));
        assert_eq!(action_index(&s(&["Show_Menu"]), DomAction::ShowMenu), Some(0));
        assert_eq!(action_index(&s(&["click"]), DomAction::ShowMenu), None);
        assert_eq!(action_index(&s(&["click"]), DomAction::Focus), None);
        assert_eq!(action_index(&[], DomAction::Press), None);
    }

    #[test]
    fn keybindings() {
        assert_eq!(format_keybinding("<Alt>f;;<Control>o").as_deref(), Some("Ctrl+O"));
        assert_eq!(format_keybinding("n;<Alt>f:n;<Primary><Shift>n").as_deref(), Some("Ctrl+Shift+N"));
        assert_eq!(format_keybinding("<Alt>c").as_deref(), Some("Alt+C"));
        assert_eq!(format_keybinding("c").as_deref(), None);
        assert_eq!(format_keybinding("").as_deref(), None);
        assert_eq!(format_keybinding(";;<Control>Return").as_deref(), Some("Ctrl+Enter"));
        assert_eq!(format_keybinding(";;F5").as_deref(), Some("F5"));
        assert_eq!(format_keybinding("Ctrl+Shift+P").as_deref(), Some("Ctrl+Shift+P"));
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = node_id(":1.42", "/org/a11y/atspi/accessible/17");
        assert_eq!(a, node_id(":1.42", "/org/a11y/atspi/accessible/17"));
        assert_ne!(a, node_id(":1.42", "/org/a11y/atspi/accessible/18"));
        assert_ne!(a, node_id(":1.43", "/org/a11y/atspi/accessible/17"));
        // (bus, path) boundary is unambiguous.
        assert_ne!(node_id(":1.4", "2/x"), node_id(":1.42", "/x"));
        // Pinned value: the hash must not change between builds/processes.
        assert_eq!(node_id(":1.42", "/org/a11y/atspi/accessible/root"), hash_key(":1.42", "/org/a11y/atspi/accessible/root", 0));
        let mut all = HashSet::new();
        for i in 0..20_000 {
            let id = node_id(":1.7", &format!("/org/a11y/atspi/accessible/{i}"));
            assert!(id != 0 && id <= ID_MASK);
            assert!(all.insert(id));
        }
    }

    #[test]
    fn id_map_retention() {
        let mut m = IdMap::new();
        let a = Obj::new(":1.5", "/a");
        let b = Obj::new(":1.5", "/b");
        m.begin();
        let ia = m.id_for(&a, None);
        let ib = m.id_for(&b, Some(ia));
        assert_eq!(ia, node_id(":1.5", "/a"));
        assert_eq!(m.lookup(ib).map(|(o, p)| (o.clone(), p)), Some((b.clone(), Some(ia))));
        for _ in 0..4 {
            m.begin();
            assert_eq!(m.id_for(&a, None), ia);
        }
        m.prune(2);
        assert!(m.lookup(ib).is_none());
        assert_eq!(m.len(), 1);
        m.begin();
        assert_eq!(m.id_for(&b, None), ib, "same element → same id even after pruning");
    }

    #[test]
    fn state_and_score() {
        let bits = |v: &[State]| v.iter().fold(0u64, |m, s| m | StateSet::new(*s).bits());
        assert_eq!(state_bits(&[1, 2]), 1 | (2 << 32));
        let frame = Role::Frame as u32;
        assert!(has_state(bits(&[State::Active]), State::Active));
        assert_eq!(window_score(bits(&[State::Active, State::Showing]), frame, "gedit"), 100);
        assert_eq!(window_score(bits(&[State::Active]), frame, "gnome-shell"), 100);
        assert_eq!(window_score(bits(&[State::Showing]), frame, "gnome-shell"), 0);
        assert_eq!(window_score(bits(&[State::Showing, State::Visible]), frame, "gedit"), 20);
        assert_eq!(window_score(bits(&[State::Showing, State::Modal]), Role::Dialog as u32, "gedit"), 30);
        assert_eq!(window_score(bits(&[State::Showing, State::Iconified]), frame, "gedit"), 0);
        assert_eq!(window_score(bits(&[]), frame, "gedit"), 0);
        assert_eq!(
            interface_mask(&s(&["org.a11y.atspi.Accessible", "org.a11y.atspi.Action", "org.a11y.atspi.Value", "x.Y"])),
            IF_ACTION | IF_VALUE
        );
    }

    #[test]
    fn numbers_and_truncation() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(0.5), "0.5");
        assert_eq!(format_number(2.126), "2.13");
        assert_eq!(truncate_chars("perché", 4), "perc…");
    }

    // -----------------------------------------------------------------------
    // Live AT-SPI test (ci/linux-test.sh harness: Xvfb + a11y bus + gtk3-demo)
    // -----------------------------------------------------------------------

    fn find<'a>(s: &'a DomSnapshot, role: DomRole, name: &str) -> Option<&'a DomNode> {
        s.nodes.iter().find(|n| n.role == role && n.name == name)
    }

    /// Gives X input focus to the top-level window titled `title` (what a click does under a
    /// window manager; the harness has none, so GTK only marks the focused window Active).
    fn x11_focus(title: &str) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, InputFocus};
        let Ok((c, screen)) = x11rb::connect(None) else { return };
        let root = c.setup().roots[screen].root;
        let Ok(Ok(tree)) = c.query_tree(root).map(|r| r.reply()) else { return };
        for w in tree.children {
            let name = c
                .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
                .ok()
                .and_then(|r| r.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.value).into_owned());
            if name.as_deref() == Some(title) {
                let _ = c.set_input_focus(InputFocus::POINTER_ROOT, w, x11rb::CURRENT_TIME);
                let _ = c.flush();
                std::thread::sleep(Duration::from_millis(300));
                return;
            }
        }
    }

    #[test]
    #[ignore = "needs an AT-SPI bus and a running GTK app (ci/linux-test.sh)"]
    fn live_gtk_snapshot_perform_set_value() {
        let app = std::env::var("AETHER_DOM_TEST_APP").unwrap_or_else(|_| "gtk3-demo".into());
        // `gtk3-demo --run=dialog` opens the demo window over the main window.
        x11_focus("Dialogs and Message Boxes");
        let mut p = AtspiProvider::new().expect("AT-SPI bus");
        p.set_target_app(Some(&app)).unwrap();
        let s1 = p.snapshot(32, false).expect("snapshot of the GTK app");
        eprintln!("app={:?} window={:?} pid={} nodes={}", s1.app_name, s1.window_title, s1.pid, s1.nodes.len());
        for n in &s1.nodes {
            eprintln!("  {:>16} {:?} {:?} {:?} value={:?} {:?} {:?}", n.id, n.role, n.native_role, n.name, n.value, n.bounds, n.actions);
        }
        assert!(s1.pid > 0 && s1.pid != std::process::id());
        assert!(s1.app_name.to_lowercase().contains(&app.to_lowercase()));
        let root = s1.nodes.iter().find(|n| n.id == s1.root).unwrap();
        let win = s1.nodes.iter().find(|n| Some(&n.id) == root.children.first()).expect("window node");
        assert!(matches!(win.role, DomRole::Window | DomRole::Dialog), "{win:?}");
        assert!(win.bounds.width > 0 && win.bounds.height > 0, "window bounds {:?}", win.bounds);

        // gtk3-demo "Dialogs and Message Boxes": buttons + two labelled entries.
        let button = find(&s1, DomRole::Button, "Interactive Dialog").expect("Interactive Dialog button");
        assert!(button.actions.contains(&DomAction::Press));
        assert!(button.bounds.width > 0 && button.bounds.height > 0);
        assert!(button.enabled);
        let entry = s1.nodes.iter().find(|n| n.role == DomRole::TextField).expect("an entry");
        assert!(entry.actions.contains(&DomAction::Focus));

        // Ids are stable across snapshots, so the diff is small.
        let s2 = p.snapshot(32, false).unwrap();
        assert!(s2.revision > s1.revision);
        let ids1: HashSet<u64> = s1.nodes.iter().map(|n| n.id).collect();
        let ids2: HashSet<u64> = s2.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids1, ids2, "same UI → same ids");
        assert!(crate::is_noop(&crate::diff(&s1, &s2)), "{:?}", crate::diff(&s1, &s2));

        // set_value on the entry shows up in the next snapshot.
        p.set_value(entry.id, "aether ✓ value").unwrap();
        let s3 = p.snapshot(32, false).unwrap();
        let e3 = s3.nodes.iter().find(|n| n.id == entry.id).expect("entry keeps its id");
        assert_eq!(e3.value.as_deref(), Some("aether ✓ value"));
        let d = crate::diff(&s2, &s3);
        assert!(d.upserted.iter().any(|n| n.id == entry.id) && d.upserted.len() < 5, "{d:?}");

        // Actions with a visible effect. (Pressing "Interactive Dialog" is not used: GTK 3
        // runs that dialog in a nested main loop from inside the AT-SPI DoAction handler, so
        // the application's accessibility stops answering until the dialog closes.)
        // Select the "Source" page of the main window's notebook: the text view below
        // switches from the demo description to its C source.
        x11_focus("Application Class");
        let m1 = p.snapshot(32, false).unwrap();
        assert_eq!(m1.window_title, "Application Class");
        let text_of = |s: &DomSnapshot| s.nodes.iter().find(|n| n.role == DomRole::TextArea).and_then(|n| n.value.clone());
        let before = text_of(&m1).unwrap_or_default();
        let source = find(&m1, DomRole::Tab, "Source").expect("Source tab");
        p.perform(source.id, DomAction::Select).unwrap();
        let mut switched = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            let s = p.snapshot(32, false).unwrap();
            if text_of(&s).is_some_and(|t| t != before) {
                switched = true;
                break;
            }
        }
        assert!(switched, "selecting the Source tab did not change the page");
        // Focus on the dialog window's entry is reflected in the next snapshot.
        x11_focus("Dialogs and Message Boxes");
        p.perform(entry.id, DomAction::Focus).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let s6 = p.snapshot(32, false).unwrap();
        assert!(s6.nodes.iter().any(|n| n.id == entry.id && n.focused), "entry focused after Focus");

        // Unknown ids.
        assert!(matches!(p.perform(1, DomAction::Press), Err(DomError::UnknownNode(1))));
        let _ = button;
    }

    #[test]
    #[ignore = "needs an AT-SPI bus and a running GTK app (ci/linux-test.sh)"]
    fn live_active_window() {
        let w = active_window(Duration::from_secs(3)).expect("an active window");
        assert!(w.pid > 0);
        eprintln!("{w:?}");
    }
}
