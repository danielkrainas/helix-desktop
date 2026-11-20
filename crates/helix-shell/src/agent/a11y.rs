//! *Control apps* (helixdesktop/spec `contracts/agent-access/README.md`,
//! "Control apps methods"; helixdesktop/desktop#149): reading an app
//! window's controls over AT-SPI and using them, for agents. The shell is
//! the AT-SPI client: it connects to the accessibility bus itself, so an
//! agent never talks to an app's accessible objects, and every read and
//! action goes through the checks here first.
//!
//! - **Which tree.** A window of the compositor's list is mapped to its
//!   app's accessible objects by its process: the window's own accessible
//!   object when its app told the compositor (GTK 4's
//!   `gtk_surface1.set_a11y_properties`, checked against the process that
//!   owns that name on the accessibility bus), otherwise the one app on
//!   the accessibility bus of the window's process and, among its
//!   top-level objects, the only one, or the only one named as the window
//!   is titled. Anything else is ambiguous and refused, never guessed.
//! - **Protected.** The shell's own process (its UI, prompts and dialogs),
//!   Settings, the system's password and permission prompts (by their
//!   programs) and excluded apps (also when another window of the same
//!   process is theirs, or the process is their program) are refused, as
//!   is everything while a system prompt is shown.
//! - **Password fields** (the role `password text`, or an editable text
//!   whose text is all mask characters or whose name says password) have
//!   no value, their insides are not read, and nothing may be done to
//!   them or inside them.
//! - **Nodes** are named by their object path (shortened against the
//!   window's own); an action first walks from the node up to the window,
//!   so a node of another window, another app or a password field is
//!   refused.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use helix_i18n::{n_, tr};
use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Type, Value};
use zbus::Connection;

use super::desktop::{Installed, Snapshot, WindowInfo};
use super::policy::{self, AgentError, Config, Target};
use super::value;

/// The registry and the desktop's root object on the accessibility bus.
pub const REGISTRY: &str = "org.a11y.atspi.Registry";
pub const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// How long an app has to answer one call.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long reading a whole tree may take before the rest is left out.
const TREE_BUDGET: Duration = Duration::from_secs(20);
/// How deep trees are read, and how far an action walks up to its window.
pub const DEPTH_MAX: u32 = 64;
/// Nodes read by `A11yTree` (`max-nodes`), and by `A11yFind` looking.
pub const NODES_DEFAULT: u32 = 1000;
pub const NODES_MAX: u32 = 5000;
/// Matches `A11yFind` returns (`limit`).
pub const FIND_DEFAULT: u32 = 50;
pub const FIND_MAX: u32 = 500;
/// The most characters of a text given as a node's `value`.
pub const TEXT_MAX: usize = 4096;
/// How often `A11yFind` with `timeout-ms` looks again.
pub const WAIT_POLL: Duration = Duration::from_millis(250);

/// AT-SPI roles by number (at-spi2-core's `AtspiRole`), as
/// `atspi_role_get_name` names them.
const ROLES: [&str; 131] = [
    "invalid", "accelerator label", "alert", "animation", "arrow", "calendar", "canvas", "check box", "check menu item", "color chooser",
    "column header", "combo box", "date editor", "desktop icon", "desktop frame", "dial", "dialog", "directory pane", "drawing area", "file chooser",
    "filler", "focus traversable", "font chooser", "frame", "glass pane", "html container", "icon", "image", "internal frame", "label",
    "layered pane", "list", "list item", "menu", "menu bar", "menu item", "option pane", "page tab", "page tab list", "panel",
    "password text", "popup menu", "progress bar", "button", "radio button", "radio menu item", "root pane", "row header", "scroll bar", "scroll pane",
    "separator", "slider", "spin button", "split pane", "status bar", "table", "table cell", "table column header", "table row header", "tearoff menu item",
    "terminal", "text", "toggle button", "tool bar", "tool tip", "tree", "tree table", "unknown", "viewport", "window",
    "extended", "header", "footer", "paragraph", "ruler", "application", "autocomplete", "editbar", "embedded", "entry",
    "chart", "caption", "document frame", "heading", "page", "section", "redundant object", "form", "link", "input method window",
    "table row", "tree item", "document spreadsheet", "document presentation", "document text", "document web", "document email", "comment", "list box", "grouping",
    "image map", "notification", "info bar", "level bar", "title bar", "block quote", "audio", "video", "definition", "article",
    "landmark", "log", "marquee", "math", "rating", "timer", "static", "math fraction", "math root", "subscript",
    "superscript", "description list", "description term", "description value", "footnote", "content deletion", "content insertion", "mark", "suggestion", "push button menu",
    "switch",
];

/// `password text`.
pub const ROLE_PASSWORD_TEXT: u32 = 40;

/// AT-SPI states by bit (`AtspiStateType`).
const STATES: [&str; 44] = [
    "invalid", "active", "armed", "busy", "checked", "collapsed", "defunct", "editable", "enabled", "expandable",
    "expanded", "focusable", "focused", "has-tooltip", "horizontal", "iconified", "modal", "multi-line", "multiselectable", "opaque",
    "pressed", "resizable", "selectable", "selected", "sensitive", "showing", "single-line", "stale", "transient", "vertical",
    "visible", "manages-descendants", "indeterminate", "required", "truncated", "animated", "invalid-entry", "supports-autocompletion", "selectable-text", "is-default",
    "visited", "checkable", "has-popup", "read-only",
];

const STATE_SHOWING: u64 = 1 << 25;
const STATE_VISIBLE: u64 = 1 << 30;

/// A role's name; `role <n>` for numbers this table does not know.
pub fn role_name(role: u32) -> &'static str {
    ROLES.get(role as usize).copied().unwrap_or("unknown")
}

/// The role named `name` (`A11yFind`'s `role`).
pub fn role_by_name(name: &str) -> Option<u32> {
    ROLES.iter().position(|r| *r == name).map(|i| i as u32)
}

/// The names of the states set in `bits`.
pub fn state_names(bits: u64) -> Vec<&'static str> {
    (1..STATES.len()).filter(|i| bits & (1 << i) != 0).map(|i| STATES[i]).collect()
}

/// The state bit named `name`.
pub fn state_bit(name: &str) -> Option<u64> {
    STATES.iter().position(|s| *s == name).filter(|i| *i > 0).map(|i| 1u64 << i)
}

/// AT-SPI's two state words as one set of bits.
fn state_bits(words: &[u32]) -> u64 {
    u64::from(words.first().copied().unwrap_or(0)) | u64::from(words.get(1).copied().unwrap_or(0)) << 32
}

/// Characters toolkits show in place of a hidden text's.
fn mask_char(c: char) -> bool {
    matches!(c, '•' | '●' | '∗' | '*' | '⦁' | '⚫' | '◦' | '·')
}

