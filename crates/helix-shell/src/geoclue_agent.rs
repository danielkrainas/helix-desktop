//! GeoClue's location agent (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.GeoClue2.Agent.md`): asks the user
//! before an app gets its location, and remembers the answer.

use std::collections::HashMap;

use futures_util::StreamExt;
use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;
use zbus::{interface, Connection};

use crate::location::LOCATION;
use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const AGENT_PATH: &str = "/org/freedesktop/GeoClue2/Agent";
/// The id GeoClue's configuration lists for the GNOME shell.
pub const AGENT_ID: &str = "gnome-shell";
const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER: &str = "org.freedesktop.GeoClue2.Manager";
const STORE: &str = "org.freedesktop.impl.portal.PermissionStore";
const STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";
const TABLE: &str = "location";

const LEVELS: [(&str, u32); 6] = [("NONE", 0), ("COUNTRY", 1), ("CITY", 4), ("NEIGHBORHOOD", 5), ("STREET", 6), ("EXACT", 8)];

/// The level `max-accuracy-level` names.
pub fn level_of_setting(name: &str) -> u32 {
    LEVELS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map_or(8, |(_, l)| *l)
}

/// A stored decision's level (0: refused).
pub fn stored_level(permissions: &[String]) -> Option<u32> {
    let first = permissions.first()?;
    LEVELS.iter().find(|(n, _)| n == first).map(|(_, l)| *l)
}

/// The name to store for `level` (the highest named level not above it).
pub fn level_name(level: u32) -> &'static str {
    LEVELS.iter().rev().find(|(_, l)| *l <= level).map_or("NONE", |(n, _)| n)
}

/// What to answer GeoClue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    Allow(u32),
    Ask,
}

pub fn decide(max: u32, known_app: bool, stored: Option<u32>, requested: u32) -> Decision {
    if max == 0 || !known_app {
        return Decision::Deny;
    }
    match stored {
        Some(0) => Decision::Deny,
        Some(level) => Decision::Allow(requested.min(level).min(max)),
        None => Decision::Ask,
    }
}

/// `MaxAccuracyLevel` now.
pub fn max_level() -> u32 {
    if !helix_settings::has_schema(LOCATION) {
        return 0;
    }
    let s = gio::Settings::new(LOCATION);
    if s.boolean("enabled") {
        level_of_setting(&s.string("max-accuracy-level"))
    } else {
        0
    }
}

pub struct Agent {
    ui: mpsc::UnboundedSender<UiEvent>,
    session: Connection,
}

impl Agent {
    async fn stored(&self, app: &str) -> Option<u32> {
        let reply = self.session.call_method(Some(STORE), STORE_PATH, Some(STORE), "Lookup", &(TABLE, TABLE)).await.ok()?;
        let (apps, _data): (HashMap<String, Vec<String>>, OwnedValue) = reply.body().deserialize().ok()?;
        stored_level(apps.get(app)?)
    }

    async fn store(&self, app: &str, level: u32) {
        let value = vec![level_name(level).to_owned()];
        if let Err(err) = self.session.call_method(Some(STORE), STORE_PATH, Some(STORE), "SetPermission", &(TABLE, true, TABLE, app, value)).await {
            tracing::warn!(%err, app, "cannot store the location decision");
        }
    }
}

#[interface(name = "org.freedesktop.GeoClue2.Agent")]
impl Agent {
    async fn authorize_app(&self, desktop_id: String, req_accuracy_level: u32) -> (bool, u32) {
        let max = max_level();
        let file = format!("{desktop_id}.desktop");
        let name = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|id| id == file)).map(|a| a.display_name().to_string());
        let stored = if max > 0 && name.is_some() { self.stored(&desktop_id).await } else { None };
        let decision = match decide(max, name.is_some(), stored, req_accuracy_level) {
            Decision::Ask => {
                let _turn = prompt::turn().await;
                let answer = prompt::ask(
                    &self.ui,
                    Prompt {
                        title: "Location Access".into(),
                        message: format!("{} wants to use your location.", name.unwrap_or_default()),
                        description: "Location access can be changed at any time from the privacy settings.".into(),
                        continue_label: "Allow".into(),
                        cancel_label: "Deny".into(),
                        ..Prompt::default()
                    },
                )
                .await;
                prompt::done(&self.ui);
                let level = match answer {
                    Answer::Continue { .. } => req_accuracy_level.min(max),
                    Answer::Cancel => 0,
                };
                self.store(&desktop_id, level).await;
                if level == 0 { Decision::Deny } else { Decision::Allow(level) }
            }
            d => d,
        };
        tracing::info!(desktop_id, req_accuracy_level, ?decision, "location access");
        match decision {
            Decision::Allow(level) => (true, level),
            _ => (false, 0),
        }
    }

    #[zbus(property)]
    fn max_accuracy_level(&self) -> u32 {
        max_level()
    }
}

async fn add_agent(system: &Connection) {
    match system.call_method(Some(GEOCLUE), MANAGER_PATH, Some(MANAGER), "AddAgent", &(AGENT_ID,)).await {
        Ok(_) => tracing::info!("GeoClue agent registered"),
        Err(err) => tracing::info!(%err, "GeoClue did not take the location agent"),
    }
}

/// Serve the agent on `system` for the rest of the session.
pub async fn run(system: Connection, session: Connection, ui: mpsc::UnboundedSender<UiEvent>) -> zbus::Result<()> {
    system.object_server().at(AGENT_PATH, Agent { ui, session }).await?;
    let dbus = zbus::fdo::DBusProxy::new(&system).await?;
    let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, GEOCLUE)]).await?;
    add_agent(&system).await;
    let watched = helix_settings::GSettings::new(&[LOCATION]);
    let (mut changes, _watch) = watched.watch();
    let mut max = max_level();
    loop {
        tokio::select! {
            Some(change) = owners.next() => {
                if change.args().is_ok_and(|a| a.new_owner().is_some()) {
                    add_agent(&system).await;
                }
            }
            Some(_) = changes.recv() => {
                let now = max_level();
                if now != max {
                    max = now;
                    if let Ok(iface) = system.object_server().interface::<_, Agent>(AGENT_PATH).await {
                        let _ = iface.get().await.max_accuracy_level_changed(iface.signal_emitter()).await;
                    }
                }
            }
            else => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_of_setting("city"), 4);
        assert_eq!(level_of_setting("exact"), 8);
        assert_eq!(level_name(7), "STREET");
        assert_eq!(level_name(0), "NONE");
        assert_eq!(stored_level(&["CITY".into(), "x".into()]), Some(4));
        assert_eq!(stored_level(&["NONE".into()]), Some(0));
        assert_eq!(stored_level(&["bogus".into()]), None);
    }

    #[test]
    fn decisions() {
        assert_eq!(decide(0, true, Some(8), 8), Decision::Deny, "location services off");
        assert_eq!(decide(8, false, None, 8), Decision::Deny, "unknown app");
        assert_eq!(decide(8, true, None, 8), Decision::Ask);
        assert_eq!(decide(8, true, Some(0), 8), Decision::Deny);
        assert_eq!(decide(4, true, Some(8), 6), Decision::Allow(4), "capped by the maximum");
        assert_eq!(decide(8, true, Some(5), 8), Decision::Allow(5), "capped by the decision");
    }
}
