//! App search providers (helix-desktop-spec
//! `org.gnome.Shell.SearchProvider2.md`): finding them and asking them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

use crate::launcher::SearchHit;
use crate::UiEvent;

const IFACE: &str = "org.gnome.Shell.SearchProvider2";
const SETTINGS: &str = "org.gnome.desktop.search-providers";
const TIMEOUT: Duration = Duration::from_secs(5);
/// Results shown per provider.
pub const PER_PROVIDER: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub desktop_id: String,
    pub app: String,
    pub bus_name: String,
    pub path: String,
    pub default_disabled: bool,
}

/// Parse a provider key file.
pub fn parse(text: &str) -> Option<Provider> {
    let mut in_group = false;
    let mut keys: HashMap<&str, &str> = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_group = line == "[Shell Search Provider]";
            continue;
        }
        if in_group && !line.starts_with('#') {
            if let Some((k, v)) = line.split_once('=') {
                keys.insert(k.trim(), v.trim());
            }
        }
    }
    if keys.get("Version").copied() != Some("2") {
        return None;
    }
    Some(Provider {
        desktop_id: keys.get("DesktopId")?.to_string(),
        app: String::new(),
        bus_name: keys.get("BusName")?.to_string(),
        path: keys.get("ObjectPath")?.to_string(),
        default_disabled: keys.get("DefaultDisabled").is_some_and(|v| *v == "true"),
    })
}

/// Which providers to ask, in order: `found` (desktop id → app name known)
/// filtered and sorted by the settings.
pub fn arrange(found: Vec<Provider>, disable_external: bool, disabled: &[String], enabled: &[String], order: &[String]) -> Vec<Provider> {
    if disable_external {
        return Vec::new();
    }
    let mut out: Vec<Provider> = found
        .into_iter()
        .filter(|p| !disabled.contains(&p.desktop_id))
        .filter(|p| !p.default_disabled || enabled.contains(&p.desktop_id))
        .collect();
    out.sort_by(|a, b| {
        let rank = |p: &Provider| order.iter().position(|d| *d == p.desktop_id).unwrap_or(usize::MAX);
        rank(a).cmp(&rank(b)).then_with(|| a.app.to_lowercase().cmp(&b.app.to_lowercase()))
    });
    out
}

/// The search terms of a query.
pub fn terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_owned).collect()
}

/// Whether `new` refines `previous` (a subsearch can be used).
pub fn refines(previous: &[String], new: &[String]) -> bool {
    !previous.is_empty() && new.len() >= previous.len() && previous.iter().zip(new).all(|(p, n)| n.starts_with(p.as_str()))
}

fn strv(settings: &gio::Settings, key: &str) -> Vec<String> {
    settings.strv(key).iter().map(|s| s.to_string()).collect()
}

/// The enabled providers now.
pub fn providers() -> Vec<Provider> {
    let mut dirs = vec![gio::glib::user_data_dir()];
    dirs.extend(gio::glib::system_data_dirs());
    let apps: HashMap<String, String> = gio::AppInfo::all().into_iter().filter_map(|a| Some((a.id()?.to_string(), a.display_name().to_string()))).collect();
    let mut seen = Vec::new();
    let mut found = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir.join("gnome-shell/search-providers")) else { continue };
        let mut files: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "ini")).collect();
        files.sort();
        for file in files {
            let name = file.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            if seen.contains(&name) {
                continue;
            }
            seen.push(name);
            let Some(mut provider) = std::fs::read_to_string(&file).ok().and_then(|t| parse(&t)) else { continue };
            let Some(app) = apps.get(&provider.desktop_id) else { continue };
            provider.app = app.clone();
            found.push(provider);
        }
    }
    let settings = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(SETTINGS, true)).map(|_| gio::Settings::new(SETTINGS));
    match settings {
        Some(s) => arrange(found, s.boolean("disable-external"), &strv(&s, "disabled"), &strv(&s, "enabled"), &strv(&s, "sort-order")),
        None => arrange(found, false, &[], &[], &[]),
    }
}

