//! Switching input sources (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`): the shell binds the
//! `switch-input-source` keys, lists the sources most recently used first,
//! and moves the chosen one to the front of `mru-sources`; the compositor
//! follows that setting.

use std::collections::HashMap;

use gio::prelude::*;

const INPUT_SOURCES: &str = "org.gnome.desktop.input-sources";
const KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";

pub type Source = (String, String);

/// The configured sources, most recently used first: the `mru-sources`
/// entries that are in `sources`, then the others in `sources` order.
pub fn mru_order(sources: &[Source], mru: &[Source]) -> Vec<Source> {
    let mut out: Vec<Source> = Vec::new();
    for s in mru.iter().chain(sources) {
        if sources.contains(s) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

/// `mru-sources` with `chosen` in front.
pub fn with_front(mru: &[Source], chosen: &Source) -> Vec<Source> {
    let mut out = vec![chosen.clone()];
    out.extend(mru.iter().filter(|m| *m != chosen).cloned());
    out
}

fn settings(schema: &str) -> Option<gio::Settings> {
    helix_settings::has_schema(schema).then(|| gio::Settings::new(schema))
}

/// The XKB rules listing layouts and variants with their descriptions.
fn rules_path() -> std::path::PathBuf {
    let root = std::env::var_os("XKB_CONFIG_ROOT").map(std::path::PathBuf::from).unwrap_or_else(|| "/usr/share/X11/xkb".into());
    root.join("rules/evdev.xml")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Layout descriptions from an `evdev.xml`: id (`layout` or
/// `layout+variant`) → description.
pub fn parse_descriptions(xml: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    let (mut layout, mut variant) = (String::new(), String::new());
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let text = rest[..start].trim();
        if !text.is_empty() && stack.len() >= 3 {
            let n = stack.len();
            let (owner, item, field) = (&stack[n - 3], &stack[n - 2], &stack[n - 1]);
            if item == "configItem" && (owner == "layout" || owner == "variant") {
                let value = unescape(text);
                match (owner.as_str(), field.as_str()) {
                    ("layout", "name") => layout = value,
                    ("variant", "name") => variant = value,
                    ("layout", "description") if !layout.is_empty() => {
                        out.insert(layout.clone(), value);
                    }
                    ("variant", "description") if !layout.is_empty() && !variant.is_empty() => {
                        out.insert(format!("{layout}+{variant}"), value);
                    }
                    _ => {}
                }
            }
        }
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if tag.starts_with('?') || tag.starts_with('!') || tag.ends_with('/') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            if stack.last().map(String::as_str) == Some(name.trim()) {
                stack.pop();
            }
            if name.trim() == "layout" {
                layout.clear();
            }
            if name.trim() == "variant" {
                variant.clear();
            }
        } else {
            stack.push(tag.split_whitespace().next().unwrap_or("").to_owned());
        }
    }
    out
}

/// The description of a layout id, from the system's XKB rules.
pub fn description(id: &str) -> Option<String> {
    static DESCRIPTIONS: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    DESCRIPTIONS
        .get_or_init(|| std::fs::read_to_string(rules_path()).map(|xml| parse_descriptions(&xml)).unwrap_or_default())
        .get(id)
        .cloned()
}

/// The sources, most recently used first.
pub fn current() -> Vec<Source> {
    let Some(settings) = settings(INPUT_SOURCES) else { return Vec::new() };
    let read = |key: &str| settings.value(key).get::<Vec<Source>>().unwrap_or_default();
    mru_order(&read("sources"), &read("mru-sources"))
}

/// Make `chosen` the active source.
pub fn activate(chosen: &Source) {
    let Some(settings) = settings(INPUT_SOURCES) else { return };
    let mru = settings.value("mru-sources").get::<Vec<Source>>().unwrap_or_default();
    if let Err(err) = settings.set_value("mru-sources", &with_front(&mru, chosen).to_variant()) {
        tracing::warn!(%err, "cannot switch input source");
        return;
    }
    gio::Settings::sync();
    tracing::info!(source = %chosen.1, "input source switched");
}

/// The configured switch keys: (accelerator, backward).
pub fn accelerators() -> Vec<(String, bool)> {
    let Some(settings) = settings(KEYBINDINGS) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, backward) in [("switch-input-source", false), ("switch-input-source-backward", true)] {
        out.extend(settings.strv(key).iter().map(|a| (a.to_string(), backward)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str) -> Source {
        ("xkb".into(), id.into())
    }

    #[test]
    fn layout_descriptions() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE xkbConfigRegistry SYSTEM "xkb.dtd">
<xkbConfigRegistry version="1.1">
  <modelList><model><configItem><name>pc105</name><description>Generic 105-key PC</description></configItem></model></modelList>
  <layoutList>
    <layout>
      <configItem>
        <name>de</name>
        <!-- Keyboard indicator -->
        <shortDescription>de</shortDescription>
        <description>German</description>
      </configItem>
      <variantList>
        <variant>
          <configItem>
            <name>nodeadkeys</name>
            <description>German (no dead keys)</description>
          </configItem>
        </variant>
      </variantList>
    </layout>
    <layout>
      <configItem><name>us</name><description>English (US)</description></configItem>
      <variantList><variant><configItem><name>dvorak</name><description>English (Dvorak) &amp; more</description></configItem></variant></variantList>
    </layout>
  </layoutList>
</xkbConfigRegistry>"#;
        let d = parse_descriptions(xml);
        assert_eq!(d.get("de").map(String::as_str), Some("German"));
        assert_eq!(d.get("de+nodeadkeys").map(String::as_str), Some("German (no dead keys)"));
        assert_eq!(d.get("us+dvorak").map(String::as_str), Some("English (Dvorak) & more"));
        assert!(!d.contains_key("pc105"), "models are not layouts");
        // The real file, when installed.
        if rules_path().exists() {
            assert_eq!(description("us").as_deref(), Some("English (US)"));
        }
    }

    #[test]
    fn most_recently_used_first() {
        let sources = [s("us"), s("de"), s("fr")];
        assert_eq!(mru_order(&[], &[s("us")]), Vec::<Source>::new());
        assert_eq!(mru_order(&sources, &[]), sources, "never used: sources order");
        assert_eq!(mru_order(&sources, &[s("gone"), s("fr"), s("us")]), [s("fr"), s("us"), s("de")]);
        assert_eq!(with_front(&[s("fr"), s("us")], &s("us")), [s("us"), s("fr")]);
        assert_eq!(with_front(&[s("fr")], &s("de")), [s("de"), s("fr")]);
    }
}
