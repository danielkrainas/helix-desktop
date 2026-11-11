//! Window-management settings from `org.gnome.mutter`,
//! `org.gnome.desktop.wm.preferences` and `org.gnome.desktop.interface`
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window-management
//! settings" and "Tiling") the compositor follows.

use gio::prelude::*;

pub const MUTTER: &str = "org.gnome.mutter";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const INTERFACE: &str = "org.gnome.desktop.interface";
pub const MUTTER_WAYLAND: &str = "org.gnome.mutter.wayland";
pub const MUTTER_WAYLAND_KEYS: &str = "org.gnome.mutter.wayland.keybindings";
pub const SCHEMAS: [&str; 5] = [MUTTER, WM_PREFERENCES, INTERFACE, MUTTER_WAYLAND, MUTTER_WAYLAND_KEYS];

/// `focus-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusMode {
    #[default]
    Click,
    Sloppy,
    Mouse,
}

/// Modifiers of `mouse-button-modifier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub alt: bool,
    pub logo: bool,
    pub ctrl: bool,
    pub shift: bool,
}

/// `<Super>`, `<Alt>`, `<Control><Alt>`…; `None` when empty or not only
/// modifiers.
pub fn parse_modifier(value: &str) -> Option<Modifiers> {
    let mut mods = Modifiers::default();
    let mut rest = value.trim();
    if rest.is_empty() {
        return None;
    }
    while let Some(after) = rest.strip_prefix('<') {
        let end = after.find('>')?;
        match after[..end].to_ascii_lowercase().as_str() {
            "super" | "mod4" | "hyper" => mods.logo = true,
            "alt" | "mod1" | "meta" => mods.alt = true,
            "control" | "ctrl" | "primary" => mods.ctrl = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
        rest = &after[end + 1..];
    }
    (rest.is_empty() && mods != Modifiers::default()).then_some(mods)
}

/// An accelerator (`<Super>Escape`): its modifiers and key name.
pub fn parse_accelerator(value: &str) -> Option<(Modifiers, String)> {
    let mut mods = Modifiers::default();
    let mut rest = value.trim();
    while let Some(after) = rest.strip_prefix('<') {
        let end = after.find('>')?;
        match after[..end].to_ascii_lowercase().as_str() {
            "super" | "mod4" | "hyper" => mods.logo = true,
            "alt" | "mod1" | "meta" => mods.alt = true,
            "control" | "ctrl" | "primary" => mods.ctrl = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
        rest = &after[end + 1..];
    }
    (!rest.is_empty()).then(|| (mods, rest.to_owned()))
}

/// Extra Xwayland options for the settings: `switchable` lists the
/// extensions this Xwayland can switch off (from its `-help`).
pub fn xwayland_args(disable: u32, byte_swapped: bool, switchable: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (bit, name) in [(1, "SECURITY"), (2, "XTEST")] {
        if disable & bit != 0 {
            if switchable.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                out.extend(["-extension".to_owned(), name.to_owned()]);
            } else {
                tracing::info!(name, "Xwayland cannot switch off this extension; ignored");
            }
        }
    }
    out.push(if byte_swapped { "+byteswappedclients" } else { "-byteswappedclients" }.to_owned());
    out
}

/// The extensions an Xwayland `-help` text lists as switchable.
pub fn switchable_extensions(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|l| !l.contains("can be run-time enabled/disabled"))
        .skip(1)
        .take_while(|l| l.starts_with('\t'))
        .map(|l| l.trim().to_owned())
        .collect()
}

/// Whether `xwayland-grab-access-rules` let an X11 app with this WM_CLASS
/// name and class grab the keyboard: some entry matches (`*`, `?`
/// wildcards) and no `!` entry does.
pub fn grab_allowed(rules: &[String], name: &str, class: &str) -> bool {
    let hits = |pattern: &str| glob(pattern, name) || glob(pattern, class);
    if rules.iter().filter_map(|r| r.strip_prefix('!')).any(hits) {
        return false;
    }
    rules.iter().filter(|r| !r.starts_with('!')).any(|r| hits(r))
}

fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    !text.is_empty() && go(&p, &t)
}

