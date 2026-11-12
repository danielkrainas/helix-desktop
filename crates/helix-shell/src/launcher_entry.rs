//! Counts, progress and urgency apps put on their dock icon (helix-desktop-spec
//! `contracts/dbus/com.canonical.Unity.LauncherEntry.md`): the
//! `com.canonical.Unity.LauncherEntry` `Update` signal, which apps send on
//! the session bus.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

use crate::UiEvent;

pub const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
pub const DBUSMENU: &str = "com.canonical.dbusmenu";

/// An entry of an app's quicklist (its dbusmenu).
#[derive(Debug, Clone, PartialEq)]
pub enum QuickItem {
    Action { id: i32, label: String, enabled: bool, checked: Option<bool> },
    Separator,
    /// An entry with its own entries.
    Submenu { label: String, items: Vec<QuickItem> },
}

/// Where an app's quicklist is: its bus name and object path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quicklist {
    pub sender: String,
    pub path: String,
}

/// A dbusmenu label without its mnemonic underscores (`__` is a `_`).
pub fn plain_label(label: &str) -> String {
    let mut out = String::new();
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// One dbusmenu layout node: id, properties, children.
pub type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// The entries of a dbusmenu layout's root: visible items in order,
/// entries with `children-display` `submenu` as submenus (of their own
/// visible entries; none left: left out), separators kept between items
/// only (in each menu).
pub fn quick_items(root: &Node) -> Vec<QuickItem> {
    fn walk(node: &Node) -> Vec<QuickItem> {
        let mut out = Vec::new();
        for child in &node.2 {
            let Ok(child) = child.try_clone().map_err(|_| ()).and_then(|v| Node::try_from(v).map_err(|_| ())) else { continue };
            let props = &child.1;
            let get = |k: &str| props.get(k).and_then(|v| v.try_clone().ok());
            if get("visible").and_then(|v| bool::try_from(v).ok()) == Some(false) {
                continue;
            }
            if get("type").and_then(|v| String::try_from(v).ok()).as_deref() == Some("separator") {
                out.push(QuickItem::Separator);
                continue;
            }
            let label = get("label").and_then(|v| String::try_from(v).ok()).map(|l| plain_label(&l)).unwrap_or_default();
            if get("children-display").and_then(|v| String::try_from(v).ok()).as_deref() == Some("submenu") {
                let items = walk(&child);
                if items.iter().any(|i| *i != QuickItem::Separator) {
                    out.push(QuickItem::Submenu { label: if label.is_empty() { "More".into() } else { label }, items });
                }
                continue;
            }
            if label.is_empty() {
                continue;
            }
            let enabled = get("enabled").and_then(|v| bool::try_from(v).ok()).unwrap_or(true);
            let toggle = get("toggle-type").and_then(|v| String::try_from(v).ok()).filter(|t| !t.is_empty());
            let checked = toggle.map(|_| get("toggle-state").and_then(|v| i32::try_from(v).ok()) == Some(1));
            out.push(QuickItem::Action { id: child.0, label, enabled, checked });
        }
        // No separators at the ends or twice in a row.
        let mut tidy: Vec<QuickItem> = Vec::new();
        for item in out {
            if item == QuickItem::Separator && tidy.last().is_none_or(|l| *l == QuickItem::Separator) {
                continue;
            }
            tidy.push(item);
        }
        while tidy.last() == Some(&QuickItem::Separator) {
            tidy.pop();
        }
        tidy
    }
    walk(root)
}

/// Read an app's quicklist.
pub async fn fetch_quicklist(conn: &Connection, list: &Quicklist) -> zbus::Result<Vec<QuickItem>> {
    let _ = conn.call_method(Some(list.sender.as_str()), list.path.as_str(), Some(DBUSMENU), "AboutToShow", &(0i32,)).await;
    let reply = conn.call_method(Some(list.sender.as_str()), list.path.as_str(), Some(DBUSMENU), "GetLayout", &(0i32, -1i32, Vec::<String>::new())).await?;
    let (_revision, root): (u32, Node) = reply.body().deserialize()?;
    Ok(quick_items(&root))
}

/// Tell the app an entry of its quicklist was chosen.
pub async fn activate_quick_item(conn: &Connection, list: &Quicklist, id: i32) -> zbus::Result<()> {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
    conn.call_method(Some(list.sender.as_str()), list.path.as_str(), Some(DBUSMENU), "Event", &(id, "clicked", zbus::zvariant::Value::from(""), time)).await.map(drop)
}

/// What an app shows on its icon.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub count: i64,
    pub count_visible: bool,
    /// 0 to 1.
    pub progress: f64,
    pub progress_visible: bool,
    pub urgent: bool,
    /// The app's quicklist (`quicklist` property) and its entries.
    pub quicklist: Option<Quicklist>,
    pub quick_items: Vec<QuickItem>,
}