/// Whether `text` is a hidden text as toolkits show it: only mask
/// characters (GTK 4's entries with hidden characters, which it reports
/// as plain text).
pub fn masked(text: &str) -> bool {
    !text.is_empty() && text.chars().all(mask_char)
}

/// Words that, in an editable text's name, make it a password field.
// Translators: comma-separated words; an editable text field whose
// accessible name contains one of them is treated as a password field
// and never read or filled in by agents. Translate each word, keep the
// commas; add words your language uses for passwords and PINs.
const SECRET_WORDS: &str = n_("password,passphrase,passcode,PIN,secret");

/// Words that make using a control a sensitive action.
// Translators: comma-separated words; using a control whose accessible
// name contains one of them counts as a sensitive action (the user may be
// asked first). Translate each word, keep the commas; several words for
// one are fine.
const SENSITIVE_WORDS: &str = n_("delete,remove,erase,discard,trash,send,submit,post,publish,pay,purchase,buy,order,checkout,transfer,donate,uninstall,format,wipe,reset,empty,revoke,quit,sign out,log out,shut down");

/// Whether `word` (lowercase) is in `text` (lowercase): as a whole word
/// for words of letters and digits of ASCII, anywhere for others (scripts
/// without spaces).
fn has_word(text: &str, word: &str) -> bool {
    let word = word.trim();
    if word.is_empty() {
        return false;
    }
    if !word.is_ascii() {
        return text.contains(word);
    }
    let bounded = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    let mut from = 0;
    while let Some(at) = text[from..].find(word) {
        let start = from + at;
        let end = start + word.len();
        if bounded(text[..start].chars().next_back()) && bounded(text[end..].chars().next()) {
            return true;
        }
        from = start + word.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Whether `text` has a word of `list` (comma-separated), in English or in
/// the user's language.
fn names_one_of(text: &str, list: &'static str) -> bool {
    let text = text.to_lowercase();
    let mut words: Vec<String> = list.split(',').map(str::to_lowercase).collect();
    let translated = tr(list);
    if translated != list {
        words.extend(translated.split(',').map(str::to_lowercase));
    }
    words.iter().any(|w| has_word(&text, w))
}

/// Whether a node is a password field: the role `password text`, or an
/// editable text whose text is hidden or whose name says it is one.
pub fn password_like(role: u32, name: &str, editable: bool, text: Option<&str>) -> bool {
    role == ROLE_PASSWORD_TEXT || (editable && (text.is_some_and(masked) || names_one_of(name, SECRET_WORDS)))
}

/// Whether using a control named `name` (or described as `description`)
/// with `action` is a sensitive action by the shell's own rule.
pub fn sensitive_control(name: &str, description: &str, action: &str) -> bool {
    matches!(action, "window.close" | "selection.delete" | "text.clear") || names_one_of(name, SENSITIVE_WORDS) || names_one_of(description, SENSITIVE_WORDS)
}

/// Actions that use the clipboard: they need the `clipboard` switch, or
/// an agent could read the clipboard by pasting into a field it then
/// reads.
pub fn clipboard_action(action: &str) -> bool {
    let bare = action.rsplit('.').next().unwrap_or(action).to_lowercase();
    action.starts_with("clipboard.") || matches!(bare.as_str(), "paste" | "copy" | "cut")
}

/// Action names the activity log records as they are; others as `other`
/// (an app names its actions, and the log holds no app's words).
const KNOWN_ACTIONS: &[&str] = &[
    "click", "press", "activate", "toggle", "select", "focus", "default", "expand", "collapse", "open", "jump", "menu", "showmenu", "increment", "decrement",
    "window.close", "window.minimize", "window.toggle-maximized", "default.activate", "menu.popup", "selection.select-all", "selection.delete", "text.clear",
    "clipboard.copy", "clipboard.cut", "clipboard.paste", "link.open", "link.copy", "misc.insert-emoji",
];

/// `action` as the activity log records it.
pub fn log_action(action: &str) -> &'static str {
    KNOWN_ACTIONS.iter().find(|a| **a == action).copied().unwrap_or("other")
}

/// An accessible object: its app's name on the bus and its path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ObjRef(pub String, pub OwnedObjectPath);

/// The part of `root`'s path before its last element: node ids are paths
/// relative to it when they start with it.
fn prefix(root: &str) -> &str {
    match root.rfind('/') {
        Some(i) if i > 0 => &root[..=i],
        _ => "/",
    }
}

/// The id of the node at `path` in the tree of the window at `root`.
pub fn node_id(root: &str, path: &str) -> String {
    let p = prefix(root);
    match path.strip_prefix(p) {
        Some(rest) if p != "/" && !rest.is_empty() && !rest.contains('/') => rest.to_owned(),
        _ => path.to_owned(),
    }
}

/// The path of node `id` in the tree of the window at `root`.
pub fn node_path(root: &str, id: &str) -> Result<OwnedObjectPath, AgentError> {
    let bad = || AgentError::NotFound(format!("node {id}"));
    if id.is_empty() {
        return Err(bad());
    }
    let path = if id.starts_with('/') { id.to_owned() } else { format!("{}{id}", prefix(root)) };
    OwnedObjectPath::try_from(path).map_err(|_| bad())
}

/// The accessibility bus, connected to when first needed and again after
/// it went away.
#[derive(Default)]
pub struct Bus {
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl Bus {
    /// A connection to the accessibility bus: `AT_SPI_BUS_ADDRESS`, else
    /// the address `org.a11y.Bus` on `session` gives.
    pub async fn get(&self, session: &Connection) -> Result<Connection, AgentError> {
        let mut conn = self.conn.lock().await;
        if let Some(c) = conn.as_ref() {
            let alive = tokio::time::timeout(CALL_TIMEOUT, zbus::fdo::DBusProxy::new(c)).await;
            let ok = match alive {
                Ok(Ok(proxy)) => tokio::time::timeout(CALL_TIMEOUT, proxy.get_id()).await.is_ok_and(|r| r.is_ok()),
                _ => false,
            };
            if ok {
                return Ok(c.clone());
            }
            *conn = None;
        }
        let unavailable = |what: String| AgentError::NotSupported(format!("the accessibility bus is not available: {what}"));
        let address = match std::env::var("AT_SPI_BUS_ADDRESS").ok().filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => {
                let ask = session.call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some("org.a11y.Bus"), "GetAddress", &());
                let reply = tokio::time::timeout(CALL_TIMEOUT, ask).await.map_err(|_| unavailable("no answer".into()))?.map_err(|e| unavailable(e.to_string()))?;
                reply.body().deserialize::<String>().map_err(|e| unavailable(e.to_string()))?
            }
        };
        let builder = zbus::connection::Builder::address(address.as_str()).map_err(|e| unavailable(e.to_string()))?;
        let c = tokio::time::timeout(CALL_TIMEOUT, builder.build()).await.map_err(|_| unavailable("no answer".into()))?.map_err(|e| unavailable(e.to_string()))?;
        *conn = Some(c.clone());
        Ok(c)
    }
}

