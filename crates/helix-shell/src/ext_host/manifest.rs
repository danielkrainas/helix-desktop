//! Extension manifests (helix-desktop-spec `contracts/extensions/README.md`,
//! "Manifest").

use zbus::zvariant::{OwnedValue, Value};

/// The API 1 minor version this Helix supports.
pub const API_MINOR: u32 = 1;

/// Permissions, with how the user sees them.
pub const PERMISSIONS: [(&str, &str); 4] = [("notifications", "show notifications"), ("inhibit-idle", "keep the screen on"), ("launch-apps", "start apps"), ("popover", "show its own panels")];

pub fn permission_text(name: &str) -> &str {
    PERMISSIONS.iter().find(|(n, _)| *n == name).map_or(name, |(_, t)| t)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Left,
    Center,
    Right,
}

impl Zone {
    pub const ALL: [Zone; 3] = [Zone::Left, Zone::Center, Zone::Right];

    pub fn index(self) -> usize {
        match self {
            Zone::Left => 0,
            Zone::Center => 1,
            Zone::Right => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    OnStartup,
    OnVisible,
    /// (1.1) When this bus name gets an owner.
    OnDBusName(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PanelItemDecl {
    pub id: String,
    pub name: String,
    pub zones: Vec<Zone>,
    pub icon: String,
    pub label: String,
    /// (1.1) A click toggles the extension's popover.
    pub popover: bool,
}

/// (1.1) A dock item.
#[derive(Debug, Clone, PartialEq)]
pub struct DockItemDecl {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub accepts_drops: bool,
}

/// (1.1) A command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandDecl {
    pub id: String,
    pub title: String,
    /// A suggested accelerator.
    pub keybinding: String,
}

/// A quick settings toggle or slider.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickDecl {
    pub id: String,
    pub title: String,
    pub icon: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingType {
    Bool,
    Int,
    Double,
    String,
    Enum(Vec<String>),
    StringList,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Double(f64),
    String(String),
    List(Vec<String>),
}

impl SettingValue {
    pub fn to_value(&self) -> OwnedValue {
        let v = match self {
            SettingValue::Bool(b) => Value::from(*b),
            SettingValue::Int(i) => Value::from(*i),
            SettingValue::Double(d) => Value::from(*d),
            SettingValue::String(s) => Value::from(s.clone()),
            SettingValue::List(l) => Value::from(l.clone()),
        };
        OwnedValue::try_from(v).expect("plain values convert")
    }

    pub fn to_toml(&self) -> toml::Value {
        match self {
            SettingValue::Bool(b) => toml::Value::Boolean(*b),
            SettingValue::Int(i) => toml::Value::Integer(*i),
            SettingValue::Double(d) => toml::Value::Float(*d),
            SettingValue::String(s) => toml::Value::String(s.clone()),
            SettingValue::List(l) => toml::Value::Array(l.iter().map(|s| toml::Value::String(s.clone())).collect()),
        }
    }

    /// As the Extensions dialog shows it.
    pub fn display(&self) -> String {
        match self {
            SettingValue::Bool(b) => if *b { "On" } else { "Off" }.to_owned(),
            SettingValue::Int(i) => i.to_string(),
            SettingValue::Double(d) => format!("{d:.2}"),
            SettingValue::String(s) => s.clone(),
            SettingValue::List(l) => l.join(", "),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingDecl {
    pub key: String,
    pub ty: SettingType,
    pub default: SettingValue,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub label: String,
    pub help: String,
}

impl SettingDecl {
    /// `value` if it fits this setting.
    pub fn check(&self, value: SettingValue) -> Result<SettingValue, String> {
        let in_range = |x: f64| self.min.is_none_or(|m| x >= m) && self.max.is_none_or(|m| x <= m);
        match (&self.ty, &value) {
            (SettingType::Bool, SettingValue::Bool(_)) | (SettingType::String, SettingValue::String(_)) | (SettingType::StringList, SettingValue::List(_)) => Ok(value),
            (SettingType::Int, SettingValue::Int(i)) if in_range(*i as f64) => Ok(value),
            (SettingType::Double, SettingValue::Double(d)) if d.is_finite() && in_range(*d) => Ok(value),
            (SettingType::Double, SettingValue::Int(i)) if in_range(*i as f64) => Ok(SettingValue::Double(*i as f64)),
            (SettingType::Enum(choices), SettingValue::String(s)) if choices.contains(s) => Ok(value),
            (SettingType::Int | SettingType::Double, SettingValue::Int(_) | SettingValue::Double(_)) => Err(match (self.min, self.max) {
                (Some(lo), Some(hi)) => format!("{} must be between {lo} and {hi}", self.key),
                (Some(lo), None) => format!("{} must be at least {lo}", self.key),
                (None, Some(hi)) => format!("{} must be at most {hi}", self.key),
                (None, None) => format!("{} must be a number", self.key),
            }),
            (SettingType::Enum(choices), _) => Err(format!("{} must be one of: {}", self.key, choices.join(", "))),
            _ => Err(format!("{} does not take {value:?}", self.key)),
        }
    }

    /// A D-Bus value for this setting.
    pub fn from_dbus(&self, value: &OwnedValue) -> Result<SettingValue, String> {
        let v: &Value<'_> = value;
        let v = match v {
            Value::Value(inner) => inner.as_ref(),
            other => other,
        };
        let parsed = match v {
            Value::Bool(b) => SettingValue::Bool(*b),
            Value::I64(i) => SettingValue::Int(*i),
            Value::I32(i) => SettingValue::Int(i64::from(*i)),
            Value::U32(i) => SettingValue::Int(i64::from(*i)),
            Value::F64(d) => SettingValue::Double(*d),
            Value::Str(s) => SettingValue::String(s.to_string()),
            Value::Array(_) => SettingValue::List(<Vec<String>>::try_from(v.try_clone().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?),
            other => return Err(format!("{} does not take {other:?}", self.key)),
        };
        self.check(parsed)
    }

    /// A TOML value (from the settings file) for this setting.
    pub fn from_toml(&self, value: &toml::Value) -> Result<SettingValue, String> {
        let parsed = match value {
            toml::Value::Boolean(b) => SettingValue::Bool(*b),
            toml::Value::Integer(i) => SettingValue::Int(*i),
            toml::Value::Float(f) => SettingValue::Double(*f),
            toml::Value::String(s) => SettingValue::String(s.clone()),
            toml::Value::Array(a) => SettingValue::List(a.iter().map(|v| v.as_str().map(str::to_owned).ok_or("not text")).collect::<Result<_, _>>()?),
            other => return Err(format!("{} does not take {other}", self.key)),
        };
        self.check(parsed)
    }

    /// Text typed by a user (`helix-extensions set`).
    pub fn from_text(&self, text: &str) -> Result<SettingValue, String> {
        let parsed = match &self.ty {
            SettingType::Bool => SettingValue::Bool(match text {
                "true" | "on" | "yes" => true,
                "false" | "off" | "no" => false,
                _ => return Err(format!("{} takes true or false", self.key)),
            }),
            SettingType::Int => SettingValue::Int(text.trim().parse().map_err(|_| format!("{} takes a whole number", self.key))?),
            SettingType::Double => SettingValue::Double(text.trim().parse().map_err(|_| format!("{} takes a number", self.key))?),
            SettingType::String | SettingType::Enum(_) => SettingValue::String(text.to_owned()),
            SettingType::StringList => SettingValue::List(text.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()),
        };
        self.check(parsed)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub api_minor: u32,
    pub exec: Vec<String>,
    pub license: String,
    pub url: String,
    pub activation: Vec<Activation>,
    pub permissions: Vec<String>,
    pub enabled_by_default: bool,
    pub panel_items: Vec<PanelItemDecl>,
    pub toggles: Vec<QuickDecl>,
    pub sliders: Vec<QuickDecl>,
    pub dock_items: Vec<DockItemDecl>,
    pub commands: Vec<CommandDecl>,
    /// By key.
    pub settings: Vec<SettingDecl>,
}

pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    id.len() <= 128 && parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

fn valid_item_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn text(table: &toml::Table, key: &str, max: usize, required: bool) -> Result<String, String> {
    match table.get(key) {
        Some(toml::Value::String(s)) if s.chars().count() <= max => Ok(s.clone()),
        Some(toml::Value::String(_)) => Err(format!("{key} is longer than {max} characters")),
        Some(_) => Err(format!("{key} must be text")),
        None if required => Err(format!("{key} is missing")),
        None => Ok(String::new()),
    }
}

fn texts(table: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("{key} must be a list of text"))).collect(),
        Some(_) => Err(format!("{key} must be a list of text")),
    }
}

/// Parse and check a manifest; `dir_name` is its directory's name (its
/// id must be that, unless `None`: development extensions).
pub fn parse(source: &str, dir_name: Option<&str>) -> Result<Manifest, String> {
    let table: toml::Table = source.parse().map_err(|e: toml::de::Error| format!("extension.toml: {}", e.message()))?;
    let id = text(&table, "id", 128, true)?;
    if !valid_id(&id) {
        return Err(format!("{id:?} is not a valid extension id"));
    }
    if let Some(dir) = dir_name {
        if dir != id {
            return Err(format!("the id {id} is not the directory's name ({dir})"));
        }
    }
    let api = text(&table, "api", 16, true)?;
    let api_minor = match api.split_once('.') {
        Some(("1", minor)) => minor.parse::<u32>().map_err(|_| format!("api {api:?} is not a version"))?,
        _ => return Err(format!("needs Helix extension API {api}; this Helix supports 1.0–1.{API_MINOR}")),
    };
    if api_minor > API_MINOR {
        return Err(format!("needs Helix extension API {api}; this Helix supports 1.0–1.{API_MINOR}"));
    }
    let exec = texts(&table, "exec")?;
    if exec.is_empty() || exec[0].is_empty() {
        return Err("exec is missing".into());
    }
    let activation = match table.get("activation") {
        None => vec![Activation::OnStartup],
        Some(_) => texts(&table, "activation")?
            .iter()
            .filter_map(|a| match a.as_str() {
                "onStartup" => Some(Ok(Activation::OnStartup)),
                "onVisible" => Some(Ok(Activation::OnVisible)),
                other => match other.strip_prefix("onDBusName:") {
                    // 1.1 only.
                    Some(_) if api_minor < 1 => None,
                    Some(name) if zbus::names::WellKnownName::try_from(name).is_ok() => Some(Ok(Activation::OnDBusName(name.to_owned()))),
                    Some(name) => Some(Err(format!("{name:?} is not a bus name"))),
                    None => Some(Err(format!("unknown activation {other:?}"))),
                },
            })
            .collect::<Result<_, _>>()?,
    };
    let mut permissions = texts(&table, "permissions")?;
    // `popover` is 1.1.
    if api_minor < 1 {
        permissions.retain(|p| p != "popover");
    }
    if let Some(bad) = permissions.iter().find(|p| !PERMISSIONS.iter().any(|(n, _)| n == p)) {
        return Err(format!("unknown permission {bad:?}"));
    }
    let enabled_by_default = match table.get("enabled-by-default") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err("enabled-by-default must be true or false".into()),
    };

    let contributes = match table.get("contributes") {
        None => toml::Table::new(),
        Some(toml::Value::Table(t)) => t.clone(),
        Some(_) => return Err("contributes must be a table".into()),
    };
    let entries = |kind: &str| -> Result<Vec<toml::Table>, String> {
        match contributes.get(kind) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(a)) => a.iter().map(|v| v.as_table().cloned().ok_or_else(|| format!("contributes.{kind} entries must be tables"))).collect(),
            Some(_) => Err(format!("contributes.{kind} must be a list of tables ([[contributes.{kind}]])")),
        }
    };
    let mut ids: Vec<String> = Vec::new();
    let mut take_id = |t: &toml::Table, kind: &str| -> Result<String, String> {
        let id = text(t, "id", 32, true).map_err(|e| format!("contributes.{kind}: {e}"))?;
        if !valid_item_id(&id) {
            return Err(format!("contributes.{kind}: {id:?} is not a valid item id"));
        }
        if ids.contains(&id) {
            return Err(format!("item id {id:?} is used twice"));
        }
        ids.push(id.clone());
        Ok(id)
    };
    let mut panel_items = Vec::new();
    for t in entries("panel-item")? {
        let id = take_id(&t, "panel-item")?;
        let zones = match t.get("zones") {
            None => Zone::ALL.to_vec(),
            Some(_) => texts(&t, "zones")?
                .iter()
                .map(|z| match z.as_str() {
                    "left" => Ok(Zone::Left),
                    "center" => Ok(Zone::Center),
                    "right" => Ok(Zone::Right),
                    other => Err(format!("unknown zone {other:?}")),
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if zones.is_empty() {
            return Err(format!("panel item {id} has no zones"));
        }
        let popover = api_minor >= 1 && matches!(t.get("popover"), Some(toml::Value::Boolean(true)));
        if popover && !permissions.iter().any(|p| p == "popover") {
            return Err(format!("panel item {id} has a popover but the manifest does not ask for the popover permission"));
        }
        panel_items.push(PanelItemDecl { name: text(&t, "name", 64, true)?, zones, icon: text(&t, "icon", 256, false)?, label: text(&t, "label", 64, false)?, id, popover });
    }
    let mut quick = |kind: &str| -> Result<Vec<QuickDecl>, String> {
        let mut out = Vec::new();
        for t in entries(kind)? {
            let id = take_id(&t, kind)?;
            out.push(QuickDecl { title: text(&t, "title", 64, true)?, icon: text(&t, "icon", 256, false)?, id });
        }
        Ok(out)
    };
    let toggles = quick("quick-toggle")?;
    let sliders = quick("quick-slider")?;
    // 1.1: dock items and commands.
    let mut dock_items = Vec::new();
    let mut commands = Vec::new();
    if api_minor >= 1 {
        for t in entries("dock-item")? {
            let id = take_id(&t, "dock-item")?;
            let accepts_drops = matches!(t.get("accepts-drops"), Some(toml::Value::Boolean(true)));
            dock_items.push(DockItemDecl { name: text(&t, "name", 64, true)?, icon: text(&t, "icon", 256, false)?, accepts_drops, id });
        }
        // Commands have ids of their own (they are not items).
        let mut command_ids: Vec<String> = Vec::new();
        for t in entries("command")? {
            let id = text(&t, "id", 32, true).map_err(|e| format!("contributes.command: {e}"))?;
            if !valid_item_id(&id) || command_ids.contains(&id) {
                return Err(format!("contributes.command: {id:?} is not a valid command id, or is used twice"));
            }
            command_ids.push(id.clone());
            commands.push(CommandDecl { title: text(&t, "title", 64, true)?, keybinding: text(&t, "keybinding", 64, false)?, id });
        }
    }

    let mut settings = Vec::new();
    if let Some(value) = table.get("settings") {
        let toml::Value::Table(decls) = value else { return Err("settings must be a table".into()) };
        for (key, decl) in decls {
            let toml::Value::Table(decl) = decl else { return Err(format!("settings.{key} must be a table")) };
            let ty = match text(decl, "type", 16, true)?.as_str() {
                "bool" => SettingType::Bool,
                "int" => SettingType::Int,
                "double" => SettingType::Double,
                "string" => SettingType::String,
                "string-list" => SettingType::StringList,
                "enum" => {
                    let choices = texts(decl, "choices")?;
                    if choices.is_empty() {
                        return Err(format!("settings.{key}: an enum needs choices"));
                    }
                    SettingType::Enum(choices)
                }
                other => return Err(format!("settings.{key}: unknown type {other:?}")),
            };
            let number = |k: &str| -> Result<Option<f64>, String> {
                match decl.get(k) {
                    None => Ok(None),
                    Some(toml::Value::Integer(i)) => Ok(Some(*i as f64)),
                    Some(toml::Value::Float(f)) => Ok(Some(*f)),
                    Some(_) => Err(format!("settings.{key}.{k} must be a number")),
                }
            };
            let mut setting = SettingDecl {
                key: key.clone(),
                ty,
                default: SettingValue::Bool(false),
                min: number("min")?,
                max: number("max")?,
                label: text(decl, "label", 64, true).map_err(|e| format!("settings.{key}: {e}"))?,
                help: text(decl, "help", 256, false)?,
            };
            let default = decl.get("default").ok_or_else(|| format!("settings.{key}: default is missing"))?;
            setting.default = setting.from_toml(default).map_err(|e| format!("settings.{key}: default: {e}"))?;
            settings.push(setting);
        }
    }
    Ok(Manifest {
        name: text(&table, "name", 64, true)?,
        description: text(&table, "description", 256, false)?,
        version: text(&table, "version", 64, true)?,
        license: text(&table, "license", 64, false)?,
        url: text(&table, "url", 256, false)?,
        id,
        api_minor,
        exec,
        activation,
        permissions,
        enabled_by_default,
        panel_items,
        toggles,
        sliders,
        dock_items,
        commands,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
id = "org.example.Caffeine"
name = "Caffeine"
description = "Keep the screen on."
version = "1.4.0"
api = "1.0"
exec = ["bin/caffeine", "--quiet"]
permissions = ["inhibit-idle"]
future-key = 3

[[contributes.panel-item]]
id = "indicator"
name = "Caffeine"
zones = ["right"]

[[contributes.quick-toggle]]
id = "toggle"
title = "Caffeine"

[settings]
minutes = { type = "int", default = 0, min = 0, max = 480, label = "Minutes" }
mode = { type = "enum", choices = ["a", "b"], default = "b", label = "Mode" }
"#;

    #[test]
    fn good_manifest() {
        let m = parse(GOOD, Some("org.example.Caffeine")).unwrap();
        assert_eq!((m.name.as_str(), m.api_minor, m.exec.len()), ("Caffeine", 0, 2));
        assert_eq!(m.activation, [Activation::OnStartup]);
        assert_eq!(m.panel_items[0].zones, [Zone::Right]);
        assert_eq!(m.toggles[0].title, "Caffeine");
        let minutes = m.settings.iter().find(|s| s.key == "minutes").unwrap();
        assert_eq!(minutes.default, SettingValue::Int(0));
        assert!(minutes.check(SettingValue::Int(500)).is_err());
        assert_eq!(minutes.from_text("30"), Ok(SettingValue::Int(30)));
        let mode = m.settings.iter().find(|s| s.key == "mode").unwrap();
        assert!(mode.from_text("c").is_err());
        assert_eq!(mode.from_dbus(&SettingValue::String("a".into()).to_value()), Ok(SettingValue::String("a".into())));
    }

    #[test]
    fn bad_manifests() {
        let with = |from: &str, to: &str| parse(&GOOD.replace(from, to), Some("org.example.Caffeine"));
        assert!(parse(GOOD, Some("other.Dir")).unwrap_err().contains("directory"));
        assert!(parse(GOOD, None).is_ok(), "development: any directory");
        assert!(with("api = \"1.0\"", "api = \"1.5\"").unwrap_err().contains("supports 1.0"));
        assert!(with("api = \"1.0\"", "api = \"2.0\"").is_err());
        assert!(with("\"inhibit-idle\"", "\"root\"").unwrap_err().contains("permission"));
        assert!(with("id = \"toggle\"", "id = \"indicator\"").unwrap_err().contains("twice"));
        assert!(with("zones = [\"right\"]", "zones = [\"top\"]").is_err());
        assert!(with("default = 0,", "default = \"x\",").is_err());
        assert!(with("exec = [\"bin/caffeine\", \"--quiet\"]", "exec = []").is_err());
        assert!(!valid_id("nodots") && !valid_id("a..b") && valid_id("org.example.A-b_1"));
    }

    #[test]
    fn api_1_1() {
        let extra = "\nactivation = [\"onDBusName:org.mpris.MediaPlayer2.x\"]\n[[contributes.dock-item]]\nid = \"stack\"\nname = \"Stack\"\naccepts-drops = true\n[[contributes.command]]\nid = \"stack\"\ntitle = \"Go\"\nkeybinding = \"<Super>g\"\n";
        let base = "id = \"org.x.A\"\nname = \"A\"\nversion = \"1\"\nexec = [\"x\"]\npermissions = [\"popover\"]\n";
        let m = parse(&format!("{base}api = \"1.1\"{extra}"), None).unwrap();
        assert_eq!(m.activation, [Activation::OnDBusName("org.mpris.MediaPlayer2.x".into())]);
        assert!(m.dock_items[0].accepts_drops);
        assert_eq!(m.commands[0].keybinding, "<Super>g");
        assert_eq!(m.permissions, ["popover"]);
        // Under 1.0 they are ignored.
        let old = parse(&format!("{base}api = \"1.0\"{extra}"), None).unwrap();
        assert!(old.activation.is_empty() && old.dock_items.is_empty() && old.commands.is_empty() && old.permissions.is_empty());
        let popover = parse("id = \"org.x.A\"\nname = \"A\"\nversion = \"1\"\nexec = [\"x\"]\napi = \"1.1\"\n[[contributes.panel-item]]\nid = \"p\"\nname = \"P\"\npopover = true\n", None);
        assert!(popover.unwrap_err().contains("popover permission"));
    }
}
