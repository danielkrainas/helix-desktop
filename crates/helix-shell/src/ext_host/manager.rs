//! `org.helix.Extensions1` on the session bus (helix-desktop-spec
//! `contracts/extensions/README.md`, "Session bus"): what tools see and
//! change.

use std::collections::HashMap;

use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use super::manifest::{SettingDecl, SettingType, SettingValue};
use super::runner::ExtHost;

pub const OBJECT_PATH: &str = "/org/helix/Extensions1";

pub struct Manager {
    pub host: ExtHost,
}

fn value<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values convert")
}

fn failed(e: String) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e)
}

/// A `GetSettings` entry: key, type, value, details.
pub type SettingEntry = (String, String, OwnedValue, HashMap<String, OwnedValue>);

/// A `GetSettings` entry.
fn setting_entry(decl: &SettingDecl, current: &SettingValue) -> SettingEntry {
    let ty = match &decl.ty {
        SettingType::Bool => "bool",
        SettingType::Int => "int",
        SettingType::Double => "double",
        SettingType::String => "string",
        SettingType::Enum(_) => "enum",
        SettingType::StringList => "string-list",
    };
    let mut details = HashMap::from([
        ("label".to_owned(), value(decl.label.clone())),
        ("help".to_owned(), value(decl.help.clone())),
        ("default".to_owned(), decl.default.to_value()),
    ]);
    if matches!(decl.ty, SettingType::Int | SettingType::Double) {
        for (key, limit) in [("min", decl.min), ("max", decl.max)] {
            if let Some(limit) = limit {
                details.insert(key.to_owned(), value(limit));
            }
        }
    }
    if let SettingType::Enum(choices) = &decl.ty {
        details.insert("choices".to_owned(), value(choices.clone()));
    }
    (decl.key.clone(), ty.to_owned(), current.to_value(), details)
}

#[zbus::interface(name = "org.helix.Extensions1")]
impl Manager {
    fn list_extensions(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
        self.host
            .list()
            .into_iter()
            .map(|i| {
                let entry = HashMap::from([
                    ("name".to_owned(), value(i.name)),
                    ("description".to_owned(), value(i.description)),
                    ("version".to_owned(), value(i.version)),
                    ("path".to_owned(), value(i.path.to_string_lossy().into_owned())),
                    ("type".to_owned(), value(i.kind.name())),
                    ("enabled".to_owned(), value(i.enabled)),
                    ("approved".to_owned(), value(i.approved)),
                    ("state".to_owned(), value(i.state.name())),
                    ("error".to_owned(), value(i.error)),
                    ("permissions".to_owned(), value(i.permissions)),
                    ("commands".to_owned(), value(i.commands)),
                    ("sandboxed".to_owned(), value(i.sandboxed)),
                ]);
                (i.id, entry)
            })
            .collect()
    }

    /// With `approve`, the user is asked first (the approval prompt).
    async fn enable(&self, id: String, approve: bool) -> zbus::fdo::Result<()> {
        if approve {
            if let Some((answer, asked)) = self.host.ask_approval(&id).map_err(failed)? {
                let allowed = answer.await.unwrap_or(false);
                if !allowed {
                    return Err(zbus::fdo::Error::AccessDenied(format!("{id} was not approved")));
                }
                // What was approved must be what it asks for now.
                if self.host.permissions(&id) != asked {
                    return Err(zbus::fdo::Error::AccessDenied(format!("{id} changed what it asks for; try again")));
                }
            }
        }
        self.host.enable(&id, approve).map_err(failed)
    }

    fn disable(&self, id: String) -> zbus::fdo::Result<()> {
        self.host.disable(&id).map_err(failed)
    }

    fn restart(&self, id: String) -> zbus::fdo::Result<()> {
        self.host.restart(&id).map_err(failed)
    }

    /// Its settings: key, type, value, details.
    fn get_settings(&self, id: String) -> zbus::fdo::Result<Vec<SettingEntry>> {
        let settings = self.host.settings(&id).ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no extension {id}")))?;
        Ok(settings.into_iter().map(|(decl, current)| setting_entry(&decl, &current)).collect())
    }

    fn get_log(&self, id: String) -> zbus::fdo::Result<Vec<String>> {
        self.host.log_lines(&id).ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no extension {id}")))
    }

    fn set_setting(&self, id: String, key: String, value: OwnedValue) -> zbus::fdo::Result<()> {
        let decl = self.host.list().into_iter().find(|i| i.id == id).and_then(|i| i.settings.into_iter().find(|(d, _)| d.key == key)).map(|(d, _)| d);
        let decl = decl.ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no setting {key} in {id}")))?;
        let parsed = decl.from_dbus(&value).map_err(zbus::fdo::Error::InvalidArgs)?;
        self.host.set_setting(&id, &key, parsed).map_err(zbus::fdo::Error::InvalidArgs)
    }

    fn run_development(&self, path: String, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<String> {
        let owner = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() {
            return Err(zbus::fdo::Error::InvalidArgs("the path must be absolute".into()));
        }
        self.host.run_development(&path, owner).map_err(zbus::fdo::Error::InvalidArgs)
    }

    fn stop_development(&self, id: String) -> zbus::fdo::Result<()> {
        self.host.stop_development(&id).map_err(zbus::fdo::Error::InvalidArgs)
    }

    async fn run_command(&self, command: String) -> zbus::fdo::Result<()> {
        self.host.run_command(&command).await.map_err(failed)
    }

    fn bind_command(&self, command: String, accelerator: String) -> zbus::fdo::Result<()> {
        self.host.bind_command(&command, &accelerator).map_err(zbus::fdo::Error::InvalidArgs)
    }

    #[zbus(signal)]
    pub async fn changed(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
}
