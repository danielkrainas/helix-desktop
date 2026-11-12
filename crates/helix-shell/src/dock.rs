//! The dock (helix-desktop-spec `contracts/wayland/README.md`, "Dock"):
//! the dash on the desktop, configured with Ubuntu's Dash to Dock
//! settings (`org.gnome.shell.extensions.dash-to-dock`) when they are
//! installed.

pub const SCHEMA: &str = "org.gnome.shell.extensions.dash-to-dock";
const SHELL: &str = "org.gnome.shell";
pub const INTERFACE: &str = "org.gnome.desktop.interface";
/// Extensions whose being listed in `org.gnome.shell disabled-extensions`
/// turns the dock off.
pub const DOCK_EXTENSIONS: [&str; 2] = ["ubuntu-dock@ubuntu.com", "dash-to-dock@micxgx.gmail.com"];

/// The screen edge the dock is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

impl Edge {
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// Which windows make an intelligently hiding dock hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideMode {
    AllWindows,
    FocusedApp,
    Maximized,
    /// Never: the dock stays above windows.
    AlwaysOnTop,
}

/// What clicking an app's icon does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    Skip,
    Minimize,
    Launch,
    CycleWindows,
    MinimizeOrOverview,
    Previews,
    MinimizeOrPreviews,
    FocusOrPreviews,
    FocusOrAppSpread,
    FocusMinimizeOrPreviews,
    FocusMinimizeOrAppSpread,
    Quit,
}

impl ClickAction {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "skip" => Self::Skip,
            "minimize" => Self::Minimize,
            "launch" => Self::Launch,
            "cycle-windows" => Self::CycleWindows,
            "minimize-or-overview" => Self::MinimizeOrOverview,
            "previews" => Self::Previews,
            "minimize-or-previews" => Self::MinimizeOrPreviews,
            "focus-or-previews" => Self::FocusOrPreviews,
            "focus-or-appspread" => Self::FocusOrAppSpread,
            "focus-minimize-or-previews" => Self::FocusMinimizeOrPreviews,
            "focus-minimize-or-appspread" => Self::FocusMinimizeOrAppSpread,
            "quit" => Self::Quit,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    Nothing,
    CycleWindows,
    SwitchWorkspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transparency {
    Default,
    Fixed,
    Dynamic,
}

/// How running apps are marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    Dots,
    Squares,
    Dashes,
    Solid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DockSettings {
    pub enabled: bool,
    pub edge: Edge,
    pub fixed: bool,
    pub autohide: bool,
    pub intellihide: bool,
    pub hide_mode: HideMode,
    pub autohide_in_fullscreen: bool,
    /// Panel mode: the dock spans the whole edge.
    pub extend: bool,
    pub icon_size: i32,
    pub icon_size_fixed: bool,
    pub show_favorites: bool,
    pub show_running: bool,
    pub show_apps_button: bool,
    pub apps_at_top: bool,
    pub show_trash: bool,
    pub show_mounts: bool,
    pub click: ClickAction,
    pub middle_click: ClickAction,
    pub shift_click: ClickAction,
    pub scroll: ScrollAction,
    /// `preferred-monitor-by-connector`: a connector, or `primary`.
    pub monitor: String,
    pub isolate_workspaces: bool,
    /// A dock on every monitor (`multi-monitor`).
    pub multi_monitor: bool,
    /// Each dock shows only the apps with windows on its monitor.
    pub isolate_monitors: bool,
    pub opacity: f64,
    pub transparency: Transparency,
    pub indicator: Indicator,
    pub show_delay: f64,
    pub hide_delay: f64,
    pub always_center: bool,
    pub counters: bool,
    /// `show-windows-preview`: window previews instead of window lists.
    pub previews: bool,
    /// `require-pressure-to-show` and `pressure-threshold` (pixels).
    pub require_pressure: bool,
    pub pressure_threshold: f64,
    /// `hide-tooltip`: no names (or window lists) on pointing.
    pub hide_tooltip: bool,
    /// How long showing and hiding slide, in seconds (`animation-time`;
    /// 0 when `org.gnome.desktop.interface enable-animations` is off).
    pub animation_time: f64,
    /// The most of the edge the dock takes (`height-fraction`).
    pub height_fraction: f64,
}

impl Default for DockSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            edge: Edge::Bottom,
            fixed: false,
            autohide: true,
            intellihide: true,
            hide_mode: HideMode::FocusedApp,
            autohide_in_fullscreen: false,
            extend: false,
            icon_size: 48,
            icon_size_fixed: false,
            show_favorites: true,
            show_running: true,
            show_apps_button: true,
            apps_at_top: false,
            show_trash: true,
            show_mounts: true,
            click: ClickAction::CycleWindows,
            middle_click: ClickAction::Launch,
            shift_click: ClickAction::Minimize,
            scroll: ScrollAction::Nothing,
            monitor: "primary".into(),
            isolate_workspaces: false,
            multi_monitor: false,
            isolate_monitors: false,
            opacity: 0.8,
            transparency: Transparency::Default,
            indicator: Indicator::Dots,
            show_delay: 0.25,
            hide_delay: 0.2,
            always_center: false,
            counters: true,
            previews: true,
            require_pressure: true,
            pressure_threshold: 100.0,
            hide_tooltip: false,
            animation_time: 0.2,
            height_fraction: 0.9,
        }
    }
}

