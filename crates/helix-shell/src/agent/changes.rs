//! Changes agents make to settings, and putting them back
//! (helixdesktop/spec `contracts/agent-access/README.md`, "Changing
//! settings" and "Changes, undo and restoring"; helixdesktop/desktop#157).
//!
//! A *change* is a `SetWallpaper`, `SetAccent`, `SetPowerMode`,
//! `SetToggle`, `StarFile` or `SetFavorite` call that changed something.
//! Each has an id and its old and new values, kept in
//! `$XDG_STATE_HOME/helix/agent-changes.json` ([`Changes`]). A change with
//! `until` has a pending restore: the shell puts the old value back when
//! it comes, when the user stops the agent, when the shell stops, or at
//! the next start after a crash. Only a setting's latest change can be
//! undone or restored, and only while the setting still has the value the
//! agent set: the user's later choice wins.
//!
//! Reading and setting go through the same code as Settings and quick
//! settings ([`AgentAccess::current`], [`AgentAccess::apply`]), and every
//! change, restore and undo is serialized, so a restore never races the
//! change that supersedes it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::MutexGuard;
use std::time::Duration;

use helix_i18n::{tr, trf};
use serde::{Deserialize, Serialize};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use super::actions::Toggle;
use super::policy::{AgentError, Config, Level};
use super::{activity, now, value, AgentAccess, AgentAccessSettings, Caller, OBJECT_PATH};

/// A record (`a{sv}`).
pub type Record = HashMap<String, OwnedValue>;

/// *Helix's choice* (README.md, "Changes, undo and restoring"): how far
/// ahead `until` may be, in seconds. The lower bound keeps a restore from
/// racing the change; the upper keeps a forgotten one from outliving the
/// day.
pub const UNTIL_MIN: i64 = 10;
pub const UNTIL_MAX: i64 = 24 * 60 * 60;

/// Changes kept at most (the oldest go first).
pub const KEEP_MAX: usize = 500;
/// *Helix's choice*: changes an agent may make with one method in any
/// minute (README.md, "Changing settings", "Limits").
pub const CHANGES_PER_MINUTE: usize = 10;
/// The longest the restores' timer sleeps.
pub const RESTORE_POLL: Duration = Duration::from_secs(30);
/// How often listed changes are looked at for settings that changed
/// under them (those not watched through GSettings).
const LIST_POLL: Duration = Duration::from_secs(15);

/// The accent colors Settings' Appearance offers, as `accent-color`
/// names them.
pub const ACCENTS: &[&str] = &["blue", "teal", "green", "yellow", "orange", "red", "pink", "purple", "slate"];
/// The power profiles `SetPowerMode` knows.
pub const PROFILES: &[&str] = &["power-saver", "balanced", "performance"];
/// `picture-options` values `fit` may be.
pub const FITS: &[&str] = &["zoom", "scaled", "centered", "stretched", "wallpaper", "spanned"];

const BACKGROUND: &str = "org.gnome.desktop.background";
const INTERFACE: &str = "org.gnome.desktop.interface";
const NOTIFICATIONS: &str = "org.gnome.desktop.notifications";
const COLOR: &str = "org.gnome.settings-daemon.plugins.color";
const SHELL: &str = "org.gnome.shell";

/// `until` (`x`, Unix seconds) from `options`, unchecked.
pub fn until_option(options: &HashMap<String, OwnedValue>) -> Result<Option<i64>, AgentError> {
    match options.get("until") {
        None => Ok(None),
        Some(v) => i64::try_from(v.clone()).map(Some).map_err(|_| AgentError::invalid_args("“until” must be Unix seconds (x)")),
    }
}

/// `until` checked against `now`: [`UNTIL_MIN`] to [`UNTIL_MAX`] ahead.
pub fn check_until(until: i64, now: i64) -> Result<i64, AgentError> {
    if until < now + UNTIL_MIN || until > now + UNTIL_MAX {
        return Err(AgentError::invalid_args(format!("“until” must be {UNTIL_MIN} seconds to {} hours ahead", UNTIL_MAX / 3600)));
    }
    Ok(until)
}

/// A setting's value: a name or URI, or on and off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Val {
    Bool(bool),
    Text(String),
}

impl Val {
    /// As the activity log has it: `on` and `off` for switches.
    pub fn text(&self) -> String {
        match self {
            Val::Bool(true) => "on".to_owned(),
            Val::Bool(false) => "off".to_owned(),
            Val::Text(t) => t.clone(),
        }
    }

    fn owned(&self) -> OwnedValue {
        match self {
            Val::Bool(b) => value(*b),
            Val::Text(t) => value(t.as_str()),
        }
    }
}

/// What one setting holds: the value, and for the background also the
/// dark style's picture and the placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Values {
    pub value: Val,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
}

impl Values {
    pub fn of(value: Val) -> Values {
        Values { value, dark: None, fit: None }
    }

    pub fn flag(on: bool) -> Values {
        Values::of(Val::Bool(on))
    }

    pub fn text(text: impl Into<String>) -> Values {
        Values::of(Val::Text(text.into()))
    }
}

/// One setting (README.md, "What is one setting").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    /// `picture-uri`, `picture-uri-dark` and `picture-options` together.
    Wallpaper,
    Accent,
    PowerMode,
    Toggle(Toggle),
    /// A file's star, by its URI.
    Star(String),
    /// An app in the dock's favorites, by its id (without `.desktop`).
    Favorite(String),
}

impl Setting {
    /// The Change record's `name`; a toggle's name for toggles.
    pub fn name(&self) -> &'static str {
        match self {
            Setting::Wallpaper => "wallpaper",
            Setting::Accent => "accent",
            Setting::PowerMode => "power-mode",
            Setting::Toggle(t) => t.name(),
            Setting::Star(_) => "star",
            Setting::Favorite(_) => "favorite",
        }
    }

    /// The Change record's `target` ("" for none).
    pub fn target(&self) -> &str {
        match self {
            Setting::Star(t) | Setting::Favorite(t) => t,
            _ => "",
        }
    }

    pub fn parse(name: &str, target: &str) -> Option<Setting> {
        Some(match name {
            "wallpaper" => Setting::Wallpaper,
            "accent" => Setting::Accent,
            "power-mode" => Setting::PowerMode,
            "star" => Setting::Star(target.to_owned()),
            "favorite" => Setting::Favorite(target.to_owned()),
            other => Setting::Toggle(Toggle::from_name(other)?),
        })
    }

    /// The activity log's `target`: the setting's name, a starred file's
    /// URI, a favorite's app id.
    pub fn log_target(&self) -> String {
        match self {
            Setting::Star(t) | Setting::Favorite(t) => t.clone(),
            other => other.name().to_owned(),
        }
    }
}

