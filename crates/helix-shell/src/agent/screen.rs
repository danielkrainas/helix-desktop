//! The *Screen and input* methods (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Screen and input methods";
//! helixdesktop/desktop#150): `Screenshot` and `Input`.
//!
//! The compositor draws the screenshot and fills (blacks out) every window
//! and shell surface the shell did not clear: the shell sends the windows
//! that may be shown and the shell surfaces (layer namespaces) that may,
//! so a window that appears in between is filled, never shown. Protected
//! are excluded apps', Settings', the shell's and the system prompts'
//! windows, and windows of any process that shows one of those; of the
//! shell, everything but the wallpaper, the desktop icons, the top bar,
//! the dock and the on-screen displays (prompts, dialogs, notifications,
//! the overview, the launcher, menus, the on-screen keyboard).
//!
//! Input is a list of events, turned here into the compositor's steps
//! (`helix_agent_input_v1`) and sent one at a time; the compositor checks
//! focus, lock and prompts before each, the shell the grant and system
//! prompts. Nothing typed is ever logged: counts and lengths only.

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use zbus::zvariant::{OwnedValue, Value};

use super::{value, AgentError};

/// The layer surfaces (by namespace) a screenshot shows; every other is
/// filled.
pub const SHOWN_LAYERS: &[&str] = &[
    "helix-background",
    "helix-background-icons",
    "helix-panel",
    "helix-panel-tooltip",
    "helix-dock",
    "helix-dock-edge",
    "helix-osd",
    "helix-workspace-switch",
    "helix-monitor-label",
    "helix-click-type",
];

/// The most a PNG screenshot may be, in bytes.
pub const MAX_PNG: usize = 64 << 20;
/// The most events one `Input` call takes.
pub const MAX_EVENTS: usize = 500;
/// The most characters one `Input` call types, in all.
pub const MAX_TEXT: usize = 10_000;
/// The longest `wait` event.
pub const MAX_WAIT_MS: u32 = 5_000;
/// The most clicks one `click` event makes.
pub const MAX_CLICKS: u32 = 3;
/// The most wheel steps one `scroll` event makes either way.
pub const MAX_STEPS: i32 = 100;
/// How long the indicator shows an agent as controlling the screen after
/// its last screenshot or input.
pub const CONTROL_SHOWN: Duration = Duration::from_secs(3);

/// What `Screenshot` captures.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShotOptions {
    /// A window id.
    pub window: Option<u32>,
    /// A monitor's connector.
    pub monitor: Option<String>,
    /// x, y, width, height: in the window, the monitor, or the layout.
    pub area: Option<(i32, i32, i32, i32)>,
}

fn int(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<i32>, AgentError> {
    let Some(v) = options.get(key) else { return Ok(None) };
    if let Ok(n) = i32::try_from(v.clone()) {
        return Ok(Some(n));
    }
    if let Ok(n) = u32::try_from(v.clone()) {
        return i32::try_from(n).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” is too large")));
    }
    if let Ok(n) = f64::try_from(v.clone()) {
        if n.is_finite() && n.abs() < 1e6 {
            return Ok(Some(n.round() as i32));
        }
    }
    Err(AgentError::invalid_args(format!("“{key}” must be a number (i)")))
}

fn number(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<f64>, AgentError> {
    let Some(v) = options.get(key) else { return Ok(None) };
    if let Ok(n) = f64::try_from(v.clone()) {
        return if n.is_finite() && n.abs() < 1e6 { Ok(Some(n)) } else { Err(AgentError::invalid_args(format!("“{key}” is out of range"))) };
    }
    int(options, key).map(|n| n.map(f64::from))
}

fn text(options: &HashMap<String, OwnedValue>, key: &str) -> Result<Option<String>, AgentError> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => String::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args(format!("“{key}” must be a string"))),
    }
}

fn window_opt(options: &HashMap<String, OwnedValue>) -> Result<Option<u32>, AgentError> {
    match options.get("window") {
        None => Ok(None),
        Some(v) => match u32::try_from(v.clone()) {
            Ok(0) => Ok(None),
            Ok(n) => Ok(Some(n)),
            Err(_) => Err(AgentError::invalid_args("“window” must be a number (u)")),
        },
    }
}

