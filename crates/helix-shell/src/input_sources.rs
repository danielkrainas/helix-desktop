//! Switching input sources (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`): the shell binds the
//! `switch-input-source` keys, lists the sources most recently used first,
//! and moves the chosen one to the front of `mru-sources`; the compositor
//! follows that setting.

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
    fn most_recently_used_first() {
        let sources = [s("us"), s("de"), s("fr")];
        assert_eq!(mru_order(&[], &[s("us")]), Vec::<Source>::new());
        assert_eq!(mru_order(&sources, &[]), sources, "never used: sources order");
        assert_eq!(mru_order(&sources, &[s("gone"), s("fr"), s("us")]), [s("fr"), s("us"), s("de")]);
        assert_eq!(with_front(&[s("fr"), s("us")], &s("us")), [s("us"), s("fr")]);
        assert_eq!(with_front(&[s("fr")], &s("de")), [s("de"), s("fr")]);
    }
}
