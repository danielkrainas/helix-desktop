//! The extension host and the SDK over a socket pair (helix-desktop-spec
//! `contracts/extensions/README.md`): Hello, permissions, settings, items
//! reaching the panel, and clicks reaching the extension.

use std::time::Duration;

use helix_extension::{DockItemState, Event, Extension, MenuEntry, PanelItemState, SliderState, ToggleState};
use helix_shell::ext_host::store::{Found, Kind};
use helix_shell::ext_host::{manifest, Action, ExtHost};
use helix_shell::UiEvent;
use zbus::zvariant::Value;

const MANIFEST: &str = r#"
id = "org.example.Test"
name = "Test"
version = "1"
api = "1.1"
exec = ["x"]
permissions = ["launch-apps"]
[[contributes.panel-item]]
id = "count"
name = "Count"
label = "?"
[[contributes.quick-toggle]]
id = "toggle"
title = "Toggle"
[[contributes.quick-slider]]
id = "level"
title = "Level"
[[contributes.dock-item]]
id = "shelf-item"
name = "Shelf"
accepts-drops = true
[[contributes.command]]
id = "count"
title = "Count Up"
[settings]
step = { type = "int", default = 2, min = 1, max = 9, label = "Step" }
"#;

async fn next<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>, mut pick: impl FnMut(UiEvent) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(t) = rx.recv().await.and_then(&mut pick) {
                return t;
            }
        }
    })
    .await
    .expect("in time")
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_sdk() {
    // Settings go to a scratch config directory.
    let tmp = std::env::temp_dir().join(format!("helix-ext-p2p-{}", std::process::id()));
    // SAFETY: this test binary runs this one test.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("GSETTINGS_BACKEND", "memory");
    }
    let (ui, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = ExtHost::new(ui);
    let found = Found { id: "org.example.Test".into(), dir: tmp.clone(), kind: Kind::Development, manifest: manifest::parse(MANIFEST, None) };
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let attach = host.attach(found, ours);
    theirs.set_nonblocking(true).unwrap();
    let client = Extension::connect_with(tokio::net::UnixStream::from_std(theirs).unwrap(), "org.example.Test".into());
    let (attached, ext) = tokio::join!(attach, client);
    attached.unwrap();
    let mut ext = ext.unwrap();

    // Before Hello, nothing works.
    assert!(ext.setting("step").await.is_err());
    let menu = ext.menu("m", vec![MenuEntry::item(1, "Reset")]).await.unwrap();
    let item = ext.panel_item("count", PanelItemState { label: "0".into(), menu: Some("m".into()), ..Default::default() }).await.unwrap();
    let toggle = ext.toggle("toggle", ToggleState { title: "Toggle".into(), ..Default::default() }).await.unwrap();
    let _slider = ext.slider("level", SliderState::default()).await.unwrap();
    let dock = ext.dock_item("shelf-item", DockItemState { count: 3, ..Default::default() }).await.unwrap();
    let granted = ext.start().await.unwrap();
    assert_eq!(granted.permissions, ["launch-apps"]);
    let _ = menu;

    // Permissions and settings.
    assert!(ext.show_notice("x", "y", "").await.unwrap_err().to_string().contains("AccessDenied"));
    assert_eq!(i64::try_from(ext.setting("step").await.unwrap()), Ok(2));
    assert!(ext.set_setting("step", Value::from(12i64)).await.is_err(), "out of range");
    assert!(ext.set_setting("nope", Value::from(1i64)).await.is_err());
    ext.set_setting("step", Value::from(3i64)).await.unwrap();
    // Its own change comes back as an event too.
    assert!(matches!(ext.next_event().await, Some(Event::SettingChanged { key, .. }) if key == "step"));
    assert!(std::fs::read_to_string(tmp.join("config/helix/extensions/org.example.Test.toml")).unwrap().contains("step = 3"));

    // Its items reach the panel, and follow changes.
    let snap = next(&mut rx, |e| match e {
        UiEvent::Extensions(s) if s.panel.first().is_some_and(|p| p.running && p.label == "0") => Some(s),
        _ => None,
    })
    .await;
    assert_eq!(snap.panel[0].full_id, "org.example.Test/count");
    assert!(snap.panel[0].has_menu && snap.toggles[0].running);
    item.update(|s| {
        s.label = "5".into();
        s.graph = vec![0.5, 2.0];
    })
    .await
    .unwrap();
    let snap = next(&mut rx, |e| match e {
        UiEvent::Extensions(s) if s.panel.first().is_some_and(|p| p.label == "5") => Some(s),
        _ => None,
    })
    .await;
    assert_eq!(snap.panel[0].graph, [0.5, 1.0], "clamped");

    // Clicks reach the extension.
    host.act("org.example.Test/count", Action::Primary { x: 10, y: 32 }).await;
    assert_eq!(ext.next_event().await, Some(Event::Activate { item: "count".into(), x: 10, y: 32 }));
    host.act("org.example.Test/toggle", Action::Toggle).await;
    assert_eq!(ext.next_event().await, Some(Event::SetActive { toggle: "toggle".into(), active: true }));
    toggle.update(|s| s.active = true).await.unwrap();
    // An item whose click opens its menu.
    item.update(|s| s.activatable = false).await.unwrap();
    host.act("org.example.Test/count", Action::Primary { x: 10, y: 32 }).await;
    let entries = next(&mut rx, |e| match e {
        UiEvent::ExtMenu { entries, x: 10, .. } => Some(entries),
        _ => None,
    })
    .await;
    assert_eq!(entries[0].label, "Reset");
    host.act("org.example.Test/count", Action::MenuChosen { menu: "/org/helix/Extension/menu/m".into(), id: 1 }).await;
    assert_eq!(ext.next_event().await, Some(Event::MenuActivated { menu: "m".into(), id: 1 }));
    // Setting changes reach it too.
    host.set_setting("org.example.Test", "step", manifest::SettingValue::Int(4)).unwrap();
    match ext.next_event().await {
        Some(Event::SettingChanged { key, value }) => assert_eq!((key.as_str(), i64::try_from(value)), ("step", Ok(4))),
        other => panic!("{other:?}"),
    }
    // 1.1: dock items (ids with "-" in their paths), drops and commands.
    let snap = next(&mut rx, |e| match e {
        UiEvent::Extensions(s) if s.dock.first().is_some_and(|d| d.count == 3) => Some(s),
        _ => None,
    })
    .await;
    assert_eq!((snap.dock[0].full_id.as_str(), snap.dock[0].accepts_drops), ("org.example.Test/shelf-item", true));
    assert_eq!(snap.commands[0].title, "Count Up");
    host.act("org.example.Test/shelf-item", Action::DockDrop(vec!["file:///a".into()])).await;
    assert_eq!(ext.next_event().await, Some(Event::Drop { item: "shelf-item".into(), uris: vec!["file:///a".into()] }));
    host.run_command("org.example.Test/count").await.unwrap();
    assert_eq!(ext.next_event().await, Some(Event::Command { id: "count".into() }));
    assert!(host.run_command("org.example.Test/nope").await.is_err());
    dock.update(|s| s.urgent = true).await.unwrap();

    // Both sides match their contracts.
    async fn introspect(conn: &zbus::Connection, path: &str) -> String {
        let reply = conn.call_method(None::<&str>, path, Some("org.freedesktop.DBus.Introspectable"), "Introspect", &()).await.unwrap();
        reply.body().deserialize().unwrap()
    }
    use helix_test_support::conformance::{assert_conformant, check, load_dbus_contract};
    let host_side = introspect(ext.connection(), "/org/helix/Extension1/Host").await;
    let iface = "org.helix.Extension1.Host";
    assert_conformant(iface, &check(iface, &load_dbus_contract("../extensions/org.helix.Extension1.Host.xml"), &host_side, &[]));
    let shell = host.connection_of("org.example.Test").unwrap();
    for (path, iface, file) in [
        ("/org/helix/Extension/panel_item/count", "org.helix.Extension1.PanelItem", "PanelItem"),
        ("/org/helix/Extension/toggle/toggle", "org.helix.Extension1.Toggle", "Toggle"),
        ("/org/helix/Extension/slider/level", "org.helix.Extension1.Slider", "Slider"),
        ("/org/helix/Extension/menu/m", "org.helix.Extension1.Menu", "Menu"),
        ("/org/helix/Extension/dock_item/shelf_item", "org.helix.Extension1.DockItem", "DockItem"),
        ("/org/helix/Extension/commands", "org.helix.Extension1.Commands", "Commands"),
    ] {
        let xml = introspect(&shell, path).await;
        assert_conformant(iface, &check(iface, &load_dbus_contract(&format!("../extensions/org.helix.Extension1.{file}.xml")), &xml, &[]));
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
