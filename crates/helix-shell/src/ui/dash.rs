//! The dash in the overview (helix-desktop-spec `contracts/wayland/README.md`,
//! "Favorites and app folders").

use helix_draw::Image;

use super::Ui;

/// Icon size and spacing (logical pixels).
pub(super) const DASH_ICON: i32 = 48;
pub(super) const DASH_GAP: i32 = 10;
/// The dash's height, padding included.
pub(super) const DASH_H: i32 = DASH_ICON + 24;

/// One dash icon.
pub(super) struct DashEntry {
    /// The app id (desktop file id without `.desktop`).
    pub app: String,
    pub icon: Option<Image>,
    /// Its windows, most recently used first.
    pub windows: Vec<u32>,
    /// After the gap (not a favorite).
    pub running_only: bool,
}

/// An app id of a desktop file id.
fn app_of(desktop_id: &str) -> &str {
    desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id)
}

/// Favorites, then other apps with windows: (app id, running only).
pub(super) fn dash_apps(favorites: &[String], installed: &[String], window_apps: &[String]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = favorites.iter().filter(|f| installed.contains(f)).map(|f| (app_of(f).to_owned(), false)).collect();
    for app in window_apps {
        if !app.is_empty() && !out.iter().any(|(a, _)| a.eq_ignore_ascii_case(app)) {
            out.push((app.clone(), true));
        }
    }
    out
}

impl Ui {
    /// The dash's entries, with their icons.
    pub(super) fn dash_entries(&mut self, scale: f64) -> Vec<DashEntry> {
        use gio::prelude::*;
        // As the launcher counts them: apps shown in menus.
        let installed: Vec<String> = gio::AppInfo::all().into_iter().filter(|a| a.should_show()).filter_map(|a| a.id().map(|i| i.to_string())).collect();
        let window_apps: Vec<String> = self.windows.mru().iter().map(|e| e.app_id.clone()).collect();
        let px = (f64::from(DASH_ICON) * scale).round() as u32;
        dash_apps(&crate::favorites::favorites(), &installed, &window_apps)
            .into_iter()
            .map(|(app, running_only)| {
                let windows = self.windows.mru().iter().filter(|e| e.app_id.eq_ignore_ascii_case(&app)).map(|e| e.id).collect();
                let key = format!("app:{app}@{px}");
                let icon = self
                    .icon_cache
                    .entry(key)
                    .or_insert_with(|| crate::icons::IconSource::for_app(&app).and_then(|s| crate::icons::load(&s, px)))
                    .clone();
                DashEntry { app, icon, windows, running_only }
            })
            .collect()
    }

    /// A dash icon was clicked: its newest window, else launch it.
    pub(super) fn dash_activate(&mut self, app: &str, window: Option<u32>) {
        self.close_overview(true);
        match window {
            Some(id) => self.activate_window(id),
            None => {
                let id = format!("{app}.desktop");
                tracing::info!(app = id, "launching from the dash");
                if let Err(err) = crate::launcher::launch(&id) {
                    tracing::warn!(%err, app = id, "launch failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorites_then_running() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let apps = dash_apps(
            &s(&["org.gnome.Calculator.desktop", "gone.desktop", "org.gnome.Nautilus.desktop"]),
            &s(&["org.gnome.Calculator.desktop", "org.gnome.Nautilus.desktop", "firefox.desktop"]),
            &s(&["firefox", "org.gnome.calculator", "firefox", ""]),
        );
        assert_eq!(apps, [("org.gnome.Calculator".to_owned(), false), ("org.gnome.Nautilus".to_owned(), false), ("firefox".to_owned(), true)]);
    }
}