/// Which parts of a setting a change wrote (the background's variants and
/// placement; everything else has one part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub value: bool,
    pub dark: bool,
    pub fit: bool,
}

impl Parts {
    pub const ONE: Parts = Parts { value: true, dark: false, fit: false };

    /// What `SetWallpaper` with `variant` wrote.
    pub fn wallpaper(variant: &str, fit_changed: bool) -> Parts {
        Parts { value: variant != "dark", dark: variant != "light", fit: fit_changed }
    }

    /// Whether `a` and `b` agree on these parts.
    pub fn same(self, a: &Values, b: &Values) -> bool {
        (!self.value || a.value == b.value) && (!self.dark || a.dark == b.dark) && (!self.fit || a.fit == b.fit)
    }
}

/// A change, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// 16 hexadecimal digits.
    pub id: String,
    /// The agent's id, executable and display name.
    pub agent: String,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub name: String,
    pub time: i64,
    /// [`Setting::name`] and [`Setting::target`].
    pub setting: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// `SetWallpaper`'s `variant`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub old: Values,
    pub new: Values,
    /// A pending restore: when, and what is put back (the first change's
    /// `old` when changes of the setting with `until` followed one
    /// another).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back: Option<Values>,
    /// Undone or restored (or the restore was skipped).
    #[serde(default)]
    pub done: bool,
}

impl Change {
    pub fn setting(&self) -> Option<Setting> {
        Setting::parse(&self.setting, &self.target)
    }

    /// What the change wrote.
    pub fn parts(&self) -> Parts {
        match &self.variant {
            Some(variant) => Parts::wallpaper(variant, self.old.fit != self.new.fit),
            None => Parts::ONE,
        }
    }

    pub fn pending(&self) -> bool {
        !self.done && self.until.is_some()
    }

    fn same_setting(&self, other: &Change) -> bool {
        self.setting == other.setting && self.target == other.target
    }

    /// The Change record a method returns (README.md, "Records").
    pub fn record(&self) -> Record {
        let mut r = values_record(&self.setting, &self.target, &self.old, &self.new);
        r.insert("changed".to_owned(), value(true));
        r.insert("id".to_owned(), value(self.id.as_str()));
        if let Some(until) = self.until {
            r.insert("until".to_owned(), value(until));
        }
        r
    }

    /// `AgentAccess1.ListChanges`' record.
    pub fn listed(&self) -> Record {
        let mut r = values_record(&self.setting, &self.target, &self.old, &self.new);
        r.remove("name");
        r.insert("id".to_owned(), value(self.id.as_str()));
        r.insert("agent".to_owned(), value(self.agent.as_str()));
        r.insert("executable".to_owned(), value(self.executable.as_str()));
        r.insert("name".to_owned(), value(self.name.as_str()));
        r.insert("time".to_owned(), value(self.time));
        r.insert("setting".to_owned(), value(self.setting.as_str()));
        r.insert("target".to_owned(), value(self.target.as_str()));
        if let (Some(until), false) = (self.until, self.done) {
            r.insert("until".to_owned(), value(until));
        }
        r
    }
}

/// The values part of a Change record: `name`, `target` (stars and
/// favorites), `old`, `new`, and for the background `old-dark`,
/// `new-dark`, `old-fit`, `new-fit`.
fn values_record(setting: &str, target: &str, old: &Values, new: &Values) -> Record {
    let mut r = Record::from([("name".to_owned(), value(setting)), ("old".to_owned(), old.value.owned()), ("new".to_owned(), new.value.owned())]);
    if !target.is_empty() {
        r.insert("target".to_owned(), value(target));
    }
    if setting == "wallpaper" {
        for (key, v) in [("old-dark", &old.dark), ("new-dark", &new.dark), ("old-fit", &old.fit), ("new-fit", &new.fit)] {
            r.insert(key.to_owned(), value(v.as_deref().unwrap_or("")));
        }
    }
    r
}

/// A call that found the value already set: `changed` false, its name,
/// target, old and new.
pub fn unchanged_record(setting: &Setting, old: &Values, new: &Values) -> Record {
    let mut r = values_record(setting.name(), setting.target(), old, new);
    r.insert("changed".to_owned(), value(false));
    r
}

/// The activity log's `old`, `new` (and the background's) of going from
/// `from` to `to`.
pub fn log_values(setting: &str, from: &Values, to: &Values) -> Vec<(&'static str, activity::Detail)> {
    let mut out = vec![("old", activity::Detail::Text(from.value.text())), ("new", activity::Detail::Text(to.value.text()))];
    if setting == "wallpaper" {
        let text = |v: &Option<String>| activity::Detail::Text(v.clone().unwrap_or_default());
        out.extend([("old-dark", text(&from.dark)), ("new-dark", text(&to.dark)), ("old-fit", text(&from.fit)), ("new-fit", text(&to.fit))]);
    }
    out
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    changes: Vec<Change>,
}

/// `$XDG_STATE_HOME/helix/agent-changes.json`.
pub fn default_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| gio::glib::home_dir().join(".local/state"));
    state.join("helix/agent-changes.json")
}

/// The changes, oldest first, kept in a file (0600, written whole through
/// a temporary file, never through a symlink, and a rename) so pending
/// restores outlive the shell.
#[derive(Debug, Default)]
pub struct Changes {
    path: Option<PathBuf>,
    list: Vec<Change>,
}

