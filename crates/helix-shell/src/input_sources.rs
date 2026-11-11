//! Switching input sources (helix-desktop-spec
//! `contracts/gsettings/input-settings.md`): the shell binds the
//! `switch-input-source` keys and moves the chosen source to the front of
//! `mru-sources`; the compositor follows that setting.

use gio::prelude::*;

const INPUT_SOURCES: &str = "org.gnome.desktop.input-sources";
const KEYBINDINGS: &str = "org.gnome.desktop.wm.keybindings";

pub type Source = (String, String);

/// The source after (or before) the active one, in `sources` order, and the
/// most-recently-used list with it in front.
pub fn next(sources: &[Source], mru: &[Source], backward: bool) -> Option<(Source, Vec<Source>)> {
    if sources.is_empty() {
        return None;
    }
    let active = mru.iter().find_map(|m| sources.iter().position(|s| s == m)).unwrap_or(0);
    let n = sources.len();
    let index = if backward { (active + n - 1) % n } else { (active + 1) % n };
    let chosen = sources[index].clone();
    let mut new_mru = vec![chosen.clone()];
    new_mru.extend(mru.iter().filter(|m| **m != chosen).cloned());
    Some((chosen, new_mru))
}

fn settings(schema: &str) -> Option<gio::Settings> {
    helix_settings::has_schema(schema).then(|| gio::Settings::new(schema))
}

/// Switch to the next (or previous) source; returns its id.
pub fn switch(backward: bool) -> Option<String> {
    let settings = settings(INPUT_SOURCES)?;
    let read = |key: &str| settings.value(key).get::<Vec<Source>>().unwrap_or_default();
    let (chosen, mru) = next(&read("sources"), &read("mru-sources"), backward)?;
    if let Err(err) = settings.set_value("mru-sources", &mru.to_variant()) {
        tracing::warn!(%err, "cannot switch input source");
        return None;
    }
    gio::Settings::sync();
    tracing::info!(source = %chosen.1, "input source switched");
    Some(chosen.1)
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
    fn cycles_in_sources_order() {
        let sources = [s("us"), s("de"), s("fr")];
        assert_eq!(next(&[], &[], false), None);
        let (chosen, mru) = next(&sources, &[], false).unwrap();
        assert_eq!(chosen, s("de"), "from the first source");
        assert_eq!(mru, [s("de")]);
        let (chosen, mru) = next(&sources, &[s("fr"), s("de"), s("us")], false).unwrap();
        assert_eq!(chosen, s("us"), "wraps around");
        assert_eq!(mru, [s("us"), s("fr"), s("de")]);
        let (chosen, _) = next(&sources, &[s("gone"), s("us")], true).unwrap();
        assert_eq!(chosen, s("fr"), "backward from the first configured MRU entry");
    }
}