/// Read the dock settings (defaults without the schema).
pub fn read() -> DockSettings {
    use gio::prelude::*;
    let d = DockSettings::default();
    let source = gio::SettingsSchemaSource::default();
    let has = |schema: &str| source.as_ref().and_then(|s| s.lookup(schema, true)).is_some();
    let disabled = has(SHELL)
        && gio::Settings::new(SHELL).settings_schema().is_some_and(|s| s.has_key("disabled-extensions"))
        && gio::Settings::new(SHELL).strv("disabled-extensions").iter().any(|e| DOCK_EXTENSIONS.contains(&e.as_str()));
    let animations = !has(INTERFACE) || {
        let i = gio::Settings::new(INTERFACE);
        !i.settings_schema().is_some_and(|s| s.has_key("enable-animations")) || i.boolean("enable-animations")
    };
    if !has(SCHEMA) {
        return DockSettings { enabled: !disabled, animation_time: if animations { d.animation_time } else { 0.0 }, ..d };
    }
    let s = gio::Settings::new(SCHEMA);
    let schema = s.settings_schema();
    let key = |k: &str| schema.as_ref().is_some_and(|x| x.has_key(k));
    let b = |k: &str, default: bool| if key(k) { s.boolean(k) } else { default };
    let text = |k: &str| key(k).then(|| s.string(k).to_string());
    let num = |k: &str, default: f64| if key(k) { s.double(k) } else { default };
    let click = |k: &str, default| text(k).and_then(|v| ClickAction::parse(&v)).unwrap_or(default);
    DockSettings {
        enabled: !disabled,
        edge: match text("dock-position").as_deref() {
            Some("TOP") => Edge::Top,
            Some("RIGHT") => Edge::Right,
            Some("LEFT") => Edge::Left,
            _ => Edge::Bottom,
        },
        fixed: b("dock-fixed", d.fixed),
        autohide: b("autohide", d.autohide),
        intellihide: b("intellihide", d.intellihide),
        hide_mode: match text("intellihide-mode").as_deref() {
            Some("ALL_WINDOWS") => HideMode::AllWindows,
            Some("MAXIMIZED_WINDOWS") => HideMode::Maximized,
            Some("ALWAYS_ON_TOP") => HideMode::AlwaysOnTop,
            _ => HideMode::FocusedApp,
        },
        autohide_in_fullscreen: b("autohide-in-fullscreen", d.autohide_in_fullscreen),
        extend: b("extend-height", d.extend),
        icon_size: if key("dash-max-icon-size") { s.int("dash-max-icon-size").clamp(16, 128) } else { d.icon_size },
        icon_size_fixed: b("icon-size-fixed", d.icon_size_fixed),
        show_favorites: b("show-favorites", d.show_favorites),
        show_running: b("show-running", d.show_running),
        show_apps_button: b("show-show-apps-button", d.show_apps_button),
        apps_at_top: b("show-apps-at-top", d.apps_at_top),
        show_trash: b("show-trash", d.show_trash),
        show_mounts: b("show-mounts", d.show_mounts),
        click: click("click-action", d.click),
        middle_click: click("middle-click-action", d.middle_click),
        shift_click: click("shift-click-action", d.shift_click),
        scroll: match text("scroll-action").as_deref() {
            Some("cycle-windows") => ScrollAction::CycleWindows,
            Some("switch-workspace") => ScrollAction::SwitchWorkspace,
            _ => ScrollAction::Nothing,
        },
        monitor: text("preferred-monitor-by-connector").filter(|m| !m.is_empty()).unwrap_or(d.monitor),
        isolate_workspaces: b("isolate-workspaces", d.isolate_workspaces),
        multi_monitor: b("multi-monitor", d.multi_monitor),
        isolate_monitors: b("isolate-monitors", d.isolate_monitors),
        opacity: num("background-opacity", d.opacity).clamp(0.0, 1.0),
        transparency: match text("transparency-mode").as_deref() {
            Some("FIXED") => Transparency::Fixed,
            Some("DYNAMIC") => Transparency::Dynamic,
            _ => Transparency::Default,
        },
        indicator: match text("running-indicator-style").as_deref() {
            Some("SQUARES") | Some("BINARY") => Indicator::Squares,
            Some("DASHES") | Some("SEGMENTED") => Indicator::Dashes,
            Some("SOLID") | Some("METRO") => Indicator::Solid,
            _ => Indicator::Dots,
        },
        show_delay: num("show-delay", d.show_delay).clamp(0.0, 5.0),
        hide_delay: num("hide-delay", d.hide_delay).clamp(0.0, 5.0),
        always_center: b("always-center-icons", d.always_center),
        counters: b("show-icons-notifications-counter", d.counters),
        previews: b("show-windows-preview", d.previews),
        require_pressure: b("require-pressure-to-show", d.require_pressure),
        pressure_threshold: num("pressure-threshold", d.pressure_threshold).clamp(0.0, 10000.0),
        hide_tooltip: b("hide-tooltip", d.hide_tooltip),
        animation_time: if animations { num("animation-time", d.animation_time).clamp(0.0, 2.0) } else { 0.0 },
        height_fraction: num("height-fraction", d.height_fraction).clamp(0.1, 1.0),
    }
}

