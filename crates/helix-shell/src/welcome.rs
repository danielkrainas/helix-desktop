//! The welcome dialog (helix-desktop-spec `contracts/gsettings/org.gnome.shell.md`,
//! "Welcome dialog"): once per shell version, when GNOME Tour is
//! installed, offer it.

use tokio::sync::mpsc;

use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

const SHELL: &str = "org.gnome.shell";
const KEY: &str = "welcome-dialog-last-shown-version";
const TOUR: &str = "org.gnome.Tour.desktop";

/// A version's numeric parts (`""` is the oldest; parts that are not
/// numbers count as 0).
fn parts(v: &str) -> Vec<u64> {
    if v.trim().is_empty() {
        return Vec::new();
    }
    v.trim().split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
}

/// Whether the dialog was last shown for a version older than `current`.
pub fn due(last_shown: &str, current: &str) -> bool {
    parts(last_shown) < parts(current)
}

fn tour() -> Option<gio::AppInfo> {
    use gio::prelude::*;
    gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|i| i == TOUR))
}

/// Offers the tour if it is due, a little after the session starts.
pub async fn run(ui: mpsc::UnboundedSender<UiEvent>) {
    use gio::prelude::*;
    if !helix_settings::has_schema(SHELL) {
        return;
    }
    let has_key = gio::Settings::new(SHELL).settings_schema().is_some_and(|s| s.has_key(KEY));
    if !has_key || !due(&gio::Settings::new(SHELL).string(KEY), crate::SHELL_VERSION) || tour().is_none() {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let _turn = prompt::turn().await;
    let p = Prompt {
        title: "Welcome".into(),
        message: "Would you like a short tour of the desktop?".into(),
        continue_label: "Take Tour".into(),
        cancel_label: "No Thanks".into(),
        ..Default::default()
    };
    let answer = prompt::ask(&ui, p).await;
    prompt::done(&ui);
    let _ = gio::Settings::new(SHELL).set_string(KEY, crate::SHELL_VERSION);
    if let Answer::Continue { .. } = answer {
        if let Some(app) = tour() {
            tracing::info!("starting the tour");
            if let Err(err) = app.launch(&[], None::<&gio::AppLaunchContext>) {
                tracing::warn!(%err, "cannot start the tour");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(due("", "51.0"));
        assert!(due("50.2", "51.0"));
        assert!(!due("51.0", "51.0"));
        assert!(!due("999999", "51.0"), "a huge number turns it off");
        assert!(due("51", "51.0"));
    }
}
