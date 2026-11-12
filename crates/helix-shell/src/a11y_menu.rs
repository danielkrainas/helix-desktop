//! The accessibility menu in the panel (helix-desktop-spec
//! `contracts/wayland/README.md`, "Accessibility menu"): switches for the
//! accessibility features, shown when asked for or when one is on.

const A11Y: &str = "org.gnome.desktop.a11y";
const APPS: &str = "org.gnome.desktop.a11y.applications";
const KEYBOARD: &str = "org.gnome.desktop.a11y.keyboard";
const INTERFACE_A11Y: &str = "org.gnome.desktop.a11y.interface";
const INTERFACE: &str = "org.gnome.desktop.interface";
const WM: &str = "org.gnome.desktop.wm.preferences";

/// Schemas whose changes the menu follows.
pub const SCHEMAS: [&str; 6] = [A11Y, APPS, KEYBOARD, INTERFACE_A11Y, INTERFACE, WM];

/// `text-scaling-factor` while Large Text is on.
pub const LARGE_TEXT: f64 = 1.25;

/// A switch: its label, schema and key.
pub struct Switch {
    pub label: &'static str,
    schema: &'static str,
    key: &'static str,
}

pub const SWITCHES: [Switch; 10] = [
    Switch { label: "High Contrast", schema: INTERFACE_A11Y, key: "high-contrast" },
    Switch { label: "Zoom", schema: APPS, key: "screen-magnifier-enabled" },
    Switch { label: "Large Text", schema: INTERFACE, key: "text-scaling-factor" },
    Switch { label: "Screen Reader", schema: APPS, key: "screen-reader-enabled" },
    Switch { label: "Screen Keyboard", schema: APPS, key: "screen-keyboard-enabled" },
    Switch { label: "Visual Alerts", schema: WM, key: "visual-bell" },
    Switch { label: "Sticky Keys", schema: KEYBOARD, key: "stickykeys-enable" },
    Switch { label: "Slow Keys", schema: KEYBOARD, key: "slowkeys-enable" },
    Switch { label: "Bounce Keys", schema: KEYBOARD, key: "bouncekeys-enable" },
    Switch { label: "Mouse Keys", schema: KEYBOARD, key: "mousekeys-enable" },
];

/// What the menu shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// The indicator is in the panel.
    pub shown: bool,
    /// Each switch's state (`SWITCHES` order); `None` when its key is not
    /// installed (the switch is left out).
    pub switches: Vec<Option<bool>>,
}

/// Whether the indicator shows: asked for, or any switch on.
pub fn shown(always: bool, switches: &[Option<bool>]) -> bool {
    always || switches.contains(&Some(true))
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

pub fn read() -> Status {
    use gio::prelude::*;
    let switches: Vec<Option<bool>> = SWITCHES
        .iter()
        .map(|s| {
            let settings = settings_with(s.schema, s.key)?;
            Some(if s.key == "text-scaling-factor" { settings.double(s.key) > 1.0 } else { settings.boolean(s.key) })
        })
        .collect();
    let always = settings_with(A11Y, "always-show-universal-access-status").is_some_and(|s| s.boolean("always-show-universal-access-status"));
    Status { shown: shown(always, &switches), switches }
}

/// Turn switch `index` to `on`.
pub fn set(index: usize, on: bool) {
    use gio::prelude::*;
    let Some(s) = SWITCHES.get(index) else { return };
    let Some(settings) = settings_with(s.schema, s.key) else { return };
    let result = if s.key == "text-scaling-factor" {
        settings.set_double(s.key, if on { LARGE_TEXT } else { 1.0 })
    } else {
        settings.set_boolean(s.key, on)
    };
    if let Err(err) = result {
        tracing::warn!(%err, key = s.key, "cannot change an accessibility setting");
    }
    gio::Settings::sync();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator() {
        assert!(!shown(false, &[Some(false), None]));
        assert!(shown(false, &[Some(false), Some(true)]));
        assert!(shown(true, &[]));
        assert_eq!(SWITCHES.len(), 10);
    }
}
