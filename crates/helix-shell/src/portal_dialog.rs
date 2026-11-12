//! `org.helix.PortalDialog` (helix-desktop-spec
//! `contracts/dbus/org.helix.PortalDialog.xml`): dialogs and running apps
//! for Helix's portal backends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const PATH: &str = "/org/helix/PortalDialog";

/// The index of `selected` among `items` (0 when absent).
pub fn initial_index(items: &[(String, String)], selected: &str) -> usize {
    items.iter().position(|(id, _)| id == selected).unwrap_or(0)
}

fn get<T: TryFrom<OwnedValue>>(options: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    options.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

/// The prompt `Dialog`'s options describe (without the picture).
pub fn dialog_prompt(options: &HashMap<String, OwnedValue>) -> Prompt {
    let text = |key: &str| get::<String>(options, key).unwrap_or_default();
    let check_label = text("check-label");
    let list: Vec<String> = get(options, "rows").unwrap_or_default();
    let selected = get::<i32>(options, "selected").and_then(|i| usize::try_from(i).ok()).unwrap_or(0);
    Prompt {
        title: text("title"),
        message: text("message"),
        description: text("description"),
        choice: (!check_label.is_empty()).then(|| (check_label, get(options, "check").unwrap_or(false))),
        selected: selected.min(list.len().saturating_sub(1)),
        list,
        continue_label: text("accept"),
        cancel_label: text("cancel"),
        ..Prompt::default()
    }
}

fn owned(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("plain values convert")
}

/// App id → state, from windows: 1 running, 2 focused.
pub fn app_states<'a>(apps: impl IntoIterator<Item = &'a str>, focused: Option<&str>) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for app in apps.into_iter().filter(|a| !a.is_empty()) {
        out.insert(app.to_owned(), 1);
    }
    if let Some(app) = focused.filter(|a| !a.is_empty()) {
        out.insert(app.to_owned(), 2);
    }
    out
}

pub struct PortalDialog {
    ui: mpsc::UnboundedSender<UiEvent>,
    apps: Arc<Mutex<HashMap<String, u32>>>,
}

impl PortalDialog {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self { ui, apps: Default::default() }
    }
}

#[interface(name = "org.helix.PortalDialog")]
impl PortalDialog {
    #[allow(clippy::too_many_arguments)]
    async fn ask(&self, title: String, message: String, description: String, grant_label: String, deny_label: String, check_label: String, check: bool) -> (bool, bool) {
        let _turn = prompt::turn().await;
        let prompt = Prompt {
            title,
            message,
            description,
            choice: (!check_label.is_empty()).then_some((check_label, check)),
            continue_label: grant_label,
            cancel_label: deny_label,
            ..Prompt::default()
        };
        let answer = prompt::ask(&self.ui, prompt).await;
        prompt::done(&self.ui);
        match answer {
            Answer::Continue { choice, .. } => (true, choice),
            Answer::Cancel => (false, check),
        }
    }

    async fn choose_app(&self, title: String, message: String, items: Vec<(String, String)>, selected: String) -> String {
        if items.is_empty() {
            return String::new();
        }
        let _turn = prompt::turn().await;
        let prompt = Prompt {
            title,
            message,
            list: items.iter().map(|(_, name)| name.clone()).collect(),
            selected: initial_index(&items, &selected),
            continue_label: "Open".into(),
            cancel_label: "Cancel".into(),
            ..Prompt::default()
        };
        let answer = prompt::ask(&self.ui, prompt).await;
        prompt::done(&self.ui);
        match answer {
            Answer::Continue { selected, .. } => items.get(selected).map(|(id, _)| id.clone()).unwrap_or_default(),
            Answer::Cancel => String::new(),
        }
    }

    /// A dialog described by `options` (see the interface notes); response
    /// 0 accepted, 1 canceled.
    async fn dialog(&self, options: HashMap<String, OwnedValue>) -> (u32, HashMap<String, OwnedValue>) {
        let mut prompt = dialog_prompt(&options);
        if let Some(path) = get::<String>(&options, "picture").filter(|p| !p.is_empty()) {
            let decoded = tokio::task::spawn_blocking(move || crate::wallpaper::decode(std::path::Path::new(&path))).await;
            match decoded {
                Ok(Ok(image)) => prompt.picture = Some(Arc::new(crate::wallpaper::thumbnail(&image, 392, 220))),
                Ok(Err(err)) => tracing::info!(%err, "cannot show the dialog's picture"),
                Err(_) => {}
            }
        }
        let _turn = prompt::turn().await;
        let answer = prompt::ask(&self.ui, prompt).await;
        prompt::done(&self.ui);
        match answer {
            Answer::Continue { choice, selected, .. } => {
                let results = HashMap::from([("check".to_owned(), owned(Value::from(choice))), ("selected".to_owned(), owned(Value::from(selected as i32)))]);
                (0, results)
            }
            Answer::Cancel => (1, HashMap::new()),
        }
    }

    async fn running_apps(&self) -> HashMap<String, u32> {
        self.apps.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[zbus(signal)]
    async fn running_apps_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// The UI reports the apps with windows.
pub async fn set_running(conn: &Connection, apps: HashMap<String, u32>) {
    let Ok(iface) = conn.object_server().interface::<_, PortalDialog>(PATH).await else { return };
    let changed = {
        let me = iface.get().await;
        let mut current = me.apps.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *current != apps;
        *current = apps;
        changed
    };
    if changed {
        let _ = PortalDialog::running_apps_changed(iface.signal_emitter()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_choices() {
        let s = app_states(["org.gnome.Calculator", "firefox", ""], Some("firefox"));
        assert_eq!(s.get("firefox"), Some(&2));
        assert_eq!(s.get("org.gnome.Calculator"), Some(&1));
        assert_eq!(s.len(), 2);
        let items = vec![("a".to_owned(), "A".to_owned()), ("b".to_owned(), "B".to_owned())];
        assert_eq!(initial_index(&items, "b"), 1);
        assert_eq!(initial_index(&items, "zzz"), 0);
        let options = HashMap::from([
            ("title".to_owned(), owned(Value::from("Set Background"))),
            ("check-label".to_owned(), owned(Value::from("Also lock screen"))),
            ("check".to_owned(), owned(Value::from(true))),
            ("rows".to_owned(), owned(Value::from(vec!["a", "b"]))),
            ("selected".to_owned(), owned(Value::from(5i32))),
        ]);
        let p = dialog_prompt(&options);
        assert_eq!((p.title.as_str(), p.choice, p.selected, p.list.len()), ("Set Background", Some(("Also lock screen".to_owned(), true)), 1, 2));
    }
}