impl Changes {
    /// Read them from `path` (`None`: memory only).
    pub fn load(path: Option<PathBuf>) -> Changes {
        let list = path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| match serde_json::from_str::<File>(&text) {
            Ok(file) => Some(file.changes),
            Err(err) => {
                tracing::warn!(%err, "cannot read the agents' changes");
                None
            }
        });
        let mut list = list.unwrap_or_default();
        list.retain(|c| !c.id.is_empty() && !c.agent.is_empty() && c.setting().is_some());
        list.sort_by_key(|c| c.time);
        Changes { path, list }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let file = File { version: 1, changes: self.list.clone() };
        let text = serde_json::to_string_pretty(&file).unwrap_or_default() + "\n";
        if let Err(err) = super::grants::write_private(path, &text) {
            tracing::warn!(%err, path = %path.display(), "cannot keep the agents' changes");
        }
    }

    /// Add `change` (given a new id): it supersedes the setting's earlier
    /// change, whose pending restore it replaces. With `until`, the
    /// restore puts back the first pending change's `old`.
    pub fn add(&mut self, mut change: Change) -> Change {
        let mut id = super::talk::new_id();
        while self.list.iter().any(|c| c.id == id) {
            id = super::talk::new_id();
        }
        change.id = id;
        change.back = None;
        if let Some(prev) = self.list.iter_mut().rev().find(|c| c.same_setting(&change) && !c.done) {
            if change.until.is_some() && prev.pending() {
                change.back = prev.back.clone();
            }
            prev.until = None;
            prev.back = None;
        }
        if change.until.is_some() && change.back.is_none() {
            change.back = Some(change.old.clone());
        }
        self.list.push(change.clone());
        while self.list.len() > KEEP_MAX {
            let Some(at) = self.list.iter().position(|c| !c.pending()) else { break };
            self.list.remove(at);
        }
        self.save();
        change
    }

    pub fn get(&self, id: &str) -> Option<&Change> {
        self.list.iter().find(|c| c.id == id)
    }

    /// Change `id` when it can still be undone or restored: not done, and
    /// the latest of its setting.
    pub fn latest(&self, id: &str) -> Option<&Change> {
        let at = self.list.iter().position(|c| c.id == id)?;
        let c = &self.list[at];
        let newer = self.list[at + 1..].iter().any(|o| o.same_setting(c));
        (!c.done && !newer).then_some(c)
    }

    /// The changes that are their setting's latest and not done, newest
    /// first.
    pub fn open(&self) -> Vec<Change> {
        let mut seen: Vec<&Change> = Vec::new();
        let mut out = Vec::new();
        for c in self.list.iter().rev() {
            if seen.iter().any(|o| o.same_setting(c)) {
                continue;
            }
            seen.push(c);
            if !c.done {
                out.push(c.clone());
            }
        }
        out
    }

    /// The pending restores (of `agent`, or everyone's), oldest first.
    pub fn pending(&self, agent: Option<&str>) -> Vec<Change> {
        self.list.iter().filter(|c| c.pending() && agent.is_none_or(|a| c.agent == a)).cloned().collect()
    }

    /// When the next restore is due.
    pub fn next_due(&self) -> Option<i64> {
        self.list.iter().filter(|c| c.pending()).filter_map(|c| c.until).min()
    }

    /// Change `id` is done with (undone, restored or skipped).
    pub fn finish(&mut self, id: &str) {
        if let Some(c) = self.list.iter_mut().find(|c| c.id == id) {
            c.done = true;
            c.until = None;
            c.back = None;
            self.save();
        }
    }

    /// Drop changes made before `oldest` (pending restores are kept).
    pub fn prune(&mut self, oldest: i64) -> bool {
        let before = self.list.len();
        self.list.retain(|c| c.time >= oldest || c.pending());
        let changed = self.list.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    /// `ClearLog`: every change goes but those with a pending restore.
    pub fn clear(&mut self) {
        self.list.retain(Change::pending);
        self.save();
    }
}

/// Why a restore was done (`cause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Its `until` came.
    Expired,
    /// *Stop*, Settings' `Revoke`, `Forget` or `StopAll`, access turned
    /// off or lowered below *Control the desktop*.
    Stopped,
    /// The shell stopped, or restored it at its next start.
    SessionEnded,
    /// The indicator's row.
    RestoreNow,
}

impl Cause {
    pub fn token(self) -> &'static str {
        match self {
            Cause::Expired => "expired",
            Cause::Stopped => "stopped",
            Cause::SessionEnded => "session-ended",
            Cause::RestoreNow => "restore-now",
        }
    }
}

/// Why a restore or an undo was not done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// No such change that can still be undone.
    NotFound,
    /// The setting no longer has the agent's value.
    Changed,
    /// The old value cannot be set any more.
    Unavailable,
}

/// The rows the indicator shows under an agent: a pending restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: String,
    /// [`Setting::name`].
    pub setting: String,
    /// The value it goes back to ([`Val::text`]).
    pub back: String,
    /// When (Unix seconds).
    pub at: i64,
}

/// A setting as Settings and quick settings name it.
fn setting_label(setting: &str) -> String {
    match setting {
        "wallpaper" => tr("Background"),
        "accent" => tr("Accent Color"),
        "power-mode" => tr("Power Mode"),
        "dark-style" => tr("Dark Style"),
        "do-not-disturb" => tr("Do Not Disturb"),
        "night-light" => tr("Night Light"),
        "wifi" => tr("Wi-Fi"),
        "airplane-mode" => tr("Airplane Mode"),
        "mute" => tr("Mute"),
        other => other,
    }
    .to_owned()
}

/// A value as Settings shows it.
fn value_label(setting: &str, value: &str) -> String {
    if setting == "wallpaper" {
        return tr("the previous picture").to_owned();
    }
    match value {
        "on" => tr("on"),
        "off" => tr("off"),
        "blue" => tr("Blue"),
        "teal" => tr("Teal"),
        "green" => tr("Green"),
        "yellow" => tr("Yellow"),
        "orange" => tr("Orange"),
        "red" => tr("Red"),
        "pink" => tr("Pink"),
        "purple" => tr("Purple"),
        "slate" => tr("Slate"),
        "power-saver" => tr("Power Saver"),
        "balanced" => tr("Balanced"),
        "performance" => tr("Performance"),
        other => other,
    }
    .to_owned()
}

