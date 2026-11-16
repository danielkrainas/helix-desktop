//! `/etc/helix/greeter.toml` (helixdesktop/spec `components/greeter.md`,
//! "Configuration"). `HELIX_GREETER_CONFIG` names another file (tests).

use serde::Deserialize;

pub const CONFIG_PATH: &str = "/etc/helix/greeter.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// false: always ask for the username.
    pub user_list: bool,
    /// Text under the clock.
    pub banner: Option<String>,
    /// The accent color's name, as `accent-color` takes it (the greeter
    /// user has no settings of their own); blue when unset or unknown.
    pub accent: Option<String>,
    /// `12h` or `24h`; unset: `org.gnome.desktop.interface clock-format`,
    /// else the locale's own time format.
    pub clock_format: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { user_list: true, banner: None, accent: None, clock_format: None }
    }
}

impl Config {
    pub fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        let mut config: Self = toml::from_str(contents)?;
        config.banner = config.banner.filter(|b| !b.trim().is_empty());
        Ok(config)
    }

    /// The configuration; defaults when the file is missing or broken.
    pub fn load() -> Self {
        let path = std::env::var("HELIX_GREETER_CONFIG").unwrap_or_else(|_| CONFIG_PATH.to_owned());
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents).unwrap_or_else(|err| {
                tracing::warn!(%err, path, "ignoring broken configuration");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// The accent to draw with.
    pub fn accent(&self) -> helix_draw::accent::Accent {
        let Some(name) = self.accent.as_deref() else { return Default::default() };
        helix_draw::accent::Accent::from_name(name.trim()).unwrap_or_else(|| {
            tracing::warn!(name, "unknown accent; using blue");
            Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("user-list = false\nbanner = \"Authorized use only.\"\n").unwrap(), Config { user_list: false, banner: Some("Authorized use only.".into()), accent: None, clock_format: None });
        assert_eq!(Config::parse("banner = \"  \"").unwrap().banner, None);
        assert!(Config::parse("user-list = \"no\"").is_err());
        use helix_draw::accent::Accent;
        assert_eq!(Config::default().accent(), Accent::Blue);
        assert_eq!(Config::parse("accent = \"orange\"").unwrap().accent(), Accent::Orange);
        assert_eq!(Config::parse("accent = \"magenta\"").unwrap().accent(), Accent::Blue, "unknown: blue");
        assert_eq!(Config::parse("clock-format = \"12h\"").unwrap().clock_format.as_deref(), Some("12h"));
        // Unknown keys are allowed (newer configuration, older greeter).
        assert!(Config::parse("theme = \"x\"").is_ok());
    }

    #[test]
    fn loads_from_env_path() {
        let path = std::env::temp_dir().join(format!("helix-greeter-config-{}.toml", std::process::id()));
        std::fs::write(&path, "user-list = false\n").unwrap();
        // Only this test sets HELIX_GREETER_CONFIG.
        unsafe { std::env::set_var("HELIX_GREETER_CONFIG", &path) };
        assert!(!Config::load().user_list);
        std::fs::write(&path, "user-list = [").unwrap();
        assert_eq!(Config::load(), Config::default());
        let _ = std::fs::remove_file(&path);
        assert_eq!(Config::load(), Config::default(), "missing file");
    }
}