/// The dock's background opacity: the setting with `FIXED`; 0.8 with
/// `DEFAULT`; with `DYNAMIC` the setting, or opaque while a window touches
/// the dock.
pub fn background_opacity(s: &DockSettings, window_near: bool) -> f64 {
    match s.transparency {
        Transparency::Fixed => s.opacity,
        Transparency::Default => 0.8,
        Transparency::Dynamic if window_near => 1.0,
        Transparency::Dynamic => s.opacity,
    }
}

/// A window, for deciding whether the dock hides.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowArea {
    pub rect: (i32, i32, i32, i32),
    pub app: String,
    pub minimized: bool,
}

fn overlaps(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Whether a window covers the whole `monitor` (full screen).
pub fn covers(window: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32)) -> bool {
    window.0 <= monitor.0 && window.1 <= monitor.1 && window.0 + window.2 >= monitor.0 + monitor.2 && window.1 + window.3 >= monitor.1 + monitor.3
}

/// Whether the windows (of the current workspace) make the dock at
/// `dock` (layout coordinates) on `monitor` hide, for `mode`.
pub fn windows_hide(mode: HideMode, dock: (i32, i32, i32, i32), monitor: (i32, i32, i32, i32), edge: Edge, windows: &[WindowArea], focused_app: Option<&str>) -> bool {
    windows.iter().filter(|w| !w.minimized && overlaps(w.rect, dock)).any(|w| match mode {
        HideMode::AllWindows => true,
        HideMode::FocusedApp => focused_app.is_some_and(|a| !a.is_empty() && a.eq_ignore_ascii_case(&w.app)),
        // As wide (or tall) as the monitor along the dock's edge.
        HideMode::Maximized => {
            if edge.vertical() {
                w.rect.3 >= monitor.3 - 1
            } else {
                w.rect.2 >= monitor.2 - 1
            }
        }
        HideMode::AlwaysOnTop => false,
    })
}

/// Whether the dock shows, for its settings and what is around it: a
/// window over it (`overlapped`), a full-screen window, the pointer on it
/// or at the screen edge (`hovered`).
pub fn shown(s: &DockSettings, overlapped: bool, fullscreen: bool, hovered: bool) -> bool {
    if fullscreen {
        return hovered && s.autohide_in_fullscreen;
    }
    if s.fixed {
        return true;
    }
    match (s.intellihide, s.autohide) {
        (true, _) if !overlapped => true,
        (true, autohide) => autohide && hovered,
        (false, true) => hovered,
        (false, false) => true,
    }
}