/// Key names of `overlay-key`: `Super` means either Super key; empty: none.
pub fn overlay_keys(value: &str) -> Vec<String> {
    match value.trim() {
        "" => Vec::new(),
        "Super" => vec!["Super_L".into(), "Super_R".into()],
        name => vec![name.to_owned()],
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WmSettings {
    /// Dragging windows to monitor edges tiles or maximizes them.
    pub edge_tiling: bool,
    pub focus_mode: FocusMode,
    pub focus_on_rest: bool,
    pub auto_raise: bool,
    pub auto_raise_delay_ms: u32,
    pub raise_on_click: bool,
    pub modifier: Option<Modifiers>,
    pub resize_with_right_button: bool,
    /// `focus-new-windows` `strict`.
    pub strict_new_windows: bool,
    pub center_new_windows: bool,
    pub auto_maximize: bool,
    pub attach_modal_dialogs: bool,
    /// Key names that open the launcher pressed alone.
    pub overlay_keys: Vec<String>,
    pub audible_bell: bool,
    pub visual_bell: bool,
    /// `visual-bell-type` `fullscreen-flash` (else `frame-flash`).
    pub fullscreen_flash: bool,
    /// `locate-pointer`'s key, when on.
    pub locate_pointer_key: Option<String>,
    /// `check-alive-timeout` (ms; 0: off).
    pub check_alive_ms: u32,
    /// `restore-shortcuts`.
    pub restore_shortcuts: Vec<(Modifiers, String)>,
    /// `xwayland-allow-grabs`.
    pub xwayland_allow_grabs: bool,
    /// `xwayland-grab-access-rules`.
    pub xwayland_grab_rules: Vec<String>,
    /// `xwayland-disable-extension` (bits: 1 Security, 2 Xtest).
    pub xwayland_disable_extension: u32,
    /// `xwayland-allow-byte-swapped-clients`.
    pub xwayland_byte_swapped: bool,
    /// `cursor-theme`.
    pub cursor_theme: String,
    /// `cursor-size` (logical pixels).
    pub cursor_size: u32,
    /// `output-luminance`: (connector, vendor, product, serial, color mode,
    /// percent).
    pub output_luminance: Vec<(String, String, String, String, u32, f64)>,
}

impl Default for WmSettings {
    fn default() -> Self {
        Self {
            edge_tiling: false,
            focus_mode: FocusMode::Click,
            focus_on_rest: false,
            auto_raise: false,
            auto_raise_delay_ms: 500,
            raise_on_click: true,
            modifier: parse_modifier("<Super>"),
            resize_with_right_button: false,
            strict_new_windows: false,
            center_new_windows: true,
            auto_maximize: true,
            attach_modal_dialogs: false,
            overlay_keys: overlay_keys("Super"),
            audible_bell: true,
            visual_bell: false,
            fullscreen_flash: true,
            locate_pointer_key: None,
            check_alive_ms: 5000,
            restore_shortcuts: parse_accelerator("<Super>Escape").into_iter().collect(),
            xwayland_allow_grabs: false,
            xwayland_grab_rules: Vec::new(),
            xwayland_disable_extension: 0,
            xwayland_byte_swapped: false,
            cursor_theme: "Adwaita".into(),
            cursor_size: 24,
            output_luminance: Vec::new(),
        }
    }
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

fn boolean(schema: &str, key: &str, default: bool) -> bool {
    settings_with(schema, key).map_or(default, |s| s.boolean(key))
}

fn string(schema: &str, key: &str) -> Option<String> {
    settings_with(schema, key).map(|s| s.string(key).to_string())
}

/// The current settings (defaults where a schema or key is missing).
pub fn read() -> WmSettings {
    let d = WmSettings::default();
    let focus_mode = match string(WM_PREFERENCES, "focus-mode").as_deref() {
        Some("sloppy") => FocusMode::Sloppy,
        Some("mouse") => FocusMode::Mouse,
        _ => FocusMode::Click,
    };
    let locate = boolean(INTERFACE, "locate-pointer", false);
    WmSettings {
        edge_tiling: boolean(MUTTER, "edge-tiling", d.edge_tiling),
        focus_mode,
        focus_on_rest: boolean(MUTTER, "focus-change-on-pointer-rest", d.focus_on_rest),
        auto_raise: boolean(WM_PREFERENCES, "auto-raise", d.auto_raise),
        auto_raise_delay_ms: settings_with(WM_PREFERENCES, "auto-raise-delay")
            .map_or(d.auto_raise_delay_ms, |s| s.int("auto-raise-delay").clamp(0, 10_000) as u32),
        raise_on_click: boolean(WM_PREFERENCES, "raise-on-click", d.raise_on_click),
        modifier: string(WM_PREFERENCES, "mouse-button-modifier").map_or(d.modifier, |m| parse_modifier(&m)),
        resize_with_right_button: boolean(WM_PREFERENCES, "resize-with-right-button", d.resize_with_right_button),
        strict_new_windows: string(WM_PREFERENCES, "focus-new-windows").as_deref() == Some("strict"),
        center_new_windows: boolean(MUTTER, "center-new-windows", d.center_new_windows),
        auto_maximize: boolean(MUTTER, "auto-maximize", d.auto_maximize),
        attach_modal_dialogs: boolean(MUTTER, "attach-modal-dialogs", d.attach_modal_dialogs),
        overlay_keys: string(MUTTER, "overlay-key").map_or(d.overlay_keys, |k| overlay_keys(&k)),
        audible_bell: boolean(WM_PREFERENCES, "audible-bell", d.audible_bell),
        visual_bell: boolean(WM_PREFERENCES, "visual-bell", d.visual_bell),
        fullscreen_flash: string(WM_PREFERENCES, "visual-bell-type").as_deref() != Some("frame-flash"),
        check_alive_ms: settings_with(MUTTER, "check-alive-timeout").map_or(d.check_alive_ms, |s| s.uint("check-alive-timeout")),
        restore_shortcuts: settings_with(MUTTER_WAYLAND_KEYS, "restore-shortcuts").map_or(d.restore_shortcuts, |s| {
            s.strv("restore-shortcuts").iter().filter_map(|a| parse_accelerator(a.as_str())).collect()
        }),
        xwayland_allow_grabs: boolean(MUTTER_WAYLAND, "xwayland-allow-grabs", d.xwayland_allow_grabs),
        xwayland_disable_extension: settings_with(MUTTER_WAYLAND, "xwayland-disable-extension").map_or(0, |s| s.flags("xwayland-disable-extension")),
        xwayland_byte_swapped: boolean(MUTTER_WAYLAND, "xwayland-allow-byte-swapped-clients", d.xwayland_byte_swapped),
        xwayland_grab_rules: settings_with(MUTTER_WAYLAND, "xwayland-grab-access-rules")
            .map_or(d.xwayland_grab_rules, |s| s.strv("xwayland-grab-access-rules").iter().map(|r| r.to_string()).collect()),
        output_luminance: settings_with(MUTTER, "output-luminance")
            .and_then(|s| s.value("output-luminance").get())
            .unwrap_or_default(),
        cursor_theme: string(INTERFACE, "cursor-theme").filter(|t| !t.is_empty()).unwrap_or(d.cursor_theme),
        cursor_size: settings_with(INTERFACE, "cursor-size").map_or(d.cursor_size, |s| s.int("cursor-size").clamp(1, 512) as u32),
        locate_pointer_key: locate.then(|| string(MUTTER, "locate-pointer-key").unwrap_or_else(|| "Control_L".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers() {
        assert_eq!(parse_modifier("<Super>"), Some(Modifiers { logo: true, ..Default::default() }));
        assert_eq!(parse_modifier("<Control><Alt>"), Some(Modifiers { ctrl: true, alt: true, ..Default::default() }));
        assert_eq!(parse_modifier(""), None);
        assert_eq!(parse_modifier("<Super>a"), None, "a key, not only modifiers");
        assert_eq!(parse_modifier("<Bogus>"), None);
    }

    #[test]
    fn accelerators() {
        let logo = Modifiers { logo: true, ..Default::default() };
        assert_eq!(parse_accelerator("<Super>Escape"), Some((logo, "Escape".into())));
        assert_eq!(parse_accelerator("<Primary><Alt>F1").map(|a| a.0.ctrl && a.0.alt), Some(true));
        assert_eq!(parse_accelerator("<Super>"), None);
        assert_eq!(parse_accelerator("<Bogus>a"), None);
    }

    #[test]
    fn xwayland_options() {
        let help = "-foo\n Only the following extensions can be run-time enabled/disabled:\n\tSHAPE\n\tXTEST\n-query host\n";
        let switchable = switchable_extensions(help);
        assert_eq!(switchable, ["SHAPE", "XTEST"]);
        assert_eq!(xwayland_args(3, false, &switchable), ["-extension", "XTEST", "-byteswappedclients"], "SECURITY can't be switched off here");
        assert_eq!(xwayland_args(0, true, &switchable), ["+byteswappedclients"]);
    }

    #[test]
    fn grab_rules() {
        let rules: Vec<String> = ["Xephyr", "*vnc*", "!xvncviewer", "Rem?te"].iter().map(|s| s.to_string()).collect();
        assert!(grab_allowed(&rules, "Xephyr", ""));
        assert!(grab_allowed(&rules, "", "Tigervnc"), "class matches too");
        assert!(!grab_allowed(&rules, "xvncviewer", "Xvncviewer"), "! entries win");
        assert!(grab_allowed(&rules, "Remote", ""));
        assert!(!grab_allowed(&rules, "xterm", "XTerm"));
        assert!(!grab_allowed(&[], "Xephyr", "Xephyr"));
    }

    #[test]
    fn overlay() {
        assert_eq!(overlay_keys("Super"), ["Super_L", "Super_R"]);
        assert_eq!(overlay_keys("Super_R"), ["Super_R"]);
        assert!(overlay_keys("").is_empty());
    }
}