/// "2 h 5 min": `secs` rounded up to the minute.
pub fn time_left(secs: i64) -> String {
    let minutes = (secs.max(0) + 59) / 60;
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => trf!("{minutes} min", minutes = m),
        (h, 0) => trf!("{hours} h", hours = h),
        (h, m) => trf!("{hours} h {minutes} min", hours = h, minutes = m),
    }
}

/// The indicator's row for `p` at `now`: "Accent Color back to Blue in
/// 25 min" (README.md, "Changes, undo and restoring", "The indicator").
pub fn row_label(p: &Pending, now: i64) -> String {
    let (setting, value) = (setting_label(&p.setting), value_label(&p.setting, &p.back));
    let left = p.at - now;
    if left < 60 {
        trf!("{setting} back to {value} in less than a minute", setting = setting, value = value)
    } else {
        trf!("{setting} back to {value} in {time}", setting = setting, value = value, time = time_left(left))
    }
}

/// Whether a picture URI's file is gone (an empty URI is no picture, not
/// a missing one).
fn picture_gone(uri: &str) -> bool {
    !uri.is_empty() && crate::wallpaper::uri_path(uri).is_none_or(|p| !p.exists())
}

impl AgentAccess {
    pub(super) fn changes(&self) -> MutexGuard<'_, Changes> {
        self.inner.changes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What `setting` is now, read as `GetState`, the stars and
    /// `favorite-apps` read it; `None` when it cannot be read (the service
    /// or the setting is not there).
    pub async fn current(&self, setting: &Setting) -> Option<Values> {
        match setting {
            Setting::Wallpaper => {
                let s = helix_settings::Schema::open(BACKGROUND);
                Some(Values { value: Val::Text(s.get::<String>("picture-uri")?), dark: s.get::<String>("picture-uri-dark"), fit: s.get::<String>("picture-options") })
            }
            Setting::Accent => helix_settings::Schema::open(INTERFACE).get::<String>("accent-color").map(Values::text),
            Setting::PowerMode => {
                let system = self.system_bus().await?;
                let (name, iface) = tokio::time::timeout(Duration::from_secs(3), crate::status::profiles_service(&system)).await.ok()??;
                let active = crate::status::prop::<String>(&system, name, crate::status::PROFILES_PATH, iface, "ActiveProfile").await?;
                Some(Values::text(active))
            }
            Setting::Toggle(t) => self.toggle_state(*t).await.map(Values::flag),
            Setting::Star(uri) => {
                let uri = uri.clone();
                let starred = tokio::task::spawn_blocking(move || {
                    let store = helix_settings::stars::Store::open(&helix_settings::stars::data_home(), false);
                    store.starred().ok().map(|all| all.contains(&uri))
                })
                .await
                .ok()??;
                Some(Values::flag(starred))
            }
            Setting::Favorite(app) => {
                let schema = helix_settings::Schema::open(SHELL);
                let list = schema.strv("favorite-apps")?;
                Some(Values::flag(list.iter().any(|f| f.strip_suffix(".desktop").unwrap_or(f) == app)))
            }
        }
    }

    /// Whether quick setting `t` is on now (`None`: not there).
    async fn toggle_state(&self, t: Toggle) -> Option<bool> {
        match t {
            Toggle::DarkStyle => helix_settings::Schema::open(INTERFACE).get::<String>("color-scheme").map(|s| s == "prefer-dark"),
            Toggle::DoNotDisturb => helix_settings::Schema::open(NOTIFICATIONS).get::<bool>("show-banners").map(|b| !b),
            Toggle::NightLight => helix_settings::Schema::open(COLOR).get::<bool>("night-light-enabled"),
            Toggle::Wifi | Toggle::AirplaneMode => {
                let system = self.system_bus().await?;
                let (wifi, bluetooth) = tokio::time::timeout(Duration::from_secs(3), crate::status::radios(&system)).await.ok()?;
                match t {
                    Toggle::Wifi => wifi,
                    _ => (wifi.is_some() || bluetooth.is_some()).then(|| crate::status::airplane(wifi, bluetooth)),
                }
            }
            Toggle::Mute => tokio::time::timeout(Duration::from_secs(3), crate::shell_keys::volume_state()).await.ok()?.map(|(_, muted)| muted),
        }
    }