/// What the dock holds, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// An app (id without `.desktop`); `favorite` is false for running apps
    /// that are not favorites.
    App { id: String, favorite: bool },
    /// A mounted drive, by index into the mounts.
    Mount(usize),
    Trash,
    ShowApps,
}

/// The dock's items: favorites, running apps, mounts, trash, and the Show
/// Apps button (first or last).
pub fn items(s: &DockSettings, favorites: &[String], installed: &[String], running: &[String], mounts: usize) -> Vec<Item> {
    let app_of = |d: &str| d.strip_suffix(".desktop").unwrap_or(d).to_owned();
    let mut out: Vec<Item> = Vec::new();
    if s.show_apps_button && s.apps_at_top {
        out.push(Item::ShowApps);
    }
    let mut seen: Vec<String> = Vec::new();
    if s.show_favorites {
        for f in favorites.iter().filter(|f| installed.contains(f)) {
            let id = app_of(f);
            seen.push(id.to_ascii_lowercase());
            out.push(Item::App { id, favorite: true });
        }
    }
    if s.show_running {
        for app in running {
            if !app.is_empty() && !seen.contains(&app.to_ascii_lowercase()) {
                seen.push(app.to_ascii_lowercase());
                out.push(Item::App { id: app.clone(), favorite: false });
            }
        }
    }
    if s.show_mounts {
        out.extend((0..mounts).map(Item::Mount));
    }
    if s.show_trash {
        out.push(Item::Trash);
    }
    if s.show_apps_button && !s.apps_at_top {
        out.push(Item::ShowApps);
    }
    out
}

/// The lines of a running app's tooltip: its name, then its windows'
/// titles (at most `MAX_TITLES`, then "N more"; untitled ones as
/// "Untitled").
pub fn tooltip_lines(name: &str, titles: &[String]) -> Vec<String> {
    let mut out = vec![name.to_owned()];
    for t in titles.iter().take(MAX_TITLES) {
        out.push(if t.trim().is_empty() { "Untitled".into() } else { t.trim().to_owned() });
    }
    if titles.len() > MAX_TITLES {
        out.push(format!("{} more", titles.len() - MAX_TITLES));
    }
    out
}

/// Most window titles in a tooltip.
pub const MAX_TITLES: usize = 5;

/// The dock's room along an edge `edge` pixels long: `height-fraction`
/// of what the panel's height at both ends leaves.
pub fn room(s: &DockSettings, edge: i32, panel: i32) -> i32 {
    ((f64::from(edge - 2 * panel)) * s.height_fraction).round() as i32
}

/// How far (pixels) a sliding dock is pushed off its edge, `elapsed`
/// seconds into a slide from `from` to `to` lasting `total` seconds;
/// `None` once it has arrived. Eases out.
pub fn slide_offset(from: f64, to: f64, elapsed: f64, total: f64) -> Option<f64> {
    if total <= 0.0 || elapsed >= total {
        return None;
    }
    let t = (elapsed / total).clamp(0.0, 1.0);
    let e = 1.0 - (1.0 - t).powi(3);
    Some(from + (to - from) * e)
}

/// The icon size that fits `count` items in `room` pixels along the edge
/// (not above `max`, not below 16, unless the size is fixed).
pub fn fit_icon_size(max: i32, fixed: bool, count: usize, room: i32) -> i32 {
    if fixed || count == 0 {
        return max;
    }
    // Each item takes the icon and its padding (a quarter of the icon).
    let per = room / count as i32;
    max.min(per * 4 / 5).max(16)
}

/// Favorites after dragging `id` to position `to` among them (a new
/// favorite is inserted there; `to` past the end appends).
pub fn moved_favorites(list: &[String], id: &str, to: usize) -> Vec<String> {
    let mut out: Vec<String> = list.iter().filter(|f| *f != id).cloned().collect();
    let from = list.iter().position(|f| f == id);
    // Dropping after itself counts the gap it leaves.
    let to = match from {
        Some(from) if from < to => to - 1,
        _ => to,
    };
    out.insert(to.min(out.len()), id.to_owned());
    out
}

/// What a click does for an app with `windows` (most recent first), the
/// focused one among them or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    Launch,
    Focus(u32),
    MinimizeAll,
    /// The app's windows to choose from (a menu listing them).
    ChooseWindow,
    Overview,
    CloseAll,
}