impl ShotOptions {
    pub fn from_dict(options: &HashMap<String, OwnedValue>) -> Result<ShotOptions, AgentError> {
        let window = window_opt(options)?;
        let monitor = text(options, "monitor")?.filter(|m| !m.is_empty());
        if window.is_some() && monitor.is_some() {
            return Err(AgentError::invalid_args("“window” and “monitor” cannot both be given"));
        }
        let parts = ["x", "y", "width", "height"].map(|k| int(options, k));
        let [x, y, w, h] = parts;
        let area = match (x?, y?, w?, h?) {
            (None, None, None, None) => None,
            (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Some((x, y, w, h)),
            (Some(_), Some(_), Some(_), Some(_)) => return Err(AgentError::invalid_args("the area's width and height must be at least 1")),
            _ => return Err(AgentError::invalid_args("an area needs all of “x”, “y”, “width” and “height”")),
        };
        Ok(ShotOptions { window, monitor, area })
    }

    /// The log's `scope`.
    pub fn scope(&self) -> &'static str {
        match (self.window.is_some(), self.monitor.is_some()) {
            (true, _) => "window",
            (_, true) => "monitor",
            _ => "screen",
        }
    }
}

/// A pointer button by name.
pub fn button_code(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "" | "left" | "1" => Some(0x110),
        "right" | "3" => Some(0x111),
        "middle" | "2" => Some(0x112),
        "back" | "8" => Some(0x116),
        "forward" | "9" => Some(0x115),
        _ => None,
    }
}

/// A modifier's name, as its key's keysym.
fn modifier(name: &str) -> Option<u32> {
    use xkbcommon::xkb::keysyms;
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(keysyms::KEY_Control_L),
        "alt" => Some(keysyms::KEY_Alt_L),
        "shift" => Some(keysyms::KEY_Shift_L),
        "super" | "meta" | "logo" | "win" => Some(keysyms::KEY_Super_L),
        "altgr" => Some(keysyms::KEY_ISO_Level3_Shift),
        _ => None,
    }
}

/// A key by its XKB keysym name (`Return`, `F4`, `a`, `eacute`; any case
/// when it is not one letter), or a common alias.
pub fn keysym(name: &str) -> Option<u32> {
    use xkbcommon::xkb;
    let alias = match name.to_ascii_lowercase().as_str() {
        "enter" => Some("Return"),
        "esc" => Some("Escape"),
        "del" => Some("Delete"),
        "ins" => Some("Insert"),
        "pgup" | "pageup" => Some("Prior"),
        "pgdn" | "pagedown" => Some("Next"),
        "space" => Some("space"),
        "backspace" => Some("BackSpace"),
        "up" => Some("Up"),
        "down" => Some("Down"),
        "left" => Some("Left"),
        "right" => Some("Right"),
        "plus" => Some("plus"),
        _ => None,
    };
    let name = alias.unwrap_or(name);
    if let Some(m) = modifier(name) {
        return Some(m);
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let sym = xkb::utf32_to_keysym(c as u32);
        return (sym.raw() != 0 && !c.is_control()).then_some(sym.raw());
    }
    let exact = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    let sym = if exact.raw() != 0 { exact } else { xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE) };
    (sym.raw() != 0).then_some(sym.raw())
}

/// A key combination (`ctrl+s`, `alt+F4`, `Return`, `ctrl+plus`): the
/// modifiers' keysyms and the key's.
pub fn combo(keys: &str) -> Result<(Vec<u32>, u32), AgentError> {
    let bad = || AgentError::invalid_args("unknown key in a key combination");
    let parts: Vec<&str> = if keys == "+" { vec!["+"] } else { keys.split('+').collect() };
    let (last, mods) = parts.split_last().ok_or_else(bad)?;
    if last.is_empty() {
        return Err(bad());
    }
    let mods = mods.iter().map(|m| modifier(m).ok_or_else(bad)).collect::<Result<Vec<_>, _>>()?;
    Ok((mods, keysym(last).ok_or_else(bad)?))
}

/// One step for the compositor (`helix_agent_input_v1`), or a pause.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// To x, y in the window.
    Motion(f64, f64),
    Button(u32, bool),
    Scroll(i32, i32),
    Key(u32, bool),
    Text(String),
    Wait(u32),
}

