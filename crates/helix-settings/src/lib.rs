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
//! - [`Schema`] reads and writes keys of a schema whose installed copy may
//!   be older than this build (Helix's own schemas after a partial upgrade):
//!   a missing key gives the built-in default instead of aborting.
//!
//! Key contracts: `helix-desktop-spec/contracts/gsettings/`.

pub mod convert;
pub mod sound;
pub mod thumbnails;

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

/// Do GIO's one-time setup (loading its modules: the settings backend,
/// gvfs, volume monitors) on the calling thread. Call it from `main`
/// before starting any thread that uses GIO: GIO's first-use setup from
/// several threads at once can deadlock (a settings thread and the
/// volume monitor taking GLib's module and type locks in opposite
/// orders), which left the shell hanging at startup.
pub fn prepare_gio() {
    let _ = gio::SettingsBackend::default();
    let _ = gio::Vfs::default();
    let _ = gio::SettingsSchemaSource::default();
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

/// One schema, read and written key by key without the risk of GLib
/// aborting the process.
///
/// GSettings aborts when asked for a key its schema does not have. The
/// installed copy of a Helix schema can be older than the program (a
/// partial upgrade, or a build run against an older package), so every
/// read of a Helix-owned key goes through here: a key that is missing, or
/// has a different type than expected, reads as `None` (callers use their
/// built-in default) and is logged once; writes to it are skipped.
pub struct Schema {
    id: String,
    settings: Option<gio::Settings>,
}

impl Schema {
    /// Open `schema_id`; without the schema every read is `None`.
    pub fn open(schema_id: &str) -> Self {
        let settings = has_schema(schema_id).then(|| gio::Settings::new(schema_id));
        Self { id: schema_id.to_owned(), settings }
    }

    /// Whether the schema is installed.
    pub fn installed(&self) -> bool {
        self.settings.is_some()
    }

    /// The underlying settings, when the schema is installed.
    pub fn settings(&self) -> Option<&gio::Settings> {
        self.settings.as_ref()
    }

    /// Whether the installed schema has `key`.
    pub fn has(&self, key: &str) -> bool {
        self.settings
            .as_ref()
            .and_then(|s| s.settings_schema())
            .is_some_and(|schema| schema.has_key(key))
    }

    /// The settings, if the installed schema has `key` with type `ty`;
    /// otherwise logs (once per schema and key) and gives `None`.
    fn checked(&self, key: &str, ty: &gio::glib::VariantTy) -> Option<&gio::Settings> {
        let settings = self.settings.as_ref()?;
        let found = settings
            .settings_schema()
            .filter(|schema| schema.has_key(key))
            .map(|schema| schema.key(key).value_type());
        if found.as_deref() == Some(ty) {
            return Some(settings);
        }
        warn_once(&self.id, key, found.as_ref().map(|t| t.as_str()), ty.as_str());
        None
    }

    /// `key`'s value, or `None` when the installed schema lacks it (or
    /// gives it another type).
    pub fn get<T: gio::glib::variant::FromVariant + gio::glib::variant::StaticVariantType>(&self, key: &str) -> Option<T> {
        self.checked(key, &T::static_variant_type())?.value(key).get::<T>()
    }

    /// `key`'s value, or `default`.
    pub fn get_or<T: gio::glib::variant::FromVariant + gio::glib::variant::StaticVariantType>(&self, key: &str, default: T) -> T {
        self.get(key).unwrap_or(default)
    }

    /// A string-list key.
    pub fn strv(&self, key: &str) -> Option<Vec<String>> {
        self.get::<Vec<String>>(key)
    }

    /// Set `key`; `false` when the schema lacks it or refuses the value.
    pub fn set<T: gio::glib::variant::ToVariant + gio::glib::variant::StaticVariantType>(&self, key: &str, value: T) -> bool {
        match self.checked(key, &T::static_variant_type()) {
            Some(settings) => settings.set_value(key, &value.to_variant()).is_ok(),
            None => false,
        }
    }
}

/// Warn once per process about a missing or retyped key.
fn warn_once(schema: &str, key: &str, found: Option<&str>, wanted: &str) {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<std::collections::HashSet<(String, String)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let first = seen
        .lock()
        .map(|mut s| s.insert((schema.to_owned(), key.to_owned())))
        .unwrap_or(false);
    if first {
        match found {
            None => tracing::warn!(schema, key, "the installed schema has no such key (older than this build?); using the default"),
            Some(found) => tracing::warn!(schema, key, found, wanted, "the installed schema gives this key another type; using the default"),
        }
    }
}

/// A `gio::Settings` look-alike for schemas Helix does not own (GNOME's,
/// Ubuntu's): the same reads and writes, but a schema that is not
/// installed, or an installed schema older than the keys Helix knows
/// (a key missing, or of another type), never aborts the process as
/// GLib does. Such a read gives the type's empty value (`false`, `0`,
/// `""`, an empty list), logged once; such a write is skipped (an error).
/// Callers that need GNOME's default for a key newer than the installed
/// schema use [`Schema::get_or`] instead.
pub struct Settings {
    schema: Schema,
}

impl Settings {
    /// Open `schema_id` (installed or not).
    pub fn new(schema_id: &str) -> Self {
        Self { schema: Schema::open(schema_id) }
    }

    /// The underlying settings, when the schema is installed (for change
    /// signals and bindings).
    pub fn inner(&self) -> Option<&gio::Settings> {
        self.schema.settings()
    }

    /// The checked schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Whether the installed schema has `key`.
    pub fn has_key(&self, key: &str) -> bool {
        self.schema.has(key)
    }

    /// The installed schema's description, when installed.
    pub fn settings_schema(&self) -> Option<gio::SettingsSchema> {
        self.inner().and_then(|s| s.settings_schema())
    }

    pub fn boolean(&self, key: &str) -> bool {
        self.schema.get_or(key, false)
    }

    pub fn string(&self, key: &str) -> String {
        self.schema.get_or(key, String::new())
    }

    pub fn int(&self, key: &str) -> i32 {
        self.schema.get_or(key, 0)
    }

    pub fn uint(&self, key: &str) -> u32 {
        self.schema.get_or(key, 0)
    }

    pub fn int64(&self, key: &str) -> i64 {
        self.schema.get_or(key, 0)
    }

    pub fn uint64(&self, key: &str) -> u64 {
        self.schema.get_or(key, 0)
    }

    pub fn double(&self, key: &str) -> f64 {
        self.schema.get_or(key, 0.0)
    }

    pub fn strv(&self, key: &str) -> Vec<String> {
        self.schema.get_or(key, Vec::new())
    }

    /// An enum key's value (its nick's number), or 0.
    pub fn enum_(&self, key: &str) -> i32 {
        match self.inner() {
            Some(s) if self.schema.has(key) => s.enum_(key),
            _ => 0,
        }
    }

    /// `key`'s value, or the unit value when the schema lacks it (so a
    /// typed `get` reads `None`).
    pub fn value(&self, key: &str) -> gio::glib::Variant {
        match self.inner() {
            Some(s) if self.schema.has(key) => s.value(key),
            _ => {
                warn_once(&self.schema.id, key, None, "*");
                ().to_variant()
            }
        }
    }

    fn write(&self, key: &str, ok: bool) -> Result<(), gio::glib::BoolError> {
        if ok {
            Ok(())
        } else {
            Err(gio::glib::bool_error!("cannot write {} {key}", self.schema.id))
        }
    }

    pub fn set_boolean(&self, key: &str, value: bool) -> Result<(), gio::glib::BoolError> {
        self.write(key, self.schema.set(key, value))
    }

    pub fn set_string(&self, key: &str, value: &str) -> Result<(), gio::glib::BoolError> {
        self.write(key, self.schema.set(key, value.to_owned()))
    }

    pub fn set_int(&self, key: &str, value: i32) -> Result<(), gio::glib::BoolError> {
        self.write(key, self.schema.set(key, value))
    }

    pub fn set_uint(&self, key: &str, value: u32) -> Result<(), gio::glib::BoolError> {
        self.write(key, self.schema.set(key, value))
    }

    pub fn set_double(&self, key: &str, value: f64) -> Result<(), gio::glib::BoolError> {
        self.write(key, self.schema.set(key, value))
    }

    pub fn set_strv(&self, key: &str, value: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), gio::glib::BoolError> {
        let list: Vec<String> = value.into_iter().map(|s| s.as_ref().to_owned()).collect();
        self.write(key, self.schema.set(key, list))
    }

    /// Set `key` to `value` when the installed schema has the key with
    /// `value`'s type.
    pub fn set_value(&self, key: &str, value: &gio::glib::Variant) -> Result<(), gio::glib::BoolError> {
        let ok = match self.inner() {
            Some(s) if self.schema.has(key) && s.settings_schema().is_some_and(|sc| sc.key(key).value_type() == *value.type_()) => s.set_value(key, value).is_ok(),
            _ => false,
        };
        self.write(key, ok)
    }

    /// Reset `key` to its default (when the schema has it).
    pub fn reset(&self, key: &str) {
        if let Some(s) = self.inner().filter(|_| self.schema.has(key)) {
            s.reset(key);
        }
    }
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