pub fn click_outcome(action: ClickAction, windows: &[u32], focused: bool, next_after_focused: Option<u32>) -> Outcome {
    let Some(&newest) = windows.first() else {
        return if action == ClickAction::Skip { Outcome::Nothing } else { Outcome::Launch };
    };
    let many = windows.len() > 1;
    match action {
        ClickAction::Skip => Outcome::Nothing,
        ClickAction::Launch => Outcome::Launch,
        ClickAction::Quit => Outcome::CloseAll,
        ClickAction::Minimize if focused => Outcome::MinimizeAll,
        ClickAction::Minimize => Outcome::Focus(newest),
        ClickAction::CycleWindows if focused && many => Outcome::Focus(next_after_focused.unwrap_or(newest)),
        ClickAction::CycleWindows => Outcome::Focus(newest),
        ClickAction::MinimizeOrOverview if focused => Outcome::MinimizeAll,
        ClickAction::MinimizeOrOverview | ClickAction::FocusOrAppSpread if many => Outcome::Overview,
        ClickAction::MinimizeOrOverview | ClickAction::FocusOrAppSpread => Outcome::Focus(newest),
        ClickAction::Previews if many => Outcome::ChooseWindow,
        ClickAction::Previews => Outcome::Focus(newest),
        ClickAction::MinimizeOrPreviews if focused => Outcome::MinimizeAll,
        ClickAction::MinimizeOrPreviews | ClickAction::FocusOrPreviews if many => Outcome::ChooseWindow,
        ClickAction::MinimizeOrPreviews | ClickAction::FocusOrPreviews => Outcome::Focus(newest),
        ClickAction::FocusMinimizeOrPreviews | ClickAction::FocusMinimizeOrAppSpread if focused => Outcome::MinimizeAll,
        ClickAction::FocusMinimizeOrPreviews if many => Outcome::ChooseWindow,
        ClickAction::FocusMinimizeOrAppSpread if many => Outcome::Overview,
        ClickAction::FocusMinimizeOrPreviews | ClickAction::FocusMinimizeOrAppSpread => Outcome::Focus(newest),
    }
}

/// The desktop file `desktop_id` (in the XDG data directories'
/// `applications/`, a `-` in the id possibly standing for a subfolder).
fn desktop_file(desktop_id: &str) -> Option<gio::glib::KeyFile> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let roots = data_home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(std::path::PathBuf::from));
    for root in roots {
        let base = root.join("applications");
        let mut candidates = vec![base.join(desktop_id)];
        candidates.extend(desktop_id.match_indices('-').map(|(i, _)| base.join(&desktop_id[..i]).join(&desktop_id[i + 1..])));
        for path in candidates {
            let file = gio::glib::KeyFile::new();
            if file.load_from_file(&path, gio::glib::KeyFileFlags::NONE).is_ok() {
                return Some(file);
            }
        }
    }
    None
}

/// A key of a desktop file's `[Desktop Entry]` (localized where it can be).
pub fn desktop_key(desktop_id: &str, key: &str) -> Option<String> {
    let file = desktop_file(desktop_id)?;
    file.locale_string("Desktop Entry", key, None).or_else(|_| file.string("Desktop Entry", key)).ok().map(|s| s.to_string())
}

/// A desktop file's extra actions (`Actions=` and their `Name`s).
pub fn desktop_actions(app_id: &str) -> Vec<(String, String)> {
    let Some(file) = desktop_file(&format!("{app_id}.desktop")) else { return Vec::new() };
    let Ok(actions) = file.string_list("Desktop Entry", "Actions") else { return Vec::new() };
    actions
        .iter()
        .filter_map(|a| {
            let group = format!("Desktop Action {a}");
            let name = file.locale_string(&group, "Name", None).ok()?;
            Some((a.to_string(), name.to_string()))
        })
        .collect()
}