impl Entry {
    /// The count to show, if any.
    pub fn shown_count(&self) -> Option<i64> {
        (self.count_visible && self.count > 0).then_some(self.count)
    }

    /// The progress to show, if any (kept within 0 and 1).
    pub fn shown_progress(&self) -> Option<f64> {
        self.progress_visible.then_some(self.progress.clamp(0.0, 1.0))
    }

    /// Apply an update's properties (others keep their values).
    pub fn apply(&mut self, props: &HashMap<String, OwnedValue>) {
        let get = |k: &str| props.get(k).and_then(|v| v.try_clone().ok());
        if let Some(v) = get("count") {
            self.count = i64::try_from(v.clone()).or_else(|_| i32::try_from(v.clone()).map(i64::from)).or_else(|_| u32::try_from(v).map(i64::from)).unwrap_or(self.count);
        }
        if let Some(b) = get("count-visible").and_then(|v| bool::try_from(v).ok()) {
            self.count_visible = b;
        }
        if let Some(p) = get("progress").and_then(|v| f64::try_from(v).ok()) {
            self.progress = p;
        }
        if let Some(b) = get("progress-visible").and_then(|v| bool::try_from(v).ok()) {
            self.progress_visible = b;
        }
        if let Some(b) = get("urgent").and_then(|v| bool::try_from(v).ok()) {
            self.urgent = b;
        }
        if let Some(path) = get("quicklist").and_then(|v| String::try_from(v.clone()).ok().or_else(|| zbus::zvariant::OwnedObjectPath::try_from(v).ok().map(|p| p.to_string()))) {
            self.quicklist = (!path.is_empty() && path != "/").then(|| Quicklist { sender: String::new(), path });
            if self.quicklist.is_none() {
                self.quick_items.clear();
            }
        }
    }
}

/// The app id (desktop file id without `.desktop`) an `app_uri`
/// (`application://<id>.desktop`) names.
pub fn app_of(app_uri: &str) -> Option<String> {
    let id = app_uri.strip_prefix("application://")?;
    let id = id.strip_suffix(".desktop").unwrap_or(id);
    (!id.is_empty()).then(|| id.to_owned())
}

