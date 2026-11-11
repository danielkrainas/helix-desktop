//! Helix GSettings surface.
//!
//! Helix reuses the system's dconf and the installed GNOME schemas rather
//! than shipping its own settings backend. This crate is a thin layer over
//! GSettings for other Helix components:
//!
//! - [`GSettings::snapshot`] reads every key of a set of schemas as D-Bus
//!   values (for the Settings portal and similar services).
//! - [`GSettings::watch`] reports which schema changed.
//! - [`key_type`] exposes a key's GVariant type, for contract checks.
//!
//! Key contracts: `helix-desktop-spec/contracts/gsettings/`.

pub mod convert;
pub mod sound;

use std::collections::BTreeMap;
use std::thread::JoinHandle;

use gio::prelude::*;
use tokio::sync::mpsc;
use zbus::zvariant::OwnedValue;

/// Schemas exposed to applications through the Settings portal.
pub const PORTAL_SCHEMAS: &[&str] = &[
    "org.gnome.desktop.interface",
    "org.gnome.desktop.a11y.interface",
    "org.gnome.desktop.wm.preferences",
    "org.gnome.desktop.sound",
    "org.gnome.desktop.privacy",
    "org.gnome.desktop.peripherals.mouse",
];

/// Keys and values of one schema.
pub type Values = BTreeMap<String, OwnedValue>;

fn schema_source() -> Option<gio::SettingsSchemaSource> {
    gio::SettingsSchemaSource::default()
}

/// Whether `schema_id` is installed.
pub fn has_schema(schema_id: &str) -> bool {
    schema_source()
        .and_then(|s| s.lookup(schema_id, true))
        .is_some()
}

/// The GVariant type string of `schema_id`'s `key`, if both exist.
pub fn key_type(schema_id: &str, key: &str) -> Option<String> {
    let schema = schema_source()?.lookup(schema_id, true)?;
    schema
        .has_key(key)
        .then(|| schema.key(key).value_type().as_str().to_string())
}

/// A set of installed schemas.
#[derive(Debug, Clone)]
pub struct GSettings {
    schemas: Vec<String>,
}

impl GSettings {
    /// Use the given schemas; ones that are not installed are skipped with a
    /// warning (constructing GSettings for a missing schema would abort).
    pub fn new(schema_ids: &[&str]) -> Self {
        let schemas = schema_ids
            .iter()
            .filter(|id| {
                let ok = has_schema(id);
                if !ok {
                    tracing::warn!(schema = id, "GSettings schema not installed; skipping");
                }
                ok
            })
            .map(|id| id.to_string())
            .collect();
        Self { schemas }
    }

    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    /// Current value of every key, by schema. Keys whose type cannot be sent
    /// over D-Bus (maybe types) are left out.
    pub fn snapshot(&self) -> BTreeMap<String, Values> {
        self.schemas
            .iter()
            .map(|id| (id.clone(), read_schema(id)))
            .collect()
    }

    /// Start watching for changes. The receiver yields the id of each schema
    /// that changed. Watching stops when the returned [`Watch`] is dropped.
    pub fn watch(&self) -> (mpsc::UnboundedReceiver<String>, Watch) {
        let (tx, rx) = mpsc::unbounded_channel();
        let schemas = self.schemas.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("helix-gsettings".into())
            .spawn(move || {
                let context = gio::glib::MainContext::new();
                let main_loop = gio::glib::MainLoop::new(Some(&context), false);
                let _ = context.with_thread_default(|| {
                    // Keep the Settings objects alive for the loop's lifetime.
                    let _settings: Vec<gio::Settings> = schemas
                        .iter()
                        .map(|id| {
                            let settings = gio::Settings::new(id);
                            let tx = tx.clone();
                            let id = id.clone();
                            settings.connect_changed(None, move |_, key| {
                                tracing::debug!(schema = %id, key, "GSettings changed");
                                let _ = tx.send(id.clone());
                            });
                            // Reading once makes GSettings start reporting changes.
                            for key in settings.settings_schema().map(|s| s.list_keys()).unwrap_or_default() {
                                let _ = settings.value(&key);
                            }
                            settings
                        })
                        .collect();
                    let _ = ready_tx.send(main_loop.clone());
                    main_loop.run();
                });
            })
            .expect("spawn GSettings watcher thread");
        let main_loop = ready_rx.recv().expect("GSettings watcher started");
        (
            rx,
            Watch {
                main_loop,
                thread: Some(thread),
            },
        )
    }
}

fn read_schema(schema_id: &str) -> Values {
    let settings = gio::Settings::new(schema_id);
    let Some(schema) = settings.settings_schema() else {
        return Values::new();
    };
    let mut values = Values::new();
    for key in schema.list_keys() {
        let variant = settings.value(&key);
        match convert::to_dbus(&variant) {
            Some(v) => {
                values.insert(key.to_string(), v);
            }
            None => tracing::debug!(schema = schema_id, %key, "skipping key not representable on D-Bus"),
        }
    }
    values
}

/// Stops the GSettings watcher thread when dropped.
pub struct Watch {
    main_loop: gio::glib::MainLoop,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.main_loop.quit();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
