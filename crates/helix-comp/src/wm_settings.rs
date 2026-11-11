//! Window-management settings from `org.gnome.mutter` and
//! `org.gnome.desktop.wm.preferences` (helix-desktop-spec
//! `contracts/wayland/README.md`) the compositor follows.

use gio::prelude::*;

pub const MUTTER: &str = "org.gnome.mutter";
pub const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
pub const SCHEMAS: [&str; 2] = [MUTTER, WM_PREFERENCES];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WmSettings {
    /// Dragging windows to monitor edges tiles or maximizes them.
    pub edge_tiling: bool,
}

fn settings_with(schema: &str, key: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema, true)?.has_key(key).then(|| gio::Settings::new(schema))
}

/// The current settings (defaults where a schema or key is missing).
pub fn read() -> WmSettings {
    let default = WmSettings::default();
    WmSettings {
        edge_tiling: settings_with(MUTTER, "edge-tiling").map_or(default.edge_tiling, |s| s.boolean("edge-tiling")),
    }
}