/// An error an app's object answered: gone objects and unknown methods
/// are `NotFound` and `NotSupported`, the rest `Failed`.
fn app_error(err: zbus::Error) -> AgentError {
    match &err {
        zbus::Error::MethodError(name, _, _) => match name.as_str() {
            "org.freedesktop.DBus.Error.UnknownObject" | "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
                AgentError::NotFound("no such node (the app changed or went away)".into())
            }
            "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface" | "org.freedesktop.DBus.Error.UnknownProperty" | "org.freedesktop.DBus.Error.NotSupported" => {
                AgentError::NotSupported("the app does not support this".into())
            }
            _ => AgentError::Failed(format!("the app answered: {err}")),
        },
        _ => AgentError::Failed(format!("the app did not answer: {err}")),
    }
}

/// An app's accessible objects on the accessibility bus.
#[derive(Clone)]
pub struct App {
    pub bus: Connection,
    /// Its name on the bus.
    pub name: String,
}

impl App {
    async fn call<B, R>(&self, path: &str, interface: &str, method: &str, body: &B) -> Result<R, AgentError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: serde::de::DeserializeOwned + Type,
    {
        let call = self.bus.call_method(Some(self.name.as_str()), path, Some(interface), method, body);
        let reply = tokio::time::timeout(CALL_TIMEOUT, call).await.map_err(|_| AgentError::Failed("the app did not answer in time".into()))?.map_err(app_error)?;
        reply.body().deserialize::<R>().map_err(|e| AgentError::Failed(format!("the app's answer was not understood: {e}")))
    }

    async fn property(&self, path: &str, interface: &str, name: &str) -> Result<OwnedValue, AgentError> {
        self.call(path, PROPERTIES, "Get", &(interface, name)).await
    }

    async fn text_property(&self, path: &str, name: &str) -> String {
        self.property(path, ACCESSIBLE, name).await.ok().and_then(|v| String::try_from(v).ok()).unwrap_or_default()
    }

    async fn number(&self, path: &str, name: &str) -> Option<f64> {
        self.property(path, "org.a11y.atspi.Value", name).await.ok().and_then(|v| f64::try_from(v).ok())
    }

    /// What the tree and the checks need of the object at `path`.
    pub async fn info(&self, path: &OwnedObjectPath) -> Result<Info, AgentError> {
        let p = path.as_str();
        let (role, states, interfaces, children, name, description) = tokio::join!(
            self.call::<_, u32>(p, ACCESSIBLE, "GetRole", &()),
            self.call::<_, Vec<u32>>(p, ACCESSIBLE, "GetState", &()),
            self.call::<_, Vec<String>>(p, ACCESSIBLE, "GetInterfaces", &()),
            self.call::<_, Vec<ObjRef>>(p, ACCESSIBLE, "GetChildren", &()),
            self.text_property(p, "Name"),
            self.text_property(p, "Description"),
        );
        // Every accessible object has a role: an object that does not
        // answer it is not there (GDBus says "unknown method" for paths
        // it does not serve).
        let role = role.map_err(|e| match e {
            AgentError::NotSupported(_) => AgentError::NotFound("no such node".into()),
            other => other,
        })?;
        let mut info = Info {
            path: path.clone(),
            role,
            name,
            description,
            states: state_bits(&states.unwrap_or_default()),
            interfaces: interfaces.unwrap_or_default(),
            children: children.unwrap_or_default(),
            ..Info::default()
        };
        // A password field's text is never read, not even masked.
        if role == ROLE_PASSWORD_TEXT {
            info.password = true;
            return Ok(info);
        }
        if info.has("Action") {
            // Names as the toolkit knows them (`click`): GetActions may
            // give them translated (GTK 4's "Click").
            let listed: Vec<(String, String, String)> = self.call(p, "org.a11y.atspi.Action", "GetActions", &()).await.unwrap_or_default();
            for (i, (shown, _, _)) in listed.into_iter().enumerate() {
                let name: Result<String, _> = self.call(p, "org.a11y.atspi.Action", "GetName", &(i as i32,)).await;
                info.actions.push(name.ok().filter(|n| !n.is_empty()).unwrap_or(shown));
            }
        }
        if info.has("Value") {
            if let Some(current) = self.number(p, "CurrentValue").await {
                let (min, max) = (self.number(p, "MinimumValue").await, self.number(p, "MaximumValue").await);
                info.number = Some((current, min.unwrap_or(f64::NAN), max.unwrap_or(f64::NAN)));
            }
        }
        if info.has("Text") {
            let count = self.property(p, "org.a11y.atspi.Text", "CharacterCount").await.ok().and_then(|v| i32::try_from(v).ok()).unwrap_or(-1);
            let end = if count < 0 { -1 } else { count.min(TEXT_MAX as i32 + 1) };
            if let Ok(text) = self.call::<_, String>(p, "org.a11y.atspi.Text", "GetText", &(0i32, end)).await {
                info.text_length = if count >= 0 { count as usize } else { text.chars().count() };
                info.text = Some(text);
            }
        }
        info.password = password_like(role, &info.name, info.editable(), info.text.as_deref());
        if info.password {
            info.text = None;
            info.number = None;
        }
        Ok(info)
    }

    /// The object's parent, if it has one.
    pub async fn parent(&self, path: &str) -> Result<Option<ObjRef>, AgentError> {
        let v = self.property(path, ACCESSIBLE, "Parent").await?;
        let parent = ObjRef::try_from(v).ok();
        Ok(parent.filter(|p| p.1.as_str() != "/org/a11y/atspi/null"))
    }

    pub async fn extents(&self, path: &str) -> Option<(i32, i32, i32, i32)> {
        // Window coordinates: Wayland tells apps nothing of where windows are.
        self.call(path, "org.a11y.atspi.Component", "GetExtents", &(1u32,)).await.ok()
    }
}

impl TryFrom<OwnedValue> for ObjRef {
    type Error = zbus::zvariant::Error;
    fn try_from(v: OwnedValue) -> Result<Self, Self::Error> {
        let (name, path): (String, OwnedObjectPath) = v.try_into()?;
        Ok(ObjRef(name, path))
    }
}

