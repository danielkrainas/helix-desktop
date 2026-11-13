//! Privacy screens (helix-desktop-spec `contracts/gsettings/org.gnome.desktop.privacy.md`,
//! "Privacy screen"): built-in panels that can narrow their viewing angle,
//! through the kernel's `privacy-screen sw-state` connector property.

pub const SCHEMA: &str = "org.gnome.desktop.privacy";
/// The connector property, and its values.
pub const PROPERTY: &str = "privacy-screen sw-state";

/// The property value for on or off.
pub fn state_name(on: bool) -> &'static str {
    if on {
        "Enabled"
    } else {
        "Disabled"
    }
}

/// `privacy-screen` (false without the key).
pub fn wanted() -> bool {
    use gio::prelude::*;
    helix_settings::has_schema(SCHEMA) && {
        let s = gio::Settings::new(SCHEMA);
        s.settings_schema().is_some_and(|x| x.has_key("privacy-screen")) && s.boolean("privacy-screen")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn values() {
        assert_eq!(super::state_name(true), "Enabled");
        assert_eq!(super::state_name(false), "Disabled");
    }
}
