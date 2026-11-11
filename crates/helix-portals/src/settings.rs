//! `org.freedesktop.impl.portal.Settings` backend.
//!
//! Contract: `helix-desktop-spec/contracts/dbus/org.freedesktop.impl.portal.Settings.xml`
//! and `org.freedesktop.impl.portal.md`.
//!
//! Values come from a [`SettingsSource`] (in production, GSettings via
//! helix-settings). The documented `org.freedesktop.appearance` namespace is
//! derived from those raw values here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, DBusError};

/// Keys and values of one namespace.
pub type Namespace = BTreeMap<String, OwnedValue>;
/// Namespaces by name.
pub type Snapshot = BTreeMap<String, Namespace>;

/// The documented appearance namespace.
pub const APPEARANCE: &str = "org.freedesktop.appearance";

const INTERFACE: &str = "org.gnome.desktop.interface";
const A11Y_INTERFACE: &str = "org.gnome.desktop.a11y.interface";

/// Provides the raw (non-derived) settings to expose.
pub trait SettingsSource: Send + Sync + 'static {
    /// Current values of every exposed namespace.
    fn snapshot(&self) -> Snapshot;
}

/// A fixed snapshot; useful for tests and as a fallback.
impl SettingsSource for Mutex<Snapshot> {
    fn snapshot(&self) -> Snapshot {
        clone_snapshot(&self.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The system's GSettings (dconf + installed schemas).
impl SettingsSource for helix_settings::GSettings {
    fn snapshot(&self) -> Snapshot {
        helix_settings::GSettings::snapshot(self)
    }
}

/// Errors with the names the portal frontend expects.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
pub enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
    Failed(String),
}

pub fn clone_value(v: &OwnedValue) -> OwnedValue {
    // Settings values never carry file descriptors, so cloning can't fail.
    v.try_clone().expect("settings values contain no file descriptors")
}

pub fn clone_snapshot(s: &Snapshot) -> Snapshot {
    s.iter()
        .map(|(ns, keys)| {
            let keys = keys.iter().map(|(k, v)| (k.clone(), clone_value(v))).collect();
            (ns.clone(), keys)
        })
        .collect()
}

fn get<'a>(raw: &'a Snapshot, ns: &str, key: &str) -> Option<&'a Value<'static>> {
    raw.get(ns).and_then(|n| n.get(key)).map(|v| &**v)
}

/// Derive `org.freedesktop.appearance` from GSettings values:
///
/// - `color-scheme` from `org.gnome.desktop.interface color-scheme`
///   (`default` → 0, `prefer-dark` → 1, `prefer-light` → 2),
/// - `contrast` from `org.gnome.desktop.a11y.interface high-contrast`,
/// - `reduced-motion` from `org.gnome.desktop.a11y.interface reduced-motion`
///   (`reduce` → 1). Schemas older than GNOME 51 lack that key; there,
///   `org.gnome.desktop.interface enable-animations` = false counts as
///   reduced motion.
///
/// `accent-color` is omitted until its mapping is specified (see gaps.md).
pub fn appearance(raw: &Snapshot) -> Namespace {
    let color_scheme = match get(raw, INTERFACE, "color-scheme") {
        Some(Value::Str(s)) if s.as_str() == "prefer-dark" => 1u32,
        Some(Value::Str(s)) if s.as_str() == "prefer-light" => 2,
        _ => 0,
    };
    let contrast = match get(raw, A11Y_INTERFACE, "high-contrast") {
        Some(Value::Bool(true)) => 1u32,
        _ => 0,
    };
    let reduced_motion = match get(raw, A11Y_INTERFACE, "reduced-motion") {
        Some(Value::Str(s)) => u32::from(s.as_str() == "reduce"),
        _ => match get(raw, INTERFACE, "enable-animations") {
            Some(Value::Bool(false)) => 1,
            _ => 0,
        },
    };
    BTreeMap::from([
        ("color-scheme".to_owned(), OwnedValue::from(color_scheme)),
        ("contrast".to_owned(), OwnedValue::from(contrast)),
        ("reduced-motion".to_owned(), OwnedValue::from(reduced_motion)),
    ])
}

/// Raw namespaces plus the derived appearance namespace.
pub fn with_appearance(mut raw: Snapshot) -> Snapshot {
    let derived = appearance(&raw);
    raw.insert(APPEARANCE.to_owned(), derived);
    raw
}

/// Does `namespace` match a `ReadAll` filter?
pub fn matches(filter: &[String], namespace: &str) -> bool {
    if filter.is_empty() || filter.iter().any(String::is_empty) {
        return true;
    }
    filter.iter().any(|f| match f.strip_suffix('*') {
        Some(prefix) if prefix.ends_with('.') => namespace.starts_with(prefix),
        _ => f == namespace,
    })
}

/// Keys whose value is new or different in `new`.
pub fn changes(old: &Snapshot, new: &Snapshot) -> Vec<(String, String, OwnedValue)> {
    let mut out = Vec::new();
    for (ns, keys) in new {
        for (key, value) in keys {
            if old.get(ns).and_then(|o| o.get(key)) != Some(value) {
                out.push((ns.clone(), key.clone(), clone_value(value)));
            }
        }
    }
    out
}

/// The exported Settings backend.
pub struct SettingsBackend {
    source: Arc<dyn SettingsSource>,
    current: Mutex<Snapshot>,
}

impl SettingsBackend {
    pub fn new(source: Arc<dyn SettingsSource>) -> Self {
        let current = with_appearance(source.snapshot());
        Self {
            source,
            current: Mutex::new(current),
        }
    }