impl Step {
    /// Whether it sends keys (checked against password fields).
    pub fn keyboard(&self) -> bool {
        matches!(self, Step::Key(..) | Step::Text(_))
    }
}

/// An `Input` call's steps, and what the log says of them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub events: u64,
    pub moves: u64,
    pub clicks: u64,
    pub buttons: u64,
    pub scrolls: u64,
    /// Key combinations without Ctrl, Alt, Super or AltGr, and key presses.
    pub keys: u64,
    /// Key combinations with Ctrl, Alt, Super or AltGr.
    pub shortcuts: u64,
    pub text_length: u64,
}

/// The keysyms of the modifiers that make a key combination a shortcut.
fn shortcut_modifier(sym: u32) -> bool {
    use xkbcommon::xkb::keysyms;
    [keysyms::KEY_Control_L, keysyms::KEY_Alt_L, keysyms::KEY_Super_L, keysyms::KEY_ISO_Level3_Shift].contains(&sym)
}

/// `Input`'s events as the compositor's steps (README.md, "Screen and
/// input methods"); `InvalidArgs` for anything malformed.
pub fn plan(events: &[HashMap<String, OwnedValue>]) -> Result<Plan, AgentError> {
    if events.is_empty() {
        return Err(AgentError::invalid_args("no events"));
    }
    if events.len() > MAX_EVENTS {
        return Err(AgentError::invalid_args(format!("at most {MAX_EVENTS} events")));
    }
    let mut out = Plan { events: events.len() as u64, ..Plan::default() };
    for e in events {
        let kind = text(e, "type")?.ok_or_else(|| AgentError::invalid_args("an event without “type”"))?;
        let at = match (number(e, "x")?, number(e, "y")?) {
            (Some(x), Some(y)) => Some((x, y)),
            (None, None) => None,
            _ => return Err(AgentError::invalid_args("a position needs both “x” and “y”")),
        };
        if let Some((x, y)) = at {
            if x < 0.0 || y < 0.0 {
                return Err(AgentError::invalid_args("positions are in the window: not negative"));
            }
        }
        match kind.as_str() {
            "move" => {
                let (x, y) = at.ok_or_else(|| AgentError::invalid_args("“move” needs “x” and “y”"))?;
                out.steps.push(Step::Motion(x, y));
                out.moves += 1;
            }
            "click" => {
                let button = button_code(&text(e, "button")?.unwrap_or_default()).ok_or_else(|| AgentError::invalid_args("unknown button"))?;
                let count = match e.get("count") {
                    None => 1,
                    Some(v) => u32::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“count” must be a number (u)"))?,
                };
                if !(1..=MAX_CLICKS).contains(&count) {
                    return Err(AgentError::invalid_args(format!("“count” must be 1 to {MAX_CLICKS}")));
                }
                if let Some((x, y)) = at {
                    out.steps.push(Step::Motion(x, y));
                }
                for _ in 0..count {
                    out.steps.push(Step::Button(button, true));
                    out.steps.push(Step::Button(button, false));
                }
                out.clicks += u64::from(count);
            }
            "down" | "up" => {
                let down = kind == "down";
                match (text(e, "button")?, text(e, "key")?) {
                    (Some(b), None) => {
                        let button = button_code(&b).ok_or_else(|| AgentError::invalid_args("unknown button"))?;
                        if let Some((x, y)) = at {
                            out.steps.push(Step::Motion(x, y));
                        }
                        out.steps.push(Step::Button(button, down));
                        out.buttons += 1;
                    }
                    (None, Some(k)) => {
                        let sym = keysym(&k).ok_or_else(|| AgentError::invalid_args("unknown key"))?;
                        out.steps.push(Step::Key(sym, down));
                        out.keys += 1;
                    }
                    _ => return Err(AgentError::invalid_args("“down” and “up” need one of “button” and “key”")),
                }
            }
            "scroll" => {
                let dx = int(e, "dx")?.unwrap_or(0);
                let dy = int(e, "dy")?.unwrap_or(0);
                if dx == 0 && dy == 0 {
                    return Err(AgentError::invalid_args("“scroll” needs “dx” or “dy”"));
                }
                if dx.abs() > MAX_STEPS || dy.abs() > MAX_STEPS {
                    return Err(AgentError::invalid_args(format!("at most {MAX_STEPS} steps")));
                }
                if let Some((x, y)) = at {
                    out.steps.push(Step::Motion(x, y));
                }
                out.steps.push(Step::Scroll(dx, dy));
                out.scrolls += 1;
            }
            "key" => {
                let keys = text(e, "keys")?.ok_or_else(|| AgentError::invalid_args("“key” needs “keys”"))?;
                let (mods, key) = combo(&keys)?;
                if mods.iter().any(|m| shortcut_modifier(*m)) {
                    out.shortcuts += 1;
                } else {
                    out.keys += 1;
                }
                out.steps.extend(mods.iter().map(|m| Step::Key(*m, true)));
                out.steps.push(Step::Key(key, true));
                out.steps.push(Step::Key(key, false));
                out.steps.extend(mods.iter().rev().map(|m| Step::Key(*m, false)));
            }
            "text" => {
                let t = text(e, "text")?.ok_or_else(|| AgentError::invalid_args("“text” needs “text”"))?;
                if t.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
                    return Err(AgentError::invalid_args("text with control characters"));
                }
                out.text_length += t.chars().count() as u64;
                if out.text_length > MAX_TEXT as u64 {
                    return Err(AgentError::invalid_args(format!("at most {MAX_TEXT} characters of text")));
                }
                if !t.is_empty() {
                    out.steps.push(Step::Text(t));
                }
            }
            "wait" => {
                let ms = match e.get("ms") {
                    Some(v) => u32::try_from(v.clone()).map_err(|_| AgentError::invalid_args("“ms” must be a number (u)"))?,
                    None => return Err(AgentError::invalid_args("“wait” needs “ms”")),
                };
                if ms > MAX_WAIT_MS {
                    return Err(AgentError::invalid_args(format!("“wait” is at most {MAX_WAIT_MS} ms")));
                }
                out.steps.push(Step::Wait(ms));
            }
            _ => return Err(AgentError::NotSupported(format!("event type {kind}"))),
        }
    }
    Ok(out)
}

