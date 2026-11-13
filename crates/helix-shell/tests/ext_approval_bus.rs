//! `org.helix.Extensions1` for other programs (helix-desktop-spec
//! `contracts/extensions/README.md`, "Session bus" and "Approval prompt"):
//! `Enable` with approval asks the user, and `GetSettings` describes an
//! extension's settings.

use std::collections::HashMap;
use std::time::Duration;

use helix_shell::ext_host::approval::ApprovalQuestion;
use helix_shell::ext_host::manager::{Manager, OBJECT_PATH};
use helix_shell::ext_host::ExtHost;
use helix_shell::UiEvent;
use helix_test_support::TestBus;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::zvariant::OwnedValue;

const MANIFEST: &str = r#"
id = "org.example.Ask"
name = "Ask"
version = "1"
api = "1.2"
exec = ["bin/nothing"]
activation = ["onVisible"]
permissions = ["inhibit-idle", "network"]
[settings]
mode = { type = "enum", choices = ["fast", "slow"], default = "slow", label = "Mode", help = "How fast" }
step = { type = "int", default = 2, min = 1, max = 9, label = "Step" }
names = { type = "string-list", default = ["a"], label = "Names" }
"#;

const IFACE: &str = "org.helix.Extensions1";

/// The next approval question, or `None` within `wait`.
async fn question(rx: &mut UnboundedReceiver<UiEvent>, wait: Duration) -> Option<ApprovalQuestion> {
    tokio::time::timeout(wait, async {
        loop {
            if let Some(UiEvent::ApproveExtension(slot)) = rx.recv().await {
                if let Some(q) = slot.take() {
                    return q;
                }
            }
        }
    })
    .await
    .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_and_settings() {
    let Some(bus) = TestBus::start() else { return };
    let tmp = std::env::temp_dir().join(format!("helix-ext-approval-{}", std::process::id()));
    let schemas = tmp.join("schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/org.helix.extensions.gschema.xml");
    std::fs::copy(data, schemas.join("org.helix.extensions.gschema.xml")).unwrap();
    let Ok(status) = std::process::Command::new("glib-compile-schemas").arg(&schemas).status() else {
        eprintln!("glib-compile-schemas is not installed; skipping");
        return;
    };
    assert!(status.success());
    let ext_dir = tmp.join("data/helix/extensions/org.example.Ask");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("extension.toml"), MANIFEST).unwrap();
    // SAFETY: this test binary runs this one test, before any GLib use.
    unsafe {
        std::env::set_var("GSETTINGS_SCHEMA_DIR", &schemas);
        std::env::set_var("GSETTINGS_BACKEND", "memory");
        std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        std::env::set_var("XDG_DATA_DIRS", tmp.join("none"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
    }
    let (ui, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = ExtHost::new(ui);
    host.rescan();
    let _server = bus.builder().serve_at(OBJECT_PATH, Manager { host: host.clone() }).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let conn = bus.connect().await;
    let enable = |approve: bool| {
        let conn = conn.clone();
        tokio::spawn(async move { conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), "Enable", &("org.example.Ask", approve)).await })
    };
    let state = || host.list().into_iter().find(|i| i.id == "org.example.Ask").map(|i| (i.enabled, i.approved)).unwrap();

    // Refused: nothing changes, and the caller hears AccessDenied.
    let call = enable(true);
    let q = question(&mut rx, Duration::from_secs(5)).await.expect("the user is asked");
    assert_eq!(q.name, "Ask");
    assert_eq!(q.can, ["keep the screen on", "use the network"]);
    assert!(!q.sandboxed);
    q.reply.send(false).unwrap();
    let err = call.await.unwrap().unwrap_err();
    assert!(matches!(&err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"), "{err:?}");
    assert_eq!(state(), (false, false));

    // The prompt went away unanswered (another question replaced it).
    let call = enable(true);
    drop(question(&mut rx, Duration::from_secs(5)).await.expect("asked"));
    assert!(call.await.unwrap().is_err());

    // Allowed: enabled and approved.
    let call = enable(true);
    question(&mut rx, Duration::from_secs(5)).await.expect("asked").reply.send(true).unwrap();
    call.await.unwrap().unwrap();
    assert_eq!(state(), (true, true));

    // Nothing left to approve: no question.
    let call = enable(true);
    assert!(question(&mut rx, Duration::from_millis(500)).await.is_none());
    call.await.unwrap().unwrap();

    // Settings, in the manifest's order.
    let reply = conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), "GetSettings", &("org.example.Ask",)).await.unwrap();
    let settings: Vec<(String, String, OwnedValue, HashMap<String, OwnedValue>)> = reply.body().deserialize().unwrap();
    let keys: Vec<(&str, &str)> = settings.iter().map(|(k, t, _, _)| (k.as_str(), t.as_str())).collect();
    assert_eq!(keys, [("mode", "enum"), ("step", "int"), ("names", "string-list")]);
    let (_, _, mode, details) = &settings[0];
    assert_eq!(String::try_from(mode.clone()).unwrap(), "slow");
    assert_eq!(String::try_from(details["label"].clone()).unwrap(), "Mode");
    assert_eq!(String::try_from(details["help"].clone()).unwrap(), "How fast");
    assert_eq!(Vec::<String>::try_from(details["choices"].clone()).unwrap(), ["fast", "slow"]);
    assert_eq!(String::try_from(details["default"].clone()).unwrap(), "slow");
    let (_, _, step, details) = &settings[1];
    assert_eq!(i64::try_from(step.clone()).unwrap(), 2);
    assert_eq!((f64::try_from(details["min"].clone()).unwrap(), f64::try_from(details["max"].clone()).unwrap()), (1.0, 9.0));
    assert!(!details.contains_key("choices"));
    assert_eq!(Vec::<String>::try_from(settings[2].2.clone()).unwrap(), ["a"]);
    let unknown = conn.call_method(Some("org.gnome.Shell"), OBJECT_PATH, Some(IFACE), "GetSettings", &("org.example.None",)).await;
    assert!(matches!(unknown, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"));

    let _ = std::fs::remove_dir_all(&tmp);
}