    /// Set the `parts` of `setting` to `values`, as Settings and quick
    /// settings do. `NotSupported` when that cannot be done (the service
    /// or the setting is gone, the picture is gone, stars cannot be
    /// written).
    pub async fn apply(&self, setting: &Setting, values: &Values, parts: Parts) -> Result<(), AgentError> {
        let unavailable = |what: &str| AgentError::NotSupported(format!("{what} is not available"));
        match setting {
            Setting::Wallpaper => {
                let s = helix_settings::Schema::open(BACKGROUND);
                let text = |v: &Val| match v {
                    Val::Text(t) => t.clone(),
                    Val::Bool(_) => String::new(),
                };
                let uri = text(&values.value);
                let dark = values.dark.clone().unwrap_or_default();
                if (parts.value && picture_gone(&uri)) || (parts.dark && picture_gone(&dark)) {
                    return Err(AgentError::NotSupported("the picture is gone".into()));
                }
                let mut ok = true;
                if parts.value {
                    ok &= s.set("picture-uri", uri);
                }
                if parts.dark {
                    ok &= s.set("picture-uri-dark", dark);
                }
                if parts.fit {
                    if let Some(fit) = &values.fit {
                        ok &= s.set("picture-options", fit.clone());
                    }
                }
                gio::Settings::sync();
                if ok { Ok(()) } else { Err(unavailable("the background")) }
            }
            Setting::Accent => {
                let Val::Text(name) = &values.value else { return Err(unavailable("the accent color")) };
                if !helix_settings::Schema::open(INTERFACE).set("accent-color", name.clone()) {
                    return Err(unavailable("the accent color"));
                }
                gio::Settings::sync();
                Ok(())
            }
            Setting::PowerMode => {
                let Val::Text(profile) = &values.value else { return Err(unavailable("the power mode")) };
                let system = self.system_bus().await.ok_or_else(|| unavailable("power-profiles-daemon"))?;
                let offered = tokio::time::timeout(Duration::from_secs(3), super::power::read(Some(&system))).await.unwrap_or_default();
                let profiles: Vec<String> = offered.get("power-profiles").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
                if !profiles.contains(profile) {
                    return Err(AgentError::NotSupported(format!("the power profile {profile} is not offered")));
                }
                let set = tokio::time::timeout(Duration::from_secs(3), crate::status::apply(&system, crate::status::Toggle::Profile(profile.clone())));
                set.await.map_err(|_| unavailable("power-profiles-daemon"))?.map_err(|e| AgentError::Failed(e.to_string()))?;
                crate::status::changed();
                Ok(())
            }
            Setting::Toggle(t) => {
                let Val::Bool(on) = values.value else { return Err(unavailable(t.name())) };
                self.set_toggle(*t, on).await
            }
            Setting::Star(uri) => {
                let Val::Bool(on) = values.value else { return Err(unavailable("stars")) };
                let uri = uri.clone();
                super::blocking(move || {
                    let mut store = helix_settings::stars::Store::open(&helix_settings::stars::data_home(), true);
                    if store.access() != helix_settings::stars::Access::ReadWrite {
                        return Err(AgentError::NotSupported(format!("stars cannot be written: {}", store.note())));
                    }
                    store.set_starred(&[uri], on).map_err(|e| AgentError::NotSupported(format!("stars cannot be written: {e}")))
                })
                .await
            }
            Setting::Favorite(app) => {
                let Val::Bool(on) = values.value else { return Err(unavailable("the dock")) };
                let schema = helix_settings::Schema::open(SHELL);
                let mut list = schema.strv("favorite-apps").ok_or_else(|| unavailable("the dock's favorites"))?;
                let desktop = format!("{app}.desktop");
                list.retain(|f| f.strip_suffix(".desktop").unwrap_or(f) != app);
                if on {
                    list.push(desktop);
                }
                if !schema.set("favorite-apps", list) {
                    return Err(unavailable("the dock's favorites"));
                }
                gio::Settings::sync();
                Ok(())
            }
        }
    }

    /// Turn quick setting `toggle` on or off, as quick settings does.
    pub(super) async fn set_toggle(&self, toggle: Toggle, on: bool) -> Result<(), AgentError> {
        let unavailable = || AgentError::NotSupported(format!("{} is not available", toggle.name()));
        match toggle {
            Toggle::DarkStyle | Toggle::NightLight => {
                let done = if toggle == Toggle::DarkStyle { crate::status::set_dark_style(on) } else { crate::status::set_night_light(on) };
                if !done {
                    return Err(unavailable());
                }
                gio::Settings::sync();
            }
            Toggle::DoNotDisturb => {
                if !helix_settings::Schema::open(NOTIFICATIONS).has("show-banners") {
                    return Err(unavailable());
                }
                crate::notifications::set_do_not_disturb(on);
            }
            Toggle::Wifi | Toggle::AirplaneMode => {
                let system = self.system_bus().await.ok_or_else(unavailable)?;
                let (wifi, bluetooth) = tokio::time::timeout(Duration::from_secs(3), crate::status::radios(&system)).await.map_err(|_| unavailable())?;
                let there = if toggle == Toggle::Wifi { wifi.is_some() } else { wifi.is_some() || bluetooth.is_some() };
                if !there {
                    return Err(unavailable());
                }
                let change = if toggle == Toggle::Wifi { crate::status::Toggle::Wifi(on) } else { crate::status::Toggle::Airplane(on) };
                crate::status::apply(&system, change).await.map_err(|e| AgentError::Failed(e.to_string()))?;
                crate::status::changed();
            }
            Toggle::Mute => {
                crate::shell_keys::volume_state().await.ok_or_else(unavailable)?;
                crate::shell_keys::set_mute(on).await;
                if let Some(conn) = self.inner.conn.get().cloned() {
                    self.refresh_quick(&conn).await;
                }
            }
        }
        Ok(())
    }

    /// Make a change of `caller`'s with `method`: read `setting`, work out
    /// its new value with `new_of` (from the old one), and when that is
    /// another value, set it, keep the change (with a pending restore for
    /// `until`) and record it in the log. The Change record.
    pub(super) async fn make_change(
        &self,
        caller: &Caller,
        method: &'static str,
        setting: Setting,
        variant: Option<&str>,
        until: Option<i64>,
        new_of: impl FnOnce(&Values) -> Values,
    ) -> Result<Record, AgentError> {
        let _turn = self.inner.change_turn.lock().await;
        let old = self.current(&setting).await.ok_or_else(|| AgentError::NotSupported(format!("{} is not available", setting.name())))?;
        let new = new_of(&old);
        let parts = match variant {
            Some(v) => Parts::wallpaper(v, old.fit != new.fit),
            None => Parts::ONE,
        };
        if parts.same(&old, &new) {
            // Nothing changes: no id, no restore, nothing counted, and a
            // pending restore of the setting stays.
            for (key, detail) in log_values(setting.name(), &old, &new) {
                caller.detail(key, detail);
            }
            return Ok(unchanged_record(&setting, &old, &new));
        }
        let limit_key = format!("{method} {}", caller.agent.id);
        if self.inner.change_limit.lock().unwrap_or_else(|e| e.into_inner()).full(&limit_key, std::time::Instant::now()) {
            return Err(AgentError::Failed(format!("too many changes: at most {CHANGES_PER_MINUTE} a minute")));
        }
        self.apply(&setting, &new, parts).await?;
        self.inner.change_limit.lock().unwrap_or_else(|e| e.into_inner()).allow(&limit_key, std::time::Instant::now());
        let change = Change {
            id: String::new(),
            agent: caller.agent.id.clone(),
            executable: caller.agent.executable.clone(),
            name: caller.agent.name.clone(),
            time: now(),
            setting: setting.name().to_owned(),
            target: setting.target().to_owned(),
            variant: variant.map(str::to_owned),
            old,
            new,
            until,
            back: None,
            done: false,
        };
        let change = self.changes().add(change);
        caller.detail("change", activity::Detail::Text(change.id.clone()));
        for (key, detail) in log_values(&change.setting, &change.old, &change.new) {
            caller.detail(key, detail);
        }
        if let Some(at) = change.until {
            caller.detail("restore-at", activity::Detail::Count(at.max(0) as u64));
        }
        tracing::info!(agent = %change.agent, id = %change.id, setting = %change.setting, until = ?change.until, "agent changed a setting");
        self.changes_changed();
        Ok(change.record())
    }