/// An accessible object, as read.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub path: OwnedObjectPath,
    pub role: u32,
    pub name: String,
    pub description: String,
    pub states: u64,
    pub interfaces: Vec<String>,
    pub actions: Vec<String>,
    /// Its text (at most [`TEXT_MAX`] + 1 characters), never a password
    /// field's.
    pub text: Option<String>,
    /// How long its whole text is.
    pub text_length: usize,
    /// Value, minimum and maximum (NaN: not told).
    pub number: Option<(f64, f64, f64)>,
    pub children: Vec<ObjRef>,
    pub password: bool,
}

impl Info {
    pub fn has(&self, interface: &str) -> bool {
        self.interfaces.iter().any(|i| i == interface || i.strip_prefix("org.a11y.atspi.") == Some(interface))
    }

    pub fn editable(&self) -> bool {
        self.has("EditableText")
    }

    /// The node's record (README.md, "Records", Accessible node) in the
    /// tree of the window at `root`: never a password field's value.
    pub fn record(&self, root: &str, parent: Option<&str>) -> HashMap<String, OwnedValue> {
        let mut r = HashMap::from([
            ("id".to_owned(), value(node_id(root, self.path.as_str()))),
            ("parent".to_owned(), value(parent.map(|p| node_id(root, p)).unwrap_or_default())),
            ("role".to_owned(), value(role_name(self.role))),
            ("name".to_owned(), value(self.name.as_str())),
            ("description".to_owned(), value(self.description.as_str())),
            ("states".to_owned(), value(state_names(self.states))),
            ("actions".to_owned(), value(self.actions.clone())),
            ("child-count".to_owned(), value(self.children.len() as u32)),
        ]);
        if self.password {
            r.insert("password".to_owned(), value(true));
            return r;
        }
        if let Some((current, min, max)) = self.number {
            r.insert("value".to_owned(), value(current));
            if !min.is_nan() {
                r.insert("min".to_owned(), value(min));
            }
            if !max.is_nan() {
                r.insert("max".to_owned(), value(max));
            }
        } else if let Some(text) = &self.text {
            // Editable texts' contents, and texts that say more than the
            // name (a label's text is its name).
            if self.editable() || (*text != self.name && !text.is_empty()) {
                let cut: String = text.chars().take(TEXT_MAX).collect();
                if self.text_length > TEXT_MAX || cut.chars().count() < text.chars().count() {
                    r.insert("value-truncated".to_owned(), value(true));
                }
                r.insert("value".to_owned(), value(cut));
            }
        }
        r
    }
}

// ----- Which tree a window is ------------------------------------------------

/// Programs of the shell and the session, whose windows are the shell's.
const SHELL_PROGRAMS: &[&str] = &["helix-shell", "helix-comp", "helix-greeter", "helix-session", "helix-portals"];
/// Programs that show the system's password and permission prompts.
const PROMPT_PROGRAMS: &[&str] = &[
    "gcr-prompter",
    "gcr4-prompter",
    "gnome-keyring-daemon",
    "polkit-gnome-authentication-agent-1",
    "polkit-kde-authentication-agent-1",
    "polkit-mate-authentication-agent-1",
    "lxpolkit",
    "lxqt-policykit-agent",
    "xfce-polkit",
    "ksshaskpass",
    "kwalletd5",
    "kwalletd6",
];
const PROMPT_PREFIXES: &[&str] = &["pinentry", "ssh-askpass", "x11-ssh-askpass"];

/// The file name of `pid`'s executable.
fn exe_name(proc_root: &Path, pid: u32) -> Option<String> {
    let exe = std::fs::read_link(proc_root.join(pid.to_string()).join("exe")).ok()?;
    let name = exe.file_name()?.to_string_lossy().trim_end_matches(" (deleted)").to_owned();
    Some(name)
}

/// An app id as systemd escapes it in unit names.
fn escaped(id: &str) -> String {
    id.replace('-', "\\x2d")
}

/// Refuse process `pid` (showing window `window` of `snapshot`) when it is
/// the shell's, Settings', a system prompt's, or an excluded app's: by its
/// program, its other windows, or the systemd unit it runs in.
pub fn check_process(pid: u32, snapshot: &Snapshot, config: &Config, installed: &[Installed], proc_root: &Path) -> Result<(), AgentError> {
    let excluded = |token: &str| Err(AgentError::Excluded(token.to_owned()));
    if pid == 0 {
        return Err(AgentError::NotSupported("the window's process is not known".into()));
    }
    if pid == std::process::id() || (snapshot.shell_pid != 0 && pid == snapshot.shell_pid) {
        return excluded("shell");
    }
    // Every window of the process: one excluded is enough.
    for w in snapshot.windows.iter().filter(|w| w.pid == pid) {
        policy::check_target(config, Target::App(&w.app_id))?;
    }
    let exe = exe_name(proc_root, pid).unwrap_or_default();
    if exe == "helix-settings-app" {
        return excluded("settings");
    }
    if SHELL_PROGRAMS.contains(&exe.as_str()) {
        return excluded("shell");
    }
    if PROMPT_PROGRAMS.contains(&exe.as_str()) || PROMPT_PREFIXES.iter().any(|p| exe.starts_with(p)) {
        return excluded("system-prompt");
    }
    let mut protected: Vec<&str> = config.excluded_apps.iter().map(|a| a.strip_suffix(".desktop").unwrap_or(a)).collect();
    protected.push(policy::SETTINGS_APP_ID);
    let cgroup = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).unwrap_or_default();
    for id in protected {
        let by_program = !exe.is_empty() && installed.iter().any(|i| i.id == id && i.exec.as_deref() == Some(exe.as_str()));
        let by_unit = !id.is_empty() && (cgroup.contains(&format!("-{}-", escaped(id))) || cgroup.contains(&format!("-{}.", escaped(id))));
        if by_program || by_unit {
            return policy::check_target(config, Target::App(id));
        }
    }
    Ok(())
}

/// The process owning `name` on the accessibility bus.
async fn owner_pid(bus: &Connection, name: &str) -> Option<u32> {
    let dbus = zbus::fdo::DBusProxy::new(bus).await.ok()?;
    let bus_name = zbus::names::BusName::try_from(name.to_owned()).ok()?;
    tokio::time::timeout(CALL_TIMEOUT, dbus.get_connection_unix_process_id(bus_name)).await.ok()?.ok()
}

/// A window's tree: its app and the window's own object.
#[derive(Clone)]
pub struct Scope {
    pub app: App,
    pub root: OwnedObjectPath,
}