/// Why the compositor stopped a run (`helix_agent_input_v1.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    Cancelled,
    UserInput,
    Cooldown,
    Locked,
    Prompt,
    Gone,
    Focus,
    Outside,
    NoKey,
    Invalid,
    Password,
    /// The shell or the compositor could not carry it out.
    Failed,
}

impl Stopped {
    pub fn from_wire(n: u32) -> Stopped {
        match n {
            0 => Stopped::Cancelled,
            1 => Stopped::UserInput,
            2 => Stopped::Cooldown,
            3 => Stopped::Locked,
            4 => Stopped::Prompt,
            5 => Stopped::Gone,
            6 => Stopped::Focus,
            7 => Stopped::Outside,
            8 => Stopped::NoKey,
            9 => Stopped::Invalid,
            10 => Stopped::Password,
            _ => Stopped::Failed,
        }
    }

    /// The error the call ends with.
    pub fn error(self, window: u32) -> AgentError {
        match self {
            Stopped::Cancelled => AgentError::Cancelled("stopped".into()),
            Stopped::UserInput => AgentError::Cancelled("user-input".into()),
            Stopped::Cooldown => AgentError::Cancelled("cooldown".into()),
            Stopped::Locked => AgentError::Locked("the screen is locked".into()),
            Stopped::Prompt => AgentError::Excluded("system-prompt".into()),
            Stopped::Gone => AgentError::NotFound(format!("window {window}")),
            Stopped::Focus => AgentError::Failed("the window lost the keyboard focus".into()),
            Stopped::Outside => AgentError::Failed("the point is outside the window, or something else is over it there".into()),
            Stopped::NoKey => AgentError::NotSupported("the keyboard layout has no such key".into()),
            Stopped::Password => AgentError::Excluded("password-field".into()),
            Stopped::Invalid => AgentError::Failed("the compositor refused the input".into()),
            Stopped::Failed => AgentError::Failed("the input could not be sent".into()),
        }
    }
}

/// A capture's result as the compositor gave it: the PNG's file, the
/// rectangle (logical: in the layout, or in the window) and how many
/// windows and surfaces were filled.
#[derive(Debug)]
pub struct Shot {
    pub file: File,
    pub rect: (i32, i32, i32, i32),
    pub filled: u32,
}