    fn current(&self) -> MutexGuard<'_, Snapshot> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-read the source and emit `SettingChanged` for every changed key.
    pub async fn refresh(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        let new = with_appearance(self.source.snapshot());
        let changed = {
            let mut current = self.current();
            let changed = changes(&current, &new);
            *current = new;
            changed
        };
        for (ns, key, value) in changed {
            tracing::debug!(namespace = %ns, %key, "setting changed");
            Self::setting_changed(emitter, &ns, &key, Value::from(value)).await?;
        }
        Ok(())
    }
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsBackend {
    async fn read_all(&self, namespaces: Vec<String>) -> Snapshot {
        self.current()
            .iter()
            .filter(|(ns, _)| matches(&namespaces, ns))
            .map(|(ns, keys)| {
                let keys = keys.iter().map(|(k, v)| (k.clone(), clone_value(v))).collect();
                (ns.clone(), keys)
            })
            .collect()
    }

    async fn read(&self, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
        self.current()
            .get(namespace)
            .and_then(|n| n.get(key))
            .map(clone_value)
            .ok_or_else(|| PortalError::NotFound(format!("requested setting {namespace} {key} not found")))
    }

    #[zbus(signal)]
    pub async fn setting_changed(
        emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Value<'_>,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(scheme: &str, animations: bool, high_contrast: bool) -> Snapshot {
        BTreeMap::from([
            (
                INTERFACE.to_owned(),
                BTreeMap::from([
                    ("color-scheme".to_owned(), OwnedValue::from(zbus::zvariant::Str::from(scheme.to_owned()))),
                    ("enable-animations".to_owned(), OwnedValue::from(animations)),
                ]),
            ),
            (
                A11Y_INTERFACE.to_owned(),
                BTreeMap::from([("high-contrast".to_owned(), OwnedValue::from(high_contrast))]),
            ),
        ])
    }

    fn u(ns: &Namespace, key: &str) -> u32 {
        u32::try_from(&*ns[key]).unwrap()
    }

    #[test]
    fn appearance_mapping() {
        let a = appearance(&raw("prefer-dark", false, true));
        assert_eq!((u(&a, "color-scheme"), u(&a, "reduced-motion"), u(&a, "contrast")), (1, 1, 1));
        let a = appearance(&raw("prefer-light", true, false));
        assert_eq!((u(&a, "color-scheme"), u(&a, "reduced-motion"), u(&a, "contrast")), (2, 0, 0));
        let a = appearance(&raw("default", true, false));
        assert_eq!(u(&a, "color-scheme"), 0);
        // Missing schemas mean "no preference".
        let a = appearance(&Snapshot::new());
        assert_eq!((u(&a, "color-scheme"), u(&a, "reduced-motion"), u(&a, "contrast")), (0, 0, 0));
        assert!(!a.contains_key("accent-color"));
    }

    #[test]
    fn reduced_motion_prefers_the_gnome_51_key() {
        let with_key = |value: &str, animations: bool| {
            let mut raw = raw("default", animations, false);
            raw.get_mut(A11Y_INTERFACE).unwrap().insert(
                "reduced-motion".to_owned(),
                OwnedValue::from(zbus::zvariant::Str::from(value.to_owned())),
            );
            u(&appearance(&raw), "reduced-motion")
        };
        assert_eq!(with_key("reduce", true), 1);
        // The explicit key wins over enable-animations when present.
        assert_eq!(with_key("no-preference", false), 0);
        // Older schemas: fall back to enable-animations.
        assert_eq!(u(&appearance(&raw("default", false, false)), "reduced-motion"), 1);
    }

    #[test]
    fn namespace_filters() {
        let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches(&[], "org.gnome.desktop.interface"));
        assert!(matches(&f(&[""]), "anything"));
        assert!(matches(&f(&["org.gnome.*"]), "org.gnome.desktop.interface"));
        assert!(!matches(&f(&["org.gnome.*"]), "org.freedesktop.appearance"));
        assert!(matches(&f(&["org.freedesktop.appearance"]), "org.freedesktop.appearance"));
        assert!(!matches(&f(&["org.freedesktop"]), "org.freedesktop.appearance"));
    }

    #[test]
    fn changes_include_derived_keys() {
        let old = with_appearance(raw("default", true, false));
        let new = with_appearance(raw("prefer-dark", true, false));
        let changed: Vec<_> = changes(&old, &new)
            .into_iter()
            .map(|(ns, k, _)| format!("{ns} {k}"))
            .collect();
        assert_eq!(
            changed,
            vec![
                "org.freedesktop.appearance color-scheme".to_owned(),
                "org.gnome.desktop.interface color-scheme".to_owned(),
            ]
        );
        assert!(changes(&new, &new).is_empty());
    }
}