/// Which accessible object is window `w`'s (see the module documentation);
/// refused rather than guessed when it is not clear.
pub async fn resolve(bus: &Connection, w: &WindowInfo, snapshot: &Snapshot) -> Result<Scope, AgentError> {
    let unclear = |why: &str| AgentError::NotSupported(format!("the window's controls cannot be told apart: {why}"));
    if !w.a11y_name.is_empty() && !w.a11y_path.is_empty() {
        // What the app told the compositor, if the name is the window's
        // process's.
        if owner_pid(bus, &w.a11y_name).await != Some(w.pid) {
            return Err(unclear("the app's accessibility name is not its process's"));
        }
        let root = OwnedObjectPath::try_from(w.a11y_path.clone()).map_err(|_| unclear("a bad object path"))?;
        tracing::debug!(window = w.id, "a window's controls, where its app said");
        return Ok(Scope { app: App { bus: bus.clone(), name: w.a11y_name.clone() }, root });
    }
    let registry = App { bus: bus.clone(), name: REGISTRY.to_owned() };
    let apps: Vec<ObjRef> = registry.call(ROOT_PATH, ACCESSIBLE, "GetChildren", &()).await.map_err(|_| AgentError::NotSupported("no accessibility registry".into()))?;
    let mut mine = Vec::new();
    for a in apps {
        if owner_pid(bus, &a.0).await == Some(w.pid) {
            mine.push(a);
        }
    }
    let app = match mine.as_slice() {
        [] => return Err(AgentError::NotSupported("the window's app has no accessibility tree".into())),
        [one] => App { bus: bus.clone(), name: one.0.clone() },
        _ => return Err(unclear("its process has several")),
    };
    let tops: Vec<ObjRef> = app.call(ROOT_PATH, ACCESSIBLE, "GetChildren", &()).await?;
    let tops: Vec<ObjRef> = tops.into_iter().filter(|t| t.0 == app.name).collect();
    let shared = snapshot.windows.iter().filter(|o| o.pid == w.pid).count() > 1;
    let mut shown = Vec::new();
    for t in &tops {
        let p = t.1.as_str();
        let states: Vec<u32> = app.call(p, ACCESSIBLE, "GetState", &()).await.unwrap_or_default();
        let bits = state_bits(&states);
        if bits & (STATE_SHOWING | STATE_VISIBLE) != 0 {
            shown.push((t.1.clone(), app.text_property(p, "Name").await));
        }
    }
    let root = if shown.len() == 1 && !shared {
        shown.remove(0).0
    } else {
        let titled: Vec<_> = shown.into_iter().filter(|(_, name)| !w.title.is_empty() && *name == w.title).collect();
        match titled.as_slice() {
            [(path, _)] => path.clone(),
            [] => return Err(unclear("none of its app's windows is this one")),
            _ => return Err(unclear("several of its app's windows have this title")),
        }
    };
    tracing::debug!(window = w.id, "a window's controls, by its process");
    Ok(Scope { app, root })
}

// ----- Reading -----------------------------------------------------------------

/// `A11yTree`'s and `A11yFind`'s options.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadOptions {
    /// Levels below the first node (0: that node only).
    pub depth: u32,
    pub max_nodes: u32,
    /// Start at this node (its id).
    pub node: Option<String>,
    pub bounds: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions { depth: DEPTH_MAX, max_nodes: NODES_DEFAULT, node: None, bounds: false }
    }
}

fn opt_u32(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<u32>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => u32::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a number (u)"))),
    }
}

fn opt_text(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<String>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => String::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a string"))),
    }
}

fn opt_bool(options: &HashMap<String, OwnedValue>, key: &str) -> Result<bool, AgentError> {
    match options.get(key) {
        None => Ok(false),
        Some(v) => bool::try_from(v.clone()).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a boolean"))),
    }
}

impl ReadOptions {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> Result<ReadOptions, AgentError> {
        let depth = opt_u32(options, "depth")?.unwrap_or(DEPTH_MAX).min(DEPTH_MAX);
        let max_nodes = match opt_u32(options, "max-nodes")? {
            None => NODES_DEFAULT,
            Some(0) => return Err(AgentError::invalid_args("“max-nodes” must be at least 1")),
            Some(n) => n.min(NODES_MAX),
        };
        let node = opt_text(options, "node")?.filter(|n| !n.is_empty());
        Ok(ReadOptions { depth, max_nodes, node, bounds: opt_bool(options, "bounds")? })
    }
}

/// What `A11yFind` looks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Match {
    pub role: Option<u32>,
    /// Contained in the name, ignoring case.
    pub name: Option<String>,
    /// States the node has (bits).
    pub states: u64,
    pub limit: u32,
}

impl Match {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> Result<Match, AgentError> {
        let role = match opt_text(options, "role")?.filter(|r| !r.is_empty()) {
            None => None,
            Some(r) => Some(role_by_name(&r).ok_or_else(|| AgentError::invalid_args(format!("unknown role “{r}”")))?),
        };
        let mut states = 0;
        if let Some(v) = options.get("states") {
            let names = <Vec<String>>::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“states” must be a list of strings"))?;
            for n in names {
                states |= state_bit(&n).ok_or_else(|| AgentError::invalid_args(format!("unknown state “{n}”")))?;
            }
        }
        let limit = match opt_u32(options, "limit")? {
            None => FIND_DEFAULT,
            Some(0) => return Err(AgentError::invalid_args("“limit” must be at least 1")),
            Some(n) => n.min(FIND_MAX),
        };
        Ok(Match { role, name: opt_text(options, "name")?.filter(|n| !n.is_empty()), states, limit })
    }

    pub fn matches(&self, info: &Info) -> bool {
        self.role.is_none_or(|r| r == info.role)
            && self.name.as_deref().is_none_or(|n| info.name.to_lowercase().contains(&n.to_lowercase()))
            && info.states & self.states == self.states
    }
}

/// A node as read: its parent's path (none for the first) and where it is
/// in its window (with `bounds`).
pub type ReadNode = (Info, Option<OwnedObjectPath>, Option<(i32, i32, i32, i32)>);

/// A tree as read: its nodes in tree order (the first is where reading
/// started), each with its parent's path.
pub struct Tree {
    pub nodes: Vec<ReadNode>,
    /// Some nodes were left out (the node limit, the depth, the time).
    pub truncated: bool,
}

