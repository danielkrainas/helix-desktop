//! The dwell click type window's data (helix-desktop-spec
//! `contracts/gsettings/a11y-mouse.md`, "Click type window"): when the
//! settings want it, and the compositor's current click type
//! (`org.helix.ClickType`).

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::Connection;

use crate::UiEvent;

pub const SCHEMA: &str = "org.gnome.desktop.a11y.mouse";
const BUS: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/helix/ClickType";
const INTERFACE: &str = "org.helix.ClickType";

/// Whether the window shows: dwell click on, in the `window` mode, with
/// `click-type-window-visible`.
pub fn window_wanted(dwell: bool, mode: &str, visible: bool) -> bool {
    dwell && mode == "window" && visible
}

fn read() -> bool {
    use gio::prelude::*;
    if !helix_settings::has_schema(SCHEMA) {
        return false;
    }
    let s = gio::Settings::new(SCHEMA);
    window_wanted(s.boolean("dwell-click-enabled"), &s.string("dwell-mode"), s.boolean("click-type-window-visible"))
}

/// Ask the compositor for `click_type` (`single`, `double`, `drag`,
/// `secondary`) next.
pub async fn set(conn: &Connection, click_type: &str) {
    if let Err(err) = conn.call_method(Some(BUS), PATH, Some(INTERFACE), "SetClickType", &(click_type,)).await {
        tracing::info!(%err, "the compositor did not take the click type");
    }
}

/// Follow the settings and the compositor's click type for the rest of
/// the session.
pub async fn follow(conn: Connection, ui: mpsc::UnboundedSender<UiEvent>) {
    let watched = helix_settings::GSettings::new(&[SCHEMA]);
    let (mut changes, _watch) = watched.watch();
    let current = async {
        let proxy = zbus::Proxy::new(&conn, BUS, PATH, INTERFACE).await.ok()?;
        let first: String = proxy.get_property("ClickType").await.unwrap_or_else(|_| "single".into());
        Some((proxy, first))
    };
    let (proxy, first) = match current.await {
        Some((proxy, first)) => (Some(proxy), first),
        None => (None, "single".into()),
    };
    let _ = ui.send(UiEvent::ClickType(first));
    let mut types = match &proxy {
        Some(p) => Some(p.receive_property_changed::<String>("ClickType").await),
        None => None,
    };
    let _ = ui.send(UiEvent::ClickTypeWindow(read()));
    loop {
        tokio::select! {
            change = changes.recv() => {
                if change.is_none() {
                    break;
                }
                let _ = ui.send(UiEvent::ClickTypeWindow(read()));
            }
            Some(change) = async { match types.as_mut() { Some(t) => t.next().await, None => std::future::pending().await } } => {
                if let Ok(value) = change.get().await {
                    let _ = ui.send(UiEvent::ClickType(value));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wanted() {
        assert!(super::window_wanted(true, "window", true));
        assert!(!super::window_wanted(true, "gesture", true));
        assert!(!super::window_wanted(false, "window", true));
        assert!(!super::window_wanted(true, "window", false));
    }
}
