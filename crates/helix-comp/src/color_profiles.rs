//! Monitors' color profiles (helix-desktop-spec
//! `contracts/dbus/org.gnome.Mutter.DisplayConfig.md`, "Color profiles"):
//! each monitor is a display device in colord (`org.freedesktop.ColorManager`
//! on the system bus), and the calibration curves (`vcgt`) of its default
//! profile are applied to its gamma ramps.
//!
//! Not yet tried with a calibrated monitor: the tests use a fake colord
//! and made-up profiles.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

use crate::display_config::{ColorRequest, ColorSet, Monitor, COLOR_MODE_DEFAULT};

pub const COLORD: &str = "org.freedesktop.ColorManager";
pub const COLORD_PATH: &str = "/org/freedesktop/ColorManager";
const DEVICE: &str = "org.freedesktop.ColorManager.Device";
const PROFILE: &str = "org.freedesktop.ColorManager.Profile";

/// The colord device id of a monitor.
pub fn device_id(m: &Monitor) -> String {
    format!("xrandr-{}-{}-{}", m.vendor, m.product, m.serial)
}

/// The properties a monitor's device is created with.
pub fn device_properties(m: &Monitor) -> HashMap<String, String> {
    let mut props = HashMap::from([
        ("Kind".to_owned(), "display".to_owned()),
        ("Mode".to_owned(), "physical".to_owned()),
        ("Colorspace".to_owned(), "rgb".to_owned()),
        ("Vendor".to_owned(), m.vendor.clone()),
        ("Model".to_owned(), m.product.clone()),
        ("Serial".to_owned(), m.serial.clone()),
        ("XRANDR_name".to_owned(), m.connector.clone()),
    ]);
    if m.builtin {
        props.insert("Embedded".to_owned(), "true".to_owned());
    }
    props
}

struct Known {
    path: OwnedObjectPath,
    connector: String,
    /// The profile file whose curves are applied ("" for none).
    applied: Option<String>,
    color_mode: u32,
}

async fn property(conn: &Connection, path: &str, iface: &str, name: &str) -> Option<OwnedValue> {
    let reply = conn.call_method(Some(COLORD), path, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, name)).await.ok()?;
    reply.body().deserialize::<OwnedValue>().ok()
}

/// The default profile's file of a device, if any.
async fn profile_file(conn: &Connection, device: &str) -> Option<String> {
    let profiles: Vec<OwnedObjectPath> = property(conn, device, DEVICE, "Profiles").await?.try_into().ok()?;
    let first = profiles.first()?;
    let file: String = property(conn, first.as_str(), PROFILE, "Filename").await?.try_into().ok()?;
    (!file.is_empty()).then_some(file)
}

async fn send(gamma: &mpsc::UnboundedSender<ColorRequest>, connector: &str, set: Option<ColorSet>) -> Option<crate::display_config::Ramps> {
    let (reply, answer) = oneshot::channel();
    gamma.send(ColorRequest { connector: connector.to_owned(), set, reply }).ok()?;
    answer.await.ok()?.ok()
}

/// Apply the curves of `known`'s profile (none in the BT.2100 color mode).
async fn apply(conn: &Connection, gamma: &mpsc::UnboundedSender<ColorRequest>, known: &mut Known) {
    let file = if known.color_mode == COLOR_MODE_DEFAULT { profile_file(conn, known.path.as_str()).await } else { None };
    if known.applied.as_ref() == Some(&file.clone().unwrap_or_default()) {
        return;
    }
    let curves = match &file {
        Some(file) => {
            let size = send(gamma, &known.connector, None).await.map(|r| r.0.len()).unwrap_or(256);
            let bytes = std::fs::read(file);
            let curves = bytes.ok().and_then(|b| crate::colorspace::icc_vcgt(&b, size));
            tracing::info!(connector = known.connector, file, calibrated = curves.is_some(), "color profile");
            curves
        }
        None => None,
    };
    send(gamma, &known.connector, Some(ColorSet::Calibration(curves))).await;
    known.applied = Some(file.unwrap_or_default());
}

/// Keep colord's devices and the calibration in step with the monitors
/// (each list from `monitors`) and with colord, for the rest of the
/// session.
pub async fn run(conn: Connection, gamma: mpsc::UnboundedSender<ColorRequest>, mut monitors: mpsc::UnboundedReceiver<Vec<Monitor>>) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(COLORD)?.member("DeviceChanged")?.build();
    let mut changes = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
    let mut known: HashMap<String, Known> = HashMap::new();
    loop {
        tokio::select! {
            list = monitors.recv() => {
                let Some(list) = list else { break };
                let shown: Vec<&Monitor> = list.iter().filter(|m| m.current_mode().is_some()).collect();
                // Monitors gone (or off): their devices go.
                let ids: Vec<String> = shown.iter().map(|m| device_id(m)).collect();
                for (id, gone) in known.iter().filter(|(id, _)| !ids.contains(id)) {
                    tracing::debug!(id, "colord device removed");
                    let _ = conn.call_method(Some(COLORD), COLORD_PATH, Some(COLORD), "DeleteDevice", &(&gone.path,)).await;
                }
                known.retain(|id, _| ids.contains(id));
                for m in shown {
                    let id = device_id(m);
                    if !known.contains_key(&id) {
                        let reply = conn
                            .call_method(Some(COLORD), COLORD_PATH, Some(COLORD), "CreateDevice", &(&id, "temp", device_properties(m)))
                            .await;
                        let path: OwnedObjectPath = match reply.and_then(|r| r.body().deserialize()) {
                            Ok(path) => path,
                            Err(err) => {
                                tracing::info!(%err, id, "colord did not take the monitor");
                                continue;
                            }
                        };
                        tracing::info!(id, path = path.as_str(), "colord device");
                        known.insert(id.clone(), Known { path, connector: m.connector.clone(), applied: None, color_mode: m.color_mode });
                    }
                    if let Some(k) = known.get_mut(&id) {
                        k.connector = m.connector.clone();
                        k.color_mode = m.color_mode;
                        apply(&conn, &gamma, k).await;
                    }
                }
            }
            change = changes.next() => {
                let Some(Ok(message)) = change else { break };
                let Ok(path) = message.body().deserialize::<OwnedObjectPath>() else { continue };
                if let Some(k) = known.values_mut().find(|k| k.path == path) {
                    apply(&conn, &gamma, k).await;
                }
            }
        }
    }
    Ok(())
}