    /// Put change `id` back now, for `cause` (a pending restore, or
    /// *Restore now*): skipped when it is no longer the setting's latest,
    /// when the setting no longer has the agent's value, or when the old
    /// value cannot be set any more. Logged as the `Restore` event.
    pub async fn restore(&self, id: &str, cause: Cause) -> Result<(), Skip> {
        let _turn = self.inner.change_turn.lock().await;
        let Some(change) = self.changes().latest(id).filter(|c| c.pending()).cloned() else { return Err(Skip::NotFound) };
        let back = change.back.clone().unwrap_or_else(|| change.old.clone());
        let result = self.put_back(&change, &back).await;
        self.changes().finish(id);
        let (outcome, reason) = match &result {
            Ok(()) => (activity::Outcome::Allowed, None),
            Err(Skip::Changed) => (activity::Outcome::Cancelled, Some("changed")),
            Err(_) => (activity::Outcome::Cancelled, Some("unavailable")),
        };
        let setting = change.setting().map(|s| s.log_target()).unwrap_or_default();
        let mut detail = vec![("change", activity::Detail::Text(change.id.clone()))];
        detail.extend(log_values(&change.setting, &change.new, &back));
        detail.push(("cause", activity::Detail::Kind(cause.token())));
        self.inner.log.record(activity::Entry { time: now(), agent: change.agent.clone(), name: change.name.clone(), method: activity::RESTORE.to_owned(), target: setting, outcome, reason, detail, ..activity::Entry::default() });
        tracing::info!(agent = %change.agent, id, cause = cause.token(), ?result, "agent's change put back");
        self.changes_changed();
        result
    }

    /// Set `change`'s setting to `to` (the parts it wrote), when it still
    /// has the agent's value.
    async fn put_back(&self, change: &Change, to: &Values) -> Result<(), Skip> {
        let setting = change.setting().ok_or(Skip::NotFound)?;
        let parts = change.parts();
        let now = self.current(&setting).await.ok_or(Skip::Unavailable)?;
        if !parts.same(&now, &change.new) {
            return Err(Skip::Changed);
        }
        self.apply(&setting, to, parts).await.map_err(|err| {
            tracing::info!(%err, id = %change.id, "cannot put an agent's change back");
            Skip::Unavailable
        })
    }

    /// Do the restores due at `now`.
    pub async fn restore_due(&self, now: i64) {
        let due: Vec<String> = self.changes().pending(None).into_iter().filter(|c| c.until.is_some_and(|u| u <= now)).map(|c| c.id).collect();
        for id in due {
            let _ = self.restore(&id, Cause::Expired).await;
        }
    }

    /// Do `agent`'s pending restores (everyone's for `None`) at once.
    pub async fn restore_pending(&self, agent: Option<&str>, cause: Cause) {
        let ids: Vec<String> = self.changes().pending(agent).into_iter().map(|c| c.id).collect();
        for id in ids {
            let _ = self.restore(&id, cause).await;
        }
    }

    /// [`Self::restore_pending`] in the background (from the sync paths:
    /// *Stop*, revoking, the settings).
    pub(super) fn restore_soon(&self, agent: Option<&str>, cause: Cause) {
        if self.changes().pending(agent).is_empty() {
            return;
        }
        let (access, agent) = (self.clone(), agent.map(str::to_owned));
        tokio::spawn(async move { access.restore_pending(agent.as_deref(), cause).await });
    }

