//! Window-management settings from `org.gnome.mutter`,
//! `org.gnome.desktop.wm.preferences` and `org.gnome.desktop.interface`
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window-management
//! settings" and "Tiling") the compositor follows.

use gio::prelude::*;

pub const MUTTER: &str = "org.gnome.mutter";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const INTERFACE: &str = "org.gnome.desktop.interface";
pub const SCHEMAS: [&str; 3] = [MUTTER, WM_PREFERENCES, INTERFACE];

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
    fn overlay() {
        assert_eq!(overlay_keys("Super"), ["Super_L", "Super_R"]);
        assert_eq!(overlay_keys("Super_R"), ["Super_R"]);
        assert!(overlay_keys("").is_empty());
    }
}