impl Scope {
    /// The node `id` of this window, checked: it is the window or inside
    /// it (walking up from it), and neither it nor what holds it is a
    /// password field. `seen` is told what it is before the checks.
    pub async fn node(&self, id: &str, seen: impl FnOnce(&Info)) -> Result<Info, AgentError> {
        // An object that cannot say what it is or what holds it cannot be
        // shown to be in this window.
        let unknown = |e: AgentError| match e {
            AgentError::NotSupported(_) => AgentError::NotFound(format!("node {id} is not in this window")),
            other => other,
        };
        let path = node_path(self.root.as_str(), id)?;
        let info = self.app.info(&path).await.map_err(unknown)?;
        seen(&info);
        if info.password {
            return Err(AgentError::Excluded("password-field".into()));
        }
        // The object must be at this very path: some toolkits answer for
        // paths they never gave (ATK's bridge reads the number a path
        // starts with), so its parent is asked for it by its place.
        if path != self.root {
            let Some(parent) = self.app.parent(path.as_str()).await.map_err(unknown)? else { return Err(unknown(AgentError::NotSupported(String::new()))) };
            let index: i32 = self.app.call(path.as_str(), ACCESSIBLE, "GetIndexInParent", &()).await.map_err(unknown)?;
            let there: ObjRef = self.app.call(parent.1.as_str(), ACCESSIBLE, "GetChildAtIndex", &(index,)).await.map_err(unknown)?;
            if parent.0 != self.app.name || there.0 != self.app.name || there.1 != path {
                return Err(AgentError::NotFound(format!("node {id} is not in this window")));
            }
        }
        let mut at = path.clone();
        for _ in 0..=DEPTH_MAX {
            if at == self.root {
                return Ok(info);
            }
            let Some(parent) = self.app.parent(at.as_str()).await.map_err(unknown)? else { break };
            if parent.0 != self.app.name {
                break;
            }
            let role: u32 = self.app.call(parent.1.as_str(), ACCESSIBLE, "GetRole", &()).await.map_err(unknown)?;
            if role == ROLE_PASSWORD_TEXT {
                return Err(AgentError::Excluded("password-field".into()));
            }
            at = parent.1;
        }
        Err(AgentError::NotFound(format!("node {id} is not in this window")))
    }

    /// Read the tree from the window (or `options.node`), depth first,
    /// leaving out password fields' insides and other processes' objects.
    pub async fn read(&self, options: &ReadOptions) -> Result<Tree, AgentError> {
        let start = match &options.node {
            Some(id) => self.node(id, |_| {}).await?.path,
            None => self.root.clone(),
        };
        let until = Instant::now() + TREE_BUDGET;
        let mut nodes = Vec::new();
        let mut truncated = false;
        // (path, parent, depth), last first.
        let mut stack = vec![(start, None::<OwnedObjectPath>, 0u32)];
        while let Some((path, parent, depth)) = stack.pop() {
            if nodes.len() as u32 >= options.max_nodes || Instant::now() >= until {
                truncated = true;
                break;
            }
            let info = match self.app.info(&path).await {
                Ok(info) => info,
                // A node that went away while reading.
                Err(AgentError::NotFound(_)) if !nodes.is_empty() => continue,
                Err(err) => return Err(err),
            };
            let bounds = if options.bounds { self.app.extents(path.as_str()).await } else { None };
            // Not into password fields, nor into other processes' objects
            // (their nodes are marked `truncated`).
            if !info.password && depth < options.depth {
                for c in info.children.iter().filter(|c| c.0 == self.app.name).rev() {
                    stack.push((c.1.clone(), Some(path.clone()), depth + 1));
                }
            } else if !info.password && !info.children.is_empty() {
                truncated = true;
            }
            nodes.push((info, parent, bounds));
        }
        Ok(Tree { nodes, truncated })
    }

    /// The records of `tree` (README.md, "Records", Accessible node).
    pub fn records(&self, tree: &Tree) -> Vec<HashMap<String, OwnedValue>> {
        let root = self.root.as_str();
        let read: std::collections::HashSet<&str> = tree.nodes.iter().map(|(i, _, _)| i.path.as_str()).collect();
        tree.nodes
            .iter()
            .enumerate()
            .map(|(n, (info, parent, bounds))| {
                // The first node has no parent in what was read.
                let mut r = info.record(root, if n == 0 { None } else { parent.as_ref().map(|p| p.as_str()) });
                let left_out = !info.password && info.children.iter().any(|c| !read.contains(c.1.as_str()));
                if left_out {
                    r.insert("truncated".to_owned(), value(true));
                }
                if let Some((x, y, w, h)) = bounds.filter(|b| b.2 >= 0 && b.3 >= 0) {
                    for (k, v) in [("x", x), ("y", y), ("width", w), ("height", h)] {
                        r.insert(k.to_owned(), value(v));
                    }
                }
                r
            })
            .collect()
    }

    /// Do `action` on the checked node `node`: one of its own actions,
    /// `default` (its first), `focus`, or `select` (in what holds it).
    pub async fn act(&self, node: &Info, action: &str) -> Result<(), AgentError> {
        let p = node.path.as_str();
        let refused = |what: &str| AgentError::Failed(format!("the app did not {what}"));
        match action {
            "focus" => {
                if !node.has("Component") {
                    return Err(AgentError::NotSupported("this node cannot take the focus".into()));
                }
                let done: bool = self.app.call(p, "org.a11y.atspi.Component", "GrabFocus", &()).await?;
                if done { Ok(()) } else { Err(refused("give it the focus")) }
            }
            "select" if !node.actions.iter().any(|a| a == "select") => {
                let parent = self.app.parent(p).await?.filter(|q| q.0 == self.app.name).ok_or_else(|| AgentError::NotSupported("nothing to select it in".into()))?;
                let index: i32 = self.app.call(p, ACCESSIBLE, "GetIndexInParent", &()).await?;
                let done: bool = self.app.call(parent.1.as_str(), "org.a11y.atspi.Selection", "SelectChild", &(index,)).await?;
                if done { Ok(()) } else { Err(refused("select it")) }
            }
            _ => {
                let index = match action {
                    "default" if !node.actions.iter().any(|a| a == "default") => (!node.actions.is_empty()).then_some(0),
                    _ => node.actions.iter().position(|a| a == action),
                };
                let index = index.ok_or_else(|| AgentError::NotSupported(format!("the node has no action “{action}”")))?;
                let done: bool = self.app.call(p, "org.a11y.atspi.Action", "DoAction", &(index as i32,)).await?;
                if done { Ok(()) } else { Err(refused("do it")) }
            }
        }
    }

    /// Set the checked node's text, or its number.
    pub async fn set(&self, node: &Info, to: &SetTo) -> Result<(), AgentError> {
        let p = node.path.as_str();
        match to {
            SetTo::Text(text) => {
                if !node.editable() {
                    return Err(AgentError::NotSupported("this node's text cannot be changed".into()));
                }
                let done: bool = self.app.call(p, "org.a11y.atspi.EditableText", "SetTextContents", &(text.as_str(),)).await?;
                if done { Ok(()) } else { Err(AgentError::Failed("the app did not change the text".into())) }
            }
            SetTo::Number(n) => {
                let Some((_, min, max)) = node.number else { return Err(AgentError::NotSupported("this node has no number".into())) };
                if (!min.is_nan() && *n < min) || (!max.is_nan() && *n > max) {
                    return Err(AgentError::invalid_args(format!("the value must be from {min} to {max}")));
                }
                let body = ("org.a11y.atspi.Value", "CurrentValue", Value::from(*n));
                let call = self.app.bus.call_method(Some(self.app.name.as_str()), p, Some(PROPERTIES), "Set", &body);
                tokio::time::timeout(CALL_TIMEOUT, call).await.map_err(|_| AgentError::Failed("the app did not answer in time".into()))?.map_err(app_error)?;
                Ok(())
            }
        }
    }
}