    /// At start: the restores a shell that did not stop normally left
    /// pending (the session that asked for them ended), at most a few
    /// seconds.
    pub async fn restore_leftovers(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.restore_pending(None, Cause::SessionEnded)).await;
    }

    /// The shell stops: the pending restores first (at most a few
    /// seconds; what is left is done at the next start).
    pub async fn session_ending(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.restore_pending(None, Cause::SessionEnded)).await;
    }

    /// Whether change `id` can be undone now: the latest of its setting,
    /// not done, still at the agent's value.
    async fn undoable(&self, change: &Change) -> bool {
        let Some(setting) = change.setting() else { return false };
        self.current(&setting).await.is_some_and(|now| change.parts().same(&now, &change.new))
    }

    /// Settings' `Undo`: put change `id`'s `old` back and drop its pending
    /// restore. The change, when known (for the log).
    pub async fn undo(&self, id: &str) -> (Option<Change>, Result<(), Skip>) {
        let _turn = self.inner.change_turn.lock().await;
        let known = self.changes().get(id).cloned();
        let Some(change) = self.changes().latest(id).cloned() else { return (known, Err(Skip::NotFound)) };
        let result = self.put_back(&change, &change.old).await;
        if result.is_ok() {
            self.changes().finish(id);
            tracing::info!(agent = %change.agent, id, "agent's change undone from Settings");
        }
        self.changes_changed();
        (Some(change), result)
    }

    /// `ListChanges`: the changes that can be undone now, newest first.
    pub async fn list_changes(&self) -> Vec<Record> {
        let open = self.changes().open();
        let mut out = Vec::new();
        for c in open {
            if self.undoable(&c).await {
                out.push(c.listed());
            }
        }
        out
    }

    /// What `ListChanges` gives may have changed: tell Settings, and the
    /// indicator (pending restores) and the timer.
    pub(super) fn changes_changed(&self) {
        self.inner.changes_wake.notify_one();
        self.publish();
        let Some(conn) = self.inner.conn.get().cloned() else { return };
        tokio::spawn(async move {
            if let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) {
                let _ = AgentAccessSettings::changes_changed(&emitter).await;
            }
        });
    }

    /// The pending restores by agent: (executable, name, rows).
    pub(super) fn pending_rows(&self) -> HashMap<String, (String, String, Vec<Pending>)> {
        let mut out: HashMap<String, (String, String, Vec<Pending>)> = HashMap::new();
        for c in self.changes().pending(None) {
            let row = Pending { id: c.id.clone(), setting: c.setting.clone(), back: c.back.as_ref().unwrap_or(&c.old).value.text(), at: c.until.unwrap_or_default() };
            out.entry(c.agent.clone()).or_insert_with(|| (c.executable.clone(), c.name.clone(), Vec::new())).2.push(row);
        }
        for (_, _, rows) in out.values_mut() {
            rows.sort_by_key(|r| r.at);
        }
        out
    }

    /// The settings changed (`enabled` off, `access-level` below
    /// *Control the desktop*): put every pending change back. Also prune
    /// changes with the log.
    pub(super) fn changes_settings(&self, config: &Config) {
        if !config.enabled || config.level < Level::Desktop {
            self.restore_soon(None, Cause::Stopped);
        }
        self.prune_changes(config.log_retention_days);
    }

    /// Drop changes older than the log keeps (`log-retention-days`), as
    /// the log is pruned.
    fn prune_changes(&self, days: u32) {
        let oldest = activity::oldest_kept(now(), days);
        let oldest = gio::glib::DateTime::from_local(
            oldest[..4].parse().unwrap_or(1970),
            oldest[5..7].parse().unwrap_or(1),
            oldest[8..10].parse().unwrap_or(1),
            0,
            0,
            0.0,
        )
        .map(|d| d.to_unix())
        .unwrap_or(0);
        if self.changes().prune(oldest) {
            self.changes_changed();
        }
    }

    /// The restores' timer (at least every [`RESTORE_POLL`], at once on
    /// resume), Settings' list following the settings under it, and the
    /// leftovers of a shell that did not stop normally put back now.
    pub(super) fn start_changes(&self) {
        let weak = std::sync::Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            // Pending from before this shell started: that session ended.
            if let Some(inner) = weak.upgrade() {
                AgentAccess { inner }.restore_pending(None, Cause::SessionEnded).await;
            }
            loop {
                let wait = {
                    let Some(inner) = weak.upgrade() else { return };
                    let access = AgentAccess { inner };
                    access.restore_due(now()).await;
                    // With the log, also when the day changes.
                    access.prune_changes(Config::read().log_retention_days);
                    let next = access.changes().next_due();
                    let secs = next.map_or(RESTORE_POLL.as_secs() as i64, |t| t - now()).clamp(1, RESTORE_POLL.as_secs() as i64);
                    let wake = access.inner.changes_wake.clone();
                    drop(access);
                    async move {
                        tokio::select! {
                            _ = wake.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(secs as u64)) => {}
                        }
                    }
                };
                wait.await;
            }
        });
        // Resume from suspend: due restores at once.
        let weak = std::sync::Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let Some(system) = (match weak.upgrade() {
                Some(inner) => AgentAccess { inner }.system_bus().await,
                None => None,
            }) else {
                return;
            };
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.login1.Manager")
                .and_then(|b| b.member("PrepareForSleep"))
                .map(|b| b.build());
            let Ok(rule) = rule else { return };
            let Ok(mut signals) = zbus::MessageStream::for_match_rule(rule, &system, None).await else { return };
            use futures_util::StreamExt;
            while let Some(Ok(message)) = signals.next().await {
                if message.body().deserialize::<bool>() == Ok(false) {
                    let Some(inner) = weak.upgrade() else { return };
                    inner.changes_wake.notify_one();
                }
            }
        });
        // Settings' list: watched settings at once, the rest every while.
        let weak = std::sync::Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let schemas: Vec<&str> = [BACKGROUND, INTERFACE, NOTIFICATIONS, COLOR, SHELL].into_iter().filter(|s| helix_settings::has_schema(s)).collect();
            let settings = helix_settings::GSettings::new(&schemas);
            let (mut changed, _watch) = settings.watch();
            let mut listed: Option<Vec<String>> = None;
            loop {
                tokio::select! {
                    got = changed.recv() => if got.is_none() { return },
                    _ = tokio::time::sleep(LIST_POLL) => {}
                }
                let Some(inner) = weak.upgrade() else { return };
                let access = AgentAccess { inner };
                if access.changes().open().is_empty() && listed.as_ref().is_some_and(Vec::is_empty) {
                    continue;
                }
                let now: Vec<String> = access.list_changes().await.iter().filter_map(|r| r.get("id").and_then(|v| String::try_from(v.clone()).ok())).collect();
                if listed.as_ref().is_some_and(|l| *l != now) {
                    access.changes_changed();
                }
                listed = Some(now);
            }
        });
    }
}

/// `org.helixdesktop.AgentAccess1.Undo`'s error: Settings only
/// (`AccessDenied`), or the change's (`NotFound`, `Failed`).
#[derive(Debug)]
pub enum UndoError {
    Denied(zbus::fdo::Error),
    Agent(AgentError),
}

impl zbus::DBusError for UndoError {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::Message> {
        match self {
            UndoError::Denied(e) => e.create_reply(call),
            UndoError::Agent(e) => e.create_reply(call),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            UndoError::Denied(e) => e.name(),
            UndoError::Agent(e) => e.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            UndoError::Denied(e) => e.description(),
            UndoError::Agent(e) => e.description(),
        }
    }
}

impl From<zbus::Error> for UndoError {
    fn from(e: zbus::Error) -> Self {
        UndoError::Agent(AgentError::ZBus(e))
    }
}