/// An `Exec` line without its field codes (`%f`, `%U`, …; `%%` is `%`).
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::new();
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some('%') = chars.next() {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run desktop action `action` of `desktop_id`.
pub fn launch_action(desktop_id: &str, action: &str) -> Result<(), String> {
    use gio::prelude::*;
    let file = desktop_file(desktop_id).ok_or_else(|| format!("no app {desktop_id}"))?;
    let exec = file.string(&format!("Desktop Action {action}"), "Exec").map_err(|e| e.to_string())?;
    let info = gio::AppInfo::create_from_commandline(strip_field_codes(&exec), None, gio::AppInfoCreateFlags::NONE).map_err(|e| e.to_string())?;
    info.launch(&[], None::<&gio::AppLaunchContext>).map_err(|e| e.to_string())
}

/// Pushes against the screen edge longer apart than this start over.
pub const PRESSURE_TIMEOUT_MS: u64 = 1000;

/// Pushing against the screen edge to show a hidden dock
/// (`require-pressure-to-show`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Pressure {
    sum: f64,
    last_ms: u64,
}

impl Pressure {
    /// Pointer motion `delta` at `time_ms` while at `edge`: whether the
    /// pushes towards the edge now add up to `threshold` pixels.
    pub fn push(&mut self, edge: Edge, delta: (f64, f64), time_ms: u64, threshold: f64) -> bool {
        let outwards = match edge {
            Edge::Bottom => delta.1,
            Edge::Top => -delta.1,
            Edge::Left => -delta.0,
            Edge::Right => delta.0,
        };
        if time_ms.saturating_sub(self.last_ms) > PRESSURE_TIMEOUT_MS {
            self.sum = 0.0;
        }
        if outwards > 0.0 {
            self.sum += outwards;
            self.last_ms = time_ms;
        }
        self.sum >= threshold
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pressure() {
        use super::{Edge, Pressure};
        let mut p = Pressure::default();
        assert!(!p.push(Edge::Bottom, (0.0, 60.0), 1000, 100.0));
        assert!(!p.push(Edge::Bottom, (50.0, -30.0), 1100, 100.0), "moving away or along adds nothing");
        assert!(p.push(Edge::Bottom, (0.0, 40.0), 1200, 100.0), "60 + 40");
        p.reset();
        assert!(!p.push(Edge::Left, (-70.0, 0.0), 5000, 100.0));
        assert!(!p.push(Edge::Left, (-70.0, 0.0), 6500, 100.0), "too long apart: starts over");
        assert!(p.push(Edge::Left, (-40.0, 0.0), 6700, 100.0));
        assert!(Pressure::default().push(Edge::Top, (0.0, -1.0), 0, 0.0), "no threshold");
    }

    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn items_in_order() {
        let set = DockSettings::default();
        let got = items(&set, &s(&["a.desktop", "gone.desktop", "b.desktop"]), &s(&["a.desktop", "b.desktop"]), &s(&["c", "A", ""]), 1);
        assert_eq!(
            got,
            [
                Item::App { id: "a".into(), favorite: true },
                Item::App { id: "b".into(), favorite: true },
                Item::App { id: "c".into(), favorite: false },
                Item::Mount(0),
                Item::Trash,
                Item::ShowApps
            ]
        );
        let top = DockSettings { apps_at_top: true, show_trash: false, show_mounts: false, show_running: false, ..DockSettings::default() };
        assert_eq!(items(&top, &s(&["a.desktop"]), &s(&["a.desktop"]), &s(&["c"]), 2), [Item::ShowApps, Item::App { id: "a".into(), favorite: true }]);
    }

    #[test]
    fn hiding() {
        let d = DockSettings::default();
        assert!(shown(&d, false, false, false));
        assert!(!shown(&d, true, false, false));
        assert!(shown(&d, true, false, true), "revealed at the edge");
        assert!(!shown(&DockSettings { autohide: false, ..d.clone() }, true, false, true));
        assert!(shown(&DockSettings { fixed: true, ..d.clone() }, true, false, false));
        assert!(!shown(&DockSettings { intellihide: false, ..d.clone() }, false, false, false));
        assert!(shown(&DockSettings { intellihide: false, autohide: false, ..d.clone() }, true, false, false));
        assert!(!shown(&d, false, true, true), "full screen");
        assert!(shown(&DockSettings { autohide_in_fullscreen: true, ..d }, false, true, true));

        let monitor = (0, 0, 1000, 800);
        let dock = (300, 720, 400, 80);
        let w = |rect, app: &str| WindowArea { rect, app: app.into(), minimized: false };
        let windows = [w((0, 0, 1000, 760), "edit"), w((0, 0, 200, 200), "calc")];
        assert!(windows_hide(HideMode::AllWindows, dock, monitor, Edge::Bottom, &windows, None));
        assert!(windows_hide(HideMode::FocusedApp, dock, monitor, Edge::Bottom, &windows, Some("edit")));
        assert!(!windows_hide(HideMode::FocusedApp, dock, monitor, Edge::Bottom, &windows, Some("calc")));
        assert!(windows_hide(HideMode::Maximized, dock, monitor, Edge::Bottom, &windows, None));
        assert!(!windows_hide(HideMode::Maximized, dock, monitor, Edge::Bottom, &[w((250, 600, 300, 200), "x")], None));
        assert!(!windows_hide(HideMode::AlwaysOnTop, dock, monitor, Edge::Bottom, &windows, None));
        assert!(covers((0, 0, 1000, 800), monitor) && !covers((0, 30, 1000, 770), monitor));
    }

    #[test]
    fn clicks() {
        use ClickAction::*;
        assert_eq!(click_outcome(CycleWindows, &[], false, None), Outcome::Launch);
        assert_eq!(click_outcome(CycleWindows, &[5, 6], false, None), Outcome::Focus(5));
        assert_eq!(click_outcome(CycleWindows, &[5, 6], true, Some(6)), Outcome::Focus(6));
        assert_eq!(click_outcome(Minimize, &[5], true, None), Outcome::MinimizeAll);
        assert_eq!(click_outcome(Minimize, &[5], false, None), Outcome::Focus(5));
        assert_eq!(click_outcome(Launch, &[5], true, None), Outcome::Launch);
        assert_eq!(click_outcome(FocusOrPreviews, &[5, 6], false, None), Outcome::ChooseWindow);
        assert_eq!(click_outcome(MinimizeOrOverview, &[5, 6], false, None), Outcome::Overview);
        assert_eq!(click_outcome(Quit, &[5], false, None), Outcome::CloseAll);
        assert_eq!(click_outcome(Skip, &[], false, None), Outcome::Nothing);
    }

    #[test]
    fn field_codes() {
        assert_eq!(strip_field_codes("firefox --private-window %u"), "firefox --private-window");
        assert_eq!(strip_field_codes("app --x=100%% %F"), "app --x=100%");
    }

    #[test]
    fn tooltips() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(tooltip_lines("Files", &[]), ["Files"]);
        assert_eq!(tooltip_lines("Files", &t(&["Home", " "])), ["Files", "Home", "Untitled"]);
        let many = t(&["a", "b", "c", "d", "e", "f", "g"]);
        assert_eq!(tooltip_lines("X", &many).last().unwrap(), "2 more");
        assert_eq!(tooltip_lines("X", &many).len(), 7);
    }