/// What `A11ySetValue` sets.
#[derive(Debug, Clone, PartialEq)]
pub enum SetTo {
    Text(String),
    Number(f64),
}

impl SetTo {
    /// The value an agent sent, for node `node`: text for editable texts,
    /// a number (or text that is one) for nodes with a number.
    pub fn of(v: &Value<'_>, node: &Info) -> Result<SetTo, AgentError> {
        let wrong = || AgentError::invalid_args("the value must be a string or a number (d)");
        let number = node.number.is_some();
        match v {
            Value::Str(s) if number && !node.editable() => s.trim().parse::<f64>().map(SetTo::Number).map_err(|_| AgentError::invalid_args("this node takes a number")),
            Value::Str(s) => Ok(SetTo::Text(s.to_string())),
            Value::F64(n) if n.is_finite() => Ok(SetTo::Number(*n)),
            Value::I32(n) => Ok(SetTo::Number(f64::from(*n))),
            Value::U32(n) => Ok(SetTo::Number(f64::from(*n))),
            Value::I64(n) => Ok(SetTo::Number(*n as f64)),
            _ => Err(wrong()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::desktop::WindowInfo;

    #[test]
    fn roles_and_states() {
        assert_eq!(role_name(40), "password text");
        assert_eq!(role_name(43), "button");
        assert_eq!(role_name(130), "switch");
        assert_eq!(role_name(999), "unknown");
        assert_eq!(role_by_name("check box"), Some(7));
        assert_eq!(role_by_name("nonsense"), None);
        assert_eq!(state_names((1 << 4) | (1 << 12) | (1 << 43)), ["checked", "focused", "read-only"]);
        assert_eq!(state_bit("focused"), Some(1 << 12));
        assert_eq!(state_bit("invalid"), None);
        assert_eq!(state_bits(&[1 << 4, 1 << 11]), (1 << 4) | (1 << 43));
    }

    #[test]
    fn password_fields() {
        // GTK 4's password entry, and its entry with hidden characters
        // (plain `text` with mask characters).
        assert!(password_like(ROLE_PASSWORD_TEXT, "Password", true, None));
        assert!(password_like(ROLE_PASSWORD_TEXT, "", false, None), "the role alone");
        assert!(password_like(61, "PIN", true, Some("")), "named so");
        assert!(password_like(61, "", true, Some("••••••")), "masked");
        assert!(password_like(61, "", true, Some("****")), "masked with stars");
        assert!(password_like(61, "Wi-Fi Passphrase", true, Some("")));
        assert!(password_like(61, "Your password", true, Some("")));
        assert!(!password_like(61, "Name field", true, Some("hello")));
        assert!(!password_like(61, "Spinning", true, Some("")), "not a word of it");
        // A label saying "Password" is not a field.
        assert!(!password_like(29, "Password", false, Some("Password")));
        assert!(!masked(""));
        assert!(!masked("•a"));
    }

    #[test]
    fn sensitive_controls() {
        assert!(sensitive_control("Delete All", "", "click"));
        assert!(sensitive_control("Send", "", "click"));
        assert!(sensitive_control("Pay now", "", "click"));
        assert!(sensitive_control("Sign out", "", "click"));
        assert!(sensitive_control("", "Moves the file to the trash", "click"));
        assert!(sensitive_control("Frame", "", "window.close"));
        assert!(!sensitive_control("Press me", "", "click"));
        assert!(!sensitive_control("Sender", "", "click"), "whole words only");
        assert!(!sensitive_control("Reorder", "", "click"));
        assert!(!sensitive_control("Postpone", "", "click"));
    }

    #[test]
    fn clipboard_actions() {
        assert!(clipboard_action("clipboard.paste"));
        assert!(clipboard_action("clipboard.copy"));
        assert!(clipboard_action("Paste"));
        assert!(clipboard_action("edit.cut"));
        assert!(!clipboard_action("click"));
        assert!(!clipboard_action("selection.select-all"));
    }

    #[test]
    fn logged_actions() {
        assert_eq!(log_action("click"), "click");
        assert_eq!(log_action("window.close"), "window.close");
        // An app's own words are not recorded.
        assert_eq!(log_action("Transfer 500 to Bob"), "other");
    }

    #[test]
    fn node_ids() {
        let root = "/org/example/App/a11y/1184df84";
        assert_eq!(node_id(root, "/org/example/App/a11y/e6274e4e"), "e6274e4e");
        assert_eq!(node_id(root, root), "1184df84");
        assert_eq!(node_id(root, "/org/a11y/atspi/accessible/5"), "/org/a11y/atspi/accessible/5");
        assert_eq!(node_path(root, "e6274e4e").unwrap().as_str(), "/org/example/App/a11y/e6274e4e");
        assert_eq!(node_path(root, "/org/a11y/atspi/accessible/5").unwrap().as_str(), "/org/a11y/atspi/accessible/5");
        assert!(node_path(root, "").is_err());
        assert!(node_path(root, "bad path!").is_err());
        assert!(node_path(root, "a/../b").is_err() || node_path(root, "a/../b").is_ok_and(|p| p.as_str().starts_with("/org/example/App/a11y/")));
        // A root at the top: ids are whole paths.
        assert_eq!(node_id("/root", "/root"), "/root");
    }

    fn info(role: u32, name: &str, text: Option<&str>, editable: bool) -> Info {
        let interfaces = if editable { vec!["org.a11y.atspi.Text".to_owned(), "org.a11y.atspi.EditableText".to_owned()] } else { vec!["org.a11y.atspi.Text".to_owned()] };
        let mut i = Info { path: OwnedObjectPath::try_from("/a/b/n1").unwrap(), role, name: name.into(), interfaces, text: text.map(str::to_owned), text_length: text.map_or(0, |t| t.chars().count()), ..Info::default() };
        i.password = password_like(role, name, editable, text);
        if i.password {
            i.text = None;
        }
        i
    }

    #[test]
    fn records_never_hold_passwords() {
        let r = info(ROLE_PASSWORD_TEXT, "Password", None, true).record("/a/b/root", Some("/a/b/root"));
        assert!(!r.contains_key("value"));
        assert!(bool::try_from(r["password"].clone()).unwrap());
        let r = info(61, "PIN", Some("••••"), true).record("/a/b/root", None);
        assert!(!r.contains_key("value"));
        let r = info(61, "Name field", Some("hello"), true).record("/a/b/root", Some("/a/b/root"));
        assert_eq!(String::try_from(r["value"].clone()).unwrap(), "hello");
        assert_eq!(String::try_from(r["id"].clone()).unwrap(), "n1");
        assert_eq!(String::try_from(r["parent"].clone()).unwrap(), "root");
        // A label's text is its name: no value.
        let r = info(29, "Clicks: 0", Some("Clicks: 0"), false).record("/a/b/root", None);
        assert!(!r.contains_key("value"));
        // Long texts are cut.
        let long = "x".repeat(TEXT_MAX + 1);
        let r = info(61, "Doc", Some(&long), true).record("/a/b/root", None);
        assert_eq!(String::try_from(r["value"].clone()).unwrap().chars().count(), TEXT_MAX);
        assert!(bool::try_from(r["value-truncated"].clone()).unwrap());
    }

    #[test]
    fn options() {
        let o = |pairs: &[(&str, Value<'static>)]| pairs.iter().map(|(k, v)| (k.to_string(), OwnedValue::try_from(v.clone()).unwrap())).collect::<HashMap<_, _>>();
        let r = ReadOptions::from_dict(&o(&[("depth", Value::from(2u32)), ("max-nodes", Value::from(99_999u32))])).unwrap();
        assert_eq!((r.depth, r.max_nodes), (2, NODES_MAX));
        assert!(ReadOptions::from_dict(&o(&[("depth", Value::from("deep"))])).is_err());
        assert!(ReadOptions::from_dict(&o(&[("max-nodes", Value::from(0u32))])).is_err());
        let m = Match::from_dict(&o(&[("role", Value::from("button")), ("name", Value::from("press")), ("states", Value::from(vec!["focusable"]))])).unwrap();
        assert_eq!((m.role, m.states, m.limit), (Some(43), 1 << 11, FIND_DEFAULT));
        let mut b = info(43, "Press me", None, false);
        b.states = (1 << 11) | (1 << 24);
        assert!(m.matches(&b));
        b.states = 0;
        assert!(!m.matches(&b));
        assert!(Match::from_dict(&o(&[("role", Value::from("thing"))])).is_err());
        assert!(Match::from_dict(&o(&[("states", Value::from(vec!["sleepy"]))])).is_err());
    }

    #[test]
    fn values_to_set() {
        let text = info(61, "Name", Some("a"), true);
        assert_eq!(SetTo::of(&Value::from("b"), &text).unwrap(), SetTo::Text("b".into()));
        let mut slider = info(51, "Volume", None, false);
        slider.interfaces = vec!["org.a11y.atspi.Value".into()];
        slider.number = Some((1.0, 0.0, 10.0));
        assert_eq!(SetTo::of(&Value::from("4.5"), &slider).unwrap(), SetTo::Number(4.5));
        assert!(SetTo::of(&Value::from("loud"), &slider).is_err());
        assert_eq!(SetTo::of(&Value::from(3.0), &slider).unwrap(), SetTo::Number(3.0));
        assert!(SetTo::of(&Value::from(true), &slider).is_err());
        assert!(SetTo::of(&Value::from(f64::NAN), &slider).is_err());
    }

    fn snap(windows: Vec<WindowInfo>) -> Snapshot {
        Snapshot { windows, ..Default::default() }
    }

    #[test]
    fn protected_processes() {
        let proc_root = std::env::temp_dir().join(format!("helix-a11y-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&proc_root);
        let fake = |pid: u32, exe: &str, cgroup: &str| {
            let dir = proc_root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(format!("/usr/bin/{exe}"), dir.join("exe")).unwrap();
            std::fs::write(dir.join("cgroup"), cgroup).unwrap();
        };
        fake(100, "gnome-text-editor", "0::/user.slice/app-helix-org.gnome.TextEditor-100.scope\n");
        fake(101, "helix-settings-app", "0::/\n");
        fake(102, "pinentry-gnome3", "0::/\n");
        fake(103, "keepassxc", "0::/\n");
        fake(104, "helix-shell", "0::/\n");
        fake(105, "bank-app", "0::/user.slice/app-helix-com.example.Bank-105.scope\n");
        fake(106, "gcr-prompter", "0::/\n");
        let config = Config { enabled: true, excluded_apps: vec!["org.keepassxc.KeePassXC".into(), "com.example.Bank.desktop".into()], ..Config::default() };
        let installed = vec![Installed { id: "org.keepassxc.KeePassXC".into(), name: "KeePassXC".into(), wm_class: None, exec: Some("keepassxc".into()) }];
        let token = |r: Result<(), AgentError>| match r {
            Ok(()) => "ok".to_owned(),
            Err(AgentError::Excluded(t)) => t,
            Err(e) => e.kind().to_owned(),
        };
        let w = |id: u32, app: &str, pid: u32| WindowInfo { id, app_id: app.into(), pid, ..WindowInfo::default() };
        let s = snap(vec![w(1, "org.gnome.TextEditor", 100), w(2, "org.example.Fake", 103)]);
        assert_eq!(token(check_process(100, &s, &config, &installed, &proc_root)), "ok");
        assert_eq!(token(check_process(0, &s, &config, &installed, &proc_root)), "NotSupported");
        assert_eq!(token(check_process(std::process::id(), &s, &config, &installed, &proc_root)), "shell");
        assert_eq!(token(check_process(101, &s, &config, &installed, &proc_root)), "settings");
        assert_eq!(token(check_process(102, &s, &config, &installed, &proc_root)), "system-prompt");
        assert_eq!(token(check_process(106, &s, &config, &installed, &proc_root)), "system-prompt");
        assert_eq!(token(check_process(104, &s, &config, &installed, &proc_root)), "shell");
        // An excluded app's program behind another app id.
        assert_eq!(token(check_process(103, &s, &config, &installed, &proc_root)), "app:org.keepassxc.KeePassXC");
        // By its systemd unit.
        assert_eq!(token(check_process(105, &s, &config, &installed, &proc_root)), "app:com.example.Bank");
        // Another window of the same process is excluded.
        let s2 = snap(vec![w(1, "org.gnome.TextEditor", 100), w(3, "org.helixdesktop.Settings", 100)]);
        assert_eq!(token(check_process(100, &s2, &config, &installed, &proc_root)), "settings");
        let s3 = Snapshot { shell_pid: 100, ..s.clone() };
        assert_eq!(token(check_process(100, &s3, &config, &installed, &proc_root)), "shell");
        let _ = std::fs::remove_dir_all(&proc_root);
    }

    #[test]
    fn words() {
        assert!(has_word("delete all", "delete"));
        assert!(has_word("x: delete", "delete"));
        assert!(!has_word("undeleted", "delete"));
        assert!(has_word("sign out now", "sign out"));
        assert!(has_word("削除する", "削除"));
    }
}