fn meta_str(meta: &HashMap<String, OwnedValue>, key: &str) -> String {
    meta.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

/// A result's icon: `icon` (a serialized `GIcon`), else `icon-data`.
pub fn result_icon(meta: &HashMap<String, OwnedValue>) -> Option<crate::icons::IconSource> {
    meta.get("icon")
        .and_then(|v| crate::icons::IconSource::from_gicon(v))
        .or_else(|| meta.get("icon-data").and_then(|v| crate::icons::IconSource::from_image_data(v)))
}

/// Ask one provider: its result ids (all) and the hits to show.
async fn ask(conn: &Connection, provider: &Provider, terms: &[String], previous: Option<&[String]>) -> zbus::Result<(Vec<String>, Vec<SearchHit>)> {
    let (dest, path) = (provider.bus_name.as_str(), provider.path.as_str());
    let reply = match previous {
        Some(prev) => conn.call_method(Some(dest), path, Some(IFACE), "GetSubsearchResultSet", &(prev, terms)).await?,
        None => conn.call_method(Some(dest), path, Some(IFACE), "GetInitialResultSet", &(terms,)).await?,
    };
    let ids: Vec<String> = reply.body().deserialize()?;
    let shown: Vec<&String> = ids.iter().take(PER_PROVIDER).collect();
    if shown.is_empty() {
        return Ok((ids, Vec::new()));
    }
    let reply = conn.call_method(Some(dest), path, Some(IFACE), "GetResultMetas", &(shown,)).await?;
    let metas: Vec<HashMap<String, OwnedValue>> = reply.body().deserialize()?;
    let hits = metas
        .iter()
        .map(|m| SearchHit {
            provider: 0,
            app: provider.app.clone(),
            id: meta_str(m, "id"),
            name: meta_str(m, "name"),
            description: meta_str(m, "description"),
            icon: result_icon(m),
            clipboard: Some(meta_str(m, "clipboardText")).filter(|t| !t.is_empty()),
        })
        .filter(|h| !h.id.is_empty())
        .collect();
    Ok((ids, hits))
}

/// Per provider (by desktop id): the terms and result ids of its last
/// answer.
type Previous = HashMap<String, (Vec<String>, Vec<String>)>;

/// Searches for the launcher.
#[derive(Clone, Default)]
pub struct Searcher {
    providers: Arc<Mutex<Vec<Provider>>>,
    previous: Arc<Mutex<Previous>>,
}

impl Searcher {
    /// Search for `query` (`serial` tells answers apart); an empty query
    /// (the launcher opened) reads the providers again.
    /// The providers are read before this returns, so searches that
    /// follow use them.
    pub async fn search(&self, conn: &Connection, ui: &mpsc::UnboundedSender<UiEvent>, serial: u64, query: &str) {
        if query.trim().is_empty() {
            let found = tokio::task::spawn_blocking(self::providers).await.unwrap_or_default();
            tracing::debug!(count = found.len(), "search providers");
            *self.providers.lock().unwrap_or_else(|e| e.into_inner()) = found;
            self.previous.lock().unwrap_or_else(|e| e.into_inner()).clear();
            return;
        }
        let terms = terms(query);
        let list = self.providers.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (index, provider) in list.into_iter().enumerate() {
            let (conn, ui, terms, previous) = (conn.clone(), ui.clone(), terms.clone(), self.previous.clone());
            tokio::spawn(async move {
                let prev = previous.lock().unwrap_or_else(|e| e.into_inner()).get(&provider.desktop_id).cloned();
                let sub = prev.as_ref().filter(|(t, _)| refines(t, &terms)).map(|(_, ids)| ids.clone());
                match tokio::time::timeout(TIMEOUT, ask(&conn, &provider, &terms, sub.as_deref())).await {
                    Ok(Ok((ids, mut hits))) => {
                        previous.lock().unwrap_or_else(|e| e.into_inner()).insert(provider.desktop_id.clone(), (terms, ids));
                        hits.iter_mut().for_each(|h| h.provider = index);
                        let _ = ui.send(UiEvent::SearchResults { serial, provider: index, hits });
                    }
                    Ok(Err(err)) => tracing::debug!(%err, provider = provider.desktop_id, "search provider failed"),
                    Err(_) => tracing::debug!(provider = provider.desktop_id, "search provider too slow"),
                }
            });
        }
    }

    /// Open the provider's app with the search (`LaunchSearch`).
    pub async fn launch(&self, conn: &Connection, provider: usize, query: &str) {
        let Some(p) = self.providers.lock().unwrap_or_else(|e| e.into_inner()).get(provider).cloned() else { return };
        if let Err(err) = conn.call_method(Some(p.bus_name.as_str()), p.path.as_str(), Some(IFACE), "LaunchSearch", &(terms(query), 0u32)).await {
            tracing::warn!(%err, provider = p.desktop_id, "cannot open the search in its app");
        }
    }

    /// The user chose a result.
    pub async fn activate(&self, conn: &Connection, provider: usize, id: &str, query: &str) {
        let Some(p) = self.providers.lock().unwrap_or_else(|e| e.into_inner()).get(provider).cloned() else { return };
        let terms = terms(query);
        if let Err(err) = conn.call_method(Some(p.bus_name.as_str()), p.path.as_str(), Some(IFACE), "ActivateResult", &(id, terms, 0u32)).await {
            tracing::warn!(%err, provider = p.desktop_id, "cannot open the search result");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metas() {
        use zbus::zvariant::Value;
        let o = |v: Value<'_>| OwnedValue::try_from(v).unwrap();
        let themed = o(Value::from(("themed", Value::from(vec!["accessories-calculator"]))));
        let m = HashMap::from([("id".to_owned(), o(Value::from("r1"))), ("icon".to_owned(), themed), ("clipboardText".to_owned(), o(Value::from("42")))]);
        assert_eq!(result_icon(&m), Some(crate::icons::IconSource::Names(vec!["accessories-calculator".into()])));
        assert_eq!(meta_str(&m, "clipboardText"), "42");
        assert_eq!(result_icon(&HashMap::new()), None);
        let hit = |p: usize| crate::launcher::SearchHit { provider: p, app: String::new(), id: String::new(), name: String::new(), description: String::new(), icon: None, clipboard: None };
        use crate::launcher::Hit;
        assert_eq!(crate::launcher::search_rows(&[hit(0), hit(0), hit(2)]), [Hit::Search(0), Hit::Search(1), Hit::More(1), Hit::Search(2), Hit::More(2)]);
        assert!(crate::launcher::search_rows(&[]).is_empty());
    }

    fn provider(id: &str, app: &str, default_disabled: bool) -> Provider {
        Provider { desktop_id: id.into(), app: app.into(), bus_name: "b".into(), path: "/p".into(), default_disabled }
    }

    #[test]
    fn key_files() {
        let p = parse("# c\n[Shell Search Provider]\nDesktopId=org.gnome.Calculator.desktop\nBusName=org.gnome.Calculator.SearchProvider\nObjectPath=/org/gnome/Calculator/SearchProvider\nVersion=2\n").unwrap();
        assert_eq!(p.bus_name, "org.gnome.Calculator.SearchProvider");
        assert!(!p.default_disabled);
        assert!(parse("[Shell Search Provider]\nDesktopId=a\nBusName=b\nObjectPath=/c\nVersion=1\n").is_none());
        assert!(parse("[Shell Search Provider]\nDesktopId=a\nBusName=b\nObjectPath=/c\nVersion=2\nDefaultDisabled=true\n").unwrap().default_disabled);
    }

    #[test]
    fn settings_and_order() {
        let found = vec![provider("c.desktop", "Characters", false), provider("s.desktop", "Settings", false), provider("t.desktop", "Terminal", true), provider("a.desktop", "Atlas", false)];
        let ids = |v: Vec<Provider>| v.into_iter().map(|p| p.desktop_id).collect::<Vec<_>>();
        assert_eq!(ids(arrange(found.clone(), false, &[], &[], &["s.desktop".into()])), ["s.desktop", "a.desktop", "c.desktop"]);
        assert_eq!(ids(arrange(found.clone(), false, &["c.desktop".into()], &["t.desktop".into()], &[])), ["a.desktop", "s.desktop", "t.desktop"]);
        assert!(arrange(found, true, &[], &[], &[]).is_empty());
    }

    #[test]
    fn subsearches() {
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(refines(&t(&["ca"]), &t(&["cal"])));
        assert!(refines(&t(&["ca"]), &t(&["cal", "x"])));
        assert!(!refines(&t(&["cal"]), &t(&["ca"])));
        assert!(!refines(&t(&["ca", "x"]), &t(&["cal"])));
        assert!(!refines(&[], &t(&["a"])));
        assert_eq!(terms("  2 +  2 "), ["2", "+", "2"]);
    }
}