/// Follow apps' updates for the rest of the session: the UI gets each
/// app's entry as it changes (`None` once its sender left the bus).
pub async fn follow(conn: Connection, ui: mpsc::UnboundedSender<UiEvent>) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(INTERFACE)?.member("Update")?.build();
    let mut updates = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
    let mut owners = zbus::fdo::DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
    let layout_rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(DBUSMENU)?.member("LayoutUpdated")?.build();
    let mut layouts = zbus::MessageStream::for_match_rule(layout_rule, &conn, None).await?;
    let mut entries: HashMap<String, Entry> = HashMap::new();
    // Sender → the apps it updated.
    let mut senders: HashMap<String, Vec<String>> = HashMap::new();
    loop {
        tokio::select! {
            message = updates.next() => {
                let Some(Ok(message)) = message else { break };
                let Ok((uri, props)) = message.body().deserialize::<(String, HashMap<String, OwnedValue>)>() else { continue };
                let Some(app) = app_of(&uri) else { continue };
                let entry = entries.entry(app.clone()).or_default();
                let had = entry.quicklist.clone();
                entry.apply(&props);
                if let (Some(list), Some(sender)) = (entry.quicklist.as_mut(), message.header().sender()) {
                    list.sender = sender.to_string();
                }
                if entry.quicklist.is_some() && entry.quicklist != had {
                    let list = entry.quicklist.clone().expect("checked");
                    entry.quick_items = fetch_quicklist(&conn, &list).await.unwrap_or_default();
                }
                if let Some(sender) = message.header().sender() {
                    let apps = senders.entry(sender.to_string()).or_default();
                    if !apps.contains(&app) {
                        apps.push(app.clone());
                    }
                }
                let _ = ui.send(UiEvent::LauncherEntry(app, Some(entry.clone())));
            }
            layout = layouts.next() => {
                let Some(Ok(message)) = layout else { continue };
                let header = message.header();
                let (Some(sender), Some(path)) = (header.sender().map(|s| s.to_string()), header.path().map(|p| p.to_string())) else { continue };
                let matching: Vec<String> = entries
                    .iter()
                    .filter(|(_, e)| e.quicklist.as_ref().is_some_and(|q| q.sender == sender && q.path == path))
                    .map(|(a, _)| a.clone())
                    .collect();
                for app in matching {
                    let Some(entry) = entries.get_mut(&app) else { continue };
                    let list = entry.quicklist.clone().expect("matched");
                    entry.quick_items = fetch_quicklist(&conn, &list).await.unwrap_or_default();
                    let _ = ui.send(UiEvent::LauncherEntry(app, Some(entry.clone())));
                }
            }
            change = owners.next() => {
                let Some(change) = change else { break };
                let Ok(args) = change.args() else { continue };
                if args.new_owner().is_some() {
                    continue;
                }
                for app in senders.remove(args.name().as_str()).unwrap_or_default() {
                    entries.remove(&app);
                    let _ = ui.send(UiEvent::LauncherEntry(app, None));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn o(v: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    fn node(id: i32, props: Vec<(&str, Value<'_>)>, children: Vec<Node>) -> Node {
        let props = props.into_iter().map(|(k, v)| (k.to_owned(), o(v))).collect();
        let children = children.into_iter().map(|c| o(Value::from(c))).collect();
        (id, props, children)
    }

    #[test]
    fn quicklists() {
        assert_eq!(plain_label("_New Window"), "New Window");
        assert_eq!(plain_label("Save__as"), "Save_as");
        let root = node(
            0,
            vec![("children-display", Value::from("submenu"))],
            vec![
                node(1, vec![("type", Value::from("separator"))], vec![]),
                node(2, vec![("label", Value::from("_New Window"))], vec![]),
                node(3, vec![("label", Value::from("Hidden")), ("visible", Value::from(false))], vec![]),
                node(4, vec![("type", Value::from("separator"))], vec![]),
                node(5, vec![("type", Value::from("separator"))], vec![]),
                node(6, vec![("label", Value::from("Offline")), ("toggle-type", Value::from("checkmark")), ("toggle-state", Value::from(1i32))], vec![]),
                node(
                    7,
                    vec![("label", Value::from("More")), ("children-display", Value::from("submenu"))],
                    vec![node(8, vec![("label", Value::from("Greyed")), ("enabled", Value::from(false))], vec![])],
                ),
                node(9, vec![("type", Value::from("separator"))], vec![]),
            ],
        );
        assert_eq!(
            quick_items(&root),
            [
                QuickItem::Action { id: 2, label: "New Window".into(), enabled: true, checked: None },
                QuickItem::Separator,
                QuickItem::Action { id: 6, label: "Offline".into(), enabled: true, checked: Some(true) },
                QuickItem::Submenu { label: "More".into(), items: vec![QuickItem::Action { id: 8, label: "Greyed".into(), enabled: false, checked: None }] },
            ]
        );
        let mut e = Entry::default();
        e.apply(&HashMap::from([("quicklist".to_owned(), o(Value::from("/com/example/menu")))]));
        assert_eq!(e.quicklist.as_ref().map(|q| q.path.as_str()), Some("/com/example/menu"));
        e.apply(&HashMap::from([("quicklist".to_owned(), o(Value::from("")))]));
        assert_eq!(e.quicklist, None);
    }

    #[test]
    fn updates() {
        assert_eq!(app_of("application://firefox.desktop").as_deref(), Some("firefox"));
        assert_eq!(app_of("application://"), None);
        assert_eq!(app_of("file:///x"), None);
        let mut e = Entry::default();
        e.apply(&HashMap::from([("count".to_owned(), o(Value::from(5i64))), ("count-visible".to_owned(), o(Value::from(true)))]));
        assert_eq!(e.shown_count(), Some(5));
        e.apply(&HashMap::from([("progress".to_owned(), o(Value::from(1.4f64))), ("progress-visible".to_owned(), o(Value::from(true)))]));
        assert_eq!((e.shown_count(), e.shown_progress()), (Some(5), Some(1.0)), "count kept, progress clamped");
        e.apply(&HashMap::from([("count-visible".to_owned(), o(Value::from(false))), ("urgent".to_owned(), o(Value::from(true)))]));
        assert_eq!((e.shown_count(), e.urgent), (None, true));
    }
}