    #[test]
    fn slides_and_room() {
        let d = DockSettings::default();
        assert_eq!(room(&d, 1080, 32), 914);
        assert_eq!(room(&DockSettings { height_fraction: 0.5, ..d }, 1032, 16), 500);
        assert_eq!(slide_offset(60.0, 0.0, 0.0, 0.2), Some(60.0));
        let mid = slide_offset(60.0, 0.0, 0.1, 0.2).unwrap();
        assert!(mid < 30.0 && mid > 0.0, "eases out: {mid}");
        assert_eq!(slide_offset(60.0, 0.0, 0.2, 0.2), None);
        assert_eq!(slide_offset(0.0, 60.0, 0.0, 0.0), None, "no animation");
    }

    #[test]
    fn sizes_and_moves() {
        assert_eq!(fit_icon_size(48, false, 10, 1000), 48);
        assert_eq!(fit_icon_size(48, false, 20, 600), 24);
        assert_eq!(fit_icon_size(48, true, 20, 600), 48);
        assert_eq!(fit_icon_size(48, false, 200, 600), 16);
        let l = s(&["a", "b", "c"]);
        assert_eq!(moved_favorites(&l, "a", 3), s(&["b", "c", "a"]));
        assert_eq!(moved_favorites(&l, "c", 0), s(&["c", "a", "b"]));
        assert_eq!(moved_favorites(&l, "a", 1), s(&["a", "b", "c"]), "dropped where it was");
        assert_eq!(moved_favorites(&l, "new", 1), s(&["a", "new", "b", "c"]));
        assert_eq!(background_opacity(&DockSettings::default(), true), 0.8);
        let dynamic = DockSettings { transparency: Transparency::Dynamic, opacity: 0.3, ..DockSettings::default() };
        assert_eq!((background_opacity(&dynamic, true), background_opacity(&dynamic, false)), (1.0, 0.3));
    }
}