impl From<Skip> for AgentError {
    fn from(skip: Skip) -> AgentError {
        match skip {
            Skip::NotFound => AgentError::NotFound("no such change that can be undone".into()),
            Skip::Changed => AgentError::Failed("changed".into()),
            Skip::Unavailable => AgentError::Failed("unavailable".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(setting: &str, old: &str, new: &str, until: Option<i64>, time: i64) -> Change {
        Change {
            id: String::new(),
            agent: "/usr/bin/bot".into(),
            executable: "/usr/bin/bot".into(),
            name: "Bot".into(),
            time,
            setting: setting.into(),
            target: String::new(),
            variant: None,
            old: Values::text(old),
            new: Values::text(new),
            until,
            back: None,
            done: false,
        }
    }

    #[test]
    fn until_range() {
        assert!(check_until(1000 + UNTIL_MIN, 1000).is_ok());
        assert!(check_until(1000 + UNTIL_MAX, 1000).is_ok());
        assert_eq!(check_until(1000 + UNTIL_MIN - 1, 1000).unwrap_err().kind(), "InvalidArgs");
        assert_eq!(check_until(1000 + UNTIL_MAX + 1, 1000).unwrap_err().kind(), "InvalidArgs");
        assert_eq!(check_until(500, 1000).unwrap_err().kind(), "InvalidArgs", "in the past");
    }

    #[test]
    fn superseding_keeps_the_first_old_value() {
        let mut c = Changes::default();
        let a = c.add(change("accent", "blue", "red", Some(100), 1));
        assert_eq!(a.back, Some(Values::text("blue")));
        // Another agent's, also temporary: the user's blue comes back.
        let b = c.add(change("accent", "red", "green", Some(200), 2));
        assert_eq!(b.back, Some(Values::text("blue")));
        assert!(c.latest(&a.id).is_none(), "superseded");
        assert!(!c.get(&a.id).unwrap().pending());
        assert_eq!(c.next_due(), Some(200));
        // Lasting: the pending restore goes.
        let d = c.add(change("accent", "green", "teal", None, 3));
        assert_eq!(c.next_due(), None);
        assert!(c.latest(&d.id).is_some());
        assert_eq!(c.open().len(), 1);
        // Another setting is apart.
        let e = c.add(change("power-mode", "balanced", "performance", Some(50), 4));
        assert_eq!(c.open().iter().map(|x| x.id.clone()).collect::<Vec<_>>(), [e.id.clone(), d.id.clone()], "newest first");
        assert_eq!(c.pending(Some("/usr/bin/bot")).len(), 1);
        c.finish(&e.id);
        assert!(c.latest(&e.id).is_none(), "done");
        assert_eq!(c.open().len(), 1);
    }

    #[test]
    fn kept_and_pruned() {
        let dir = std::env::temp_dir().join(format!("helix-changes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("agent-changes.json");
        let mut c = Changes::load(Some(path.clone()));
        let old = c.add(change("accent", "blue", "red", None, 10));
        let kept = c.add(change("dark-style", "off", "on", Some(5000), 20));
        let again = Changes::load(Some(path.clone()));
        assert_eq!(again.list, c.list);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Pruning keeps the pending restore.
        c.prune(100);
        assert!(c.get(&old.id).is_none() && c.get(&kept.id).is_some());
        c.clear();
        assert!(c.get(&kept.id).is_some(), "pending survives ClearLog");
        for i in 0..KEEP_MAX + 5 {
            c.add(change("accent", "a", "b", None, 30 + i as i64));
        }
        assert_eq!(c.list.len(), KEEP_MAX);
        assert!(c.get(&kept.id).is_some());
        // A symlink in its place is not written through.
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_file_name("agent-changes.json.tmp")).ok();
        let target = dir.join("elsewhere");
        std::os::unix::fs::symlink(&target, path.with_file_name("agent-changes.json.tmp")).unwrap();
        c.add(change("accent", "b", "c", None, 9999));
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_and_parts() {
        let mut ch = change("wallpaper", "file:///a.png", "file:///b.png", Some(77), 1);
        ch.id = "0123456789abcdef".into();
        ch.variant = Some("light".into());
        ch.old.dark = Some("file:///d.png".into());
        ch.new.dark = Some("file:///d.png".into());
        ch.old.fit = Some("none".into());
        ch.new.fit = Some("zoom".into());
        let r = ch.record();
        let s = |k: &str| String::try_from(r[k].clone()).unwrap();
        assert_eq!((s("name"), s("old"), s("new"), s("old-dark"), s("new-fit")), ("wallpaper".into(), "file:///a.png".into(), "file:///b.png".into(), "file:///d.png".into(), "zoom".into()));
        assert_eq!(i64::try_from(r["until"].clone()).unwrap(), 77);
        assert!(!r.contains_key("target"));
        let parts = ch.parts();
        assert_eq!(parts, Parts { value: true, dark: false, fit: true });
        // The user changed only the dark picture: still the agent's.
        let mut now = ch.new.clone();
        now.dark = Some("file:///other.png".into());
        assert!(parts.same(&now, &ch.new));
        now.fit = Some("centered".into());
        assert!(!parts.same(&now, &ch.new));
        let star = unchanged_record(&Setting::Star("file:///home/me/a".into()), &Values::flag(true), &Values::flag(true));
        assert!(!bool::try_from(star["changed"].clone()).unwrap());
        assert_eq!(String::try_from(star["target"].clone()).unwrap(), "file:///home/me/a");
        assert!(bool::try_from(star["old"].clone()).unwrap());
        for name in ["wallpaper", "accent", "power-mode", "dark-style", "mute", "star", "favorite"] {
            assert_eq!(Setting::parse(name, "x").unwrap().name(), name);
        }
        assert_eq!(Setting::parse("bluetooth", ""), None);
    }

    #[test]
    fn indicator_rows() {
        let row = |setting: &str, back: &str, at: i64| row_label(&Pending { id: "x".into(), setting: setting.into(), back: back.into(), at }, 1000);
        assert_eq!(row("accent", "blue", 1000 + 25 * 60 - 30), "Accent Color back to Blue in 25 min");
        assert_eq!(row("dark-style", "off", 1000 + 61), "Dark Style back to off in 2 min");
        assert_eq!(row("dark-style", "off", 1000 + 60), "Dark Style back to off in 1 min");
        assert_eq!(row("wallpaper", "file:///x", 1000 + 2 * 3600 + 5 * 60), "Background back to the previous picture in 2 h 5 min");
        assert_eq!(row("power-mode", "balanced", 1000 + 3600), "Power Mode back to Balanced in 1 h");
        assert_eq!(row("night-light", "on", 1030), "Night Light back to on in less than a minute");
    }
}