/// Why a capture failed (`helix_screenshot_frame_v1.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotError {
    Locked,
    NoWindow,
    Invalid,
    Failed,
}

/// A request for the UI.
#[derive(Debug)]
pub enum Request {
    /// A screenshot: `window` (0: the layout), the area (in the window
    /// or the layout), what may be shown.
    Capture { window: u32, area: Option<(i32, i32, i32, i32)>, shown: Vec<u32>, layers: Vec<String>, reply: oneshot::Sender<Result<Shot, ShotError>> },
    /// Start a run into `window` for `agent`: its id.
    Start { agent: String, window: u32, reply: oneshot::Sender<Result<u64, String>> },
    /// One step of run `run`: done, or why it stopped.
    Step { run: u64, step: Step, reply: oneshot::Sender<Result<(), Stopped>> },
    /// Stop run `run` now.
    Cancel { run: u64 },
    /// End run `run` (letting go of what it holds).
    End { run: u64 },
}

/// A [`Request`] that can travel in a `UiEvent`.
#[derive(Debug, Clone)]
pub struct ScreenSlot(Arc<Mutex<Option<Request>>>);

impl ScreenSlot {
    pub fn new(request: Request) -> Self {
        Self(Arc::new(Mutex::new(Some(request))))
    }

    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for ScreenSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A PNG's width and height in pixels, from its header.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// The Image record (README.md, "Records").
pub fn record(png: Vec<u8>, rect: (i32, i32, i32, i32), filled: u32) -> Result<HashMap<String, OwnedValue>, AgentError> {
    let (w, h) = png_size(&png).ok_or_else(|| AgentError::Failed("the compositor's image is not a PNG".into()))?;
    let scale = if rect.2 > 0 { f64::from(w) / f64::from(rect.2) } else { 1.0 };
    Ok(HashMap::from([
        ("png".to_owned(), OwnedValue::try_from(Value::from(png)).expect("bytes hold no file descriptors")),
        ("width".to_owned(), value(w as i32)),
        ("height".to_owned(), value(h as i32)),
        ("scale".to_owned(), value(scale)),
        ("x".to_owned(), value(rect.0)),
        ("y".to_owned(), value(rect.1)),
        ("logical-width".to_owned(), value(rect.2)),
        ("logical-height".to_owned(), value(rect.3)),
        ("filled".to_owned(), value(filled)),
    ]))
}

/// Read a capture's PNG (at most [`MAX_PNG`]).
pub fn read_png(file: File) -> Result<Vec<u8>, AgentError> {
    use std::io::{Read, Seek};
    let mut file = file;
    file.seek(std::io::SeekFrom::Start(0)).map_err(|e| AgentError::Failed(e.to_string()))?;
    let mut png = Vec::new();
    file.take(MAX_PNG as u64 + 1).read_to_end(&mut png).map_err(|e| AgentError::Failed(e.to_string()))?;
    if png.len() > MAX_PNG {
        return Err(AgentError::Failed(format!("the screenshot is larger than {} MiB; take a monitor, a window or an area", MAX_PNG >> 20)));
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(pairs: &[(&str, Value<'static>)]) -> HashMap<String, OwnedValue> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.clone()).unwrap())).collect()
    }

    #[test]
    fn key_names() {
        use xkbcommon::xkb::keysyms;
        assert_eq!(keysym("Return"), Some(keysyms::KEY_Return));
        assert_eq!(keysym("enter"), Some(keysyms::KEY_Return));
        assert_eq!(keysym("f4"), Some(keysyms::KEY_F4));
        assert_eq!(keysym("a"), Some(keysyms::KEY_a));
        assert_eq!(keysym("A"), Some(keysyms::KEY_A));
        assert_eq!(keysym("é"), Some(keysyms::KEY_eacute));
        assert_eq!(keysym("nonsense-key"), None);
        assert_eq!(combo("ctrl+s").unwrap(), (vec![keysyms::KEY_Control_L], keysyms::KEY_s));
        assert_eq!(combo("Ctrl+Shift+T").unwrap(), (vec![keysyms::KEY_Control_L, keysyms::KEY_Shift_L], keysyms::KEY_T));
        assert_eq!(combo("ctrl+plus").unwrap().1, keysyms::KEY_plus);
        assert_eq!(combo("+").unwrap(), (vec![], keysyms::KEY_plus));
        assert!(combo("ctrl+").is_err());
        assert!(combo("hyper+a").is_err());
    }

    #[test]
    fn plans() {
        let events = [
            ev(&[("type", Value::from("click")), ("x", Value::from(10i32)), ("y", Value::from(20.5f64)), ("count", Value::from(2u32))]),
            ev(&[("type", Value::from("key")), ("keys", Value::from("ctrl+s"))]),
            ev(&[("type", Value::from("key")), ("keys", Value::from("Return"))]),
            ev(&[("type", Value::from("text")), ("text", Value::from("héllo\n"))]),
            ev(&[("type", Value::from("scroll")), ("dy", Value::from(-3i32))]),
            ev(&[("type", Value::from("down")), ("button", Value::from("right"))]),
            ev(&[("type", Value::from("up")), ("key", Value::from("shift"))]),
            ev(&[("type", Value::from("wait")), ("ms", Value::from(100u32))]),
        ];
        let p = plan(&events).unwrap();
        assert_eq!(p.steps[0], Step::Motion(10.0, 20.5));
        assert_eq!(&p.steps[1..5], &[Step::Button(0x110, true), Step::Button(0x110, false), Step::Button(0x110, true), Step::Button(0x110, false)]);
        assert_eq!((p.events, p.clicks, p.shortcuts, p.keys, p.text_length, p.scrolls, p.buttons), (8, 2, 1, 2, 6, 1, 1));
        assert!(p.steps.contains(&Step::Text("héllo\n".into())));
        assert!(p.steps.contains(&Step::Scroll(0, -3)));
        assert!(p.steps.contains(&Step::Wait(100)));
        let bad = |pairs: &[(&str, Value<'static>)]| plan(&[ev(pairs)]).unwrap_err();
        assert!(matches!(bad(&[("type", Value::from("move")), ("x", Value::from(1i32))]), AgentError::InvalidArgs(_)));
        assert!(matches!(bad(&[("type", Value::from("move")), ("x", Value::from(-1i32)), ("y", Value::from(1i32))]), AgentError::InvalidArgs(_)));
        assert!(matches!(bad(&[("type", Value::from("text")), ("text", Value::from("bell\u{7}"))]), AgentError::InvalidArgs(_)));
        assert!(matches!(bad(&[("type", Value::from("wait")), ("ms", Value::from(60_000u32))]), AgentError::InvalidArgs(_)));
        assert!(matches!(bad(&[("type", Value::from("click")), ("count", Value::from(9u32))]), AgentError::InvalidArgs(_)));
        assert!(matches!(bad(&[("type", Value::from("teleport"))]), AgentError::NotSupported(_)));
        assert!(matches!(plan(&[]), Err(AgentError::InvalidArgs(_))));
    }

    #[test]
    fn shot_options() {
        let o = ShotOptions::from_dict(&ev(&[("monitor", Value::from("DP-1")), ("x", Value::from(1i32)), ("y", Value::from(2i32)), ("width", Value::from(3i32)), ("height", Value::from(4i32))])).unwrap();
        assert_eq!((o.monitor.as_deref(), o.area, o.scope()), (Some("DP-1"), Some((1, 2, 3, 4)), "monitor"));
        assert!(ShotOptions::from_dict(&ev(&[("window", Value::from(3u32)), ("monitor", Value::from("DP-1"))])).is_err());
        assert!(ShotOptions::from_dict(&ev(&[("x", Value::from(1i32))])).is_err());
        assert!(ShotOptions::from_dict(&ev(&[("x", Value::from(1i32)), ("y", Value::from(1i32)), ("width", Value::from(0i32)), ("height", Value::from(1i32))])).is_err());
        assert_eq!(ShotOptions::from_dict(&HashMap::new()).unwrap().scope(), "screen");
    }

    #[test]
    fn png_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(png_size(b"GIF89a"), None);
        let r = record(png, (10, 20, 320, 240), 2).unwrap();
        assert_eq!(f64::try_from(r["scale"].clone()).unwrap(), 2.0);
        assert_eq!(u32::try_from(r["filled"].clone()).unwrap(), 2);
    }
}
