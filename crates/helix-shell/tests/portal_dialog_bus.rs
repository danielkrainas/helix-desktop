//! `org.helixdesktop.PortalDialog` on a test bus (helixdesktop/spec
//! `contracts/dbus/org.helixdesktop.PortalDialog.xml`): the exported interface
//! matches the contract, and `ChooseFiles` hands the UI the portal's
//! options and the UI's answer back as results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use helix_shell::file_chooser::{Chosen, Mode};
use helix_shell::portal_dialog::{PortalDialog, PATH};
use helix_shell::UiEvent;
use helix_test_support::conformance::{assert_conformant, check, load_dbus_contract};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedValue, Value};

const IFACE: &str = "org.helixdesktop.PortalDialog";

#[tokio::test(flavor = "multi_thread")]
async fn choose_files_round_trip() {
    let Some(bus) = TestBus::start() else { return };
    // The places are read from a scratch home, never the user's.
    let home = std::env::temp_dir().join(format!("helix-portal-dialog-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".config")).unwrap();
    for (k, v) in [("HOME", home.clone()), ("XDG_CONFIG_HOME", home.join(".config")), ("XDG_DATA_HOME", home.join(".local/share")), ("XDG_RUNTIME_DIR", home.clone()), ("USER", PathBuf::from("helix-test-nobody"))] {
        std::env::set_var(k, v);
    }
    let (ui, mut events) = tokio::sync::mpsc::unbounded_channel();
    let _shell = bus.builder().serve_at(PATH, PortalDialog::new(ui)).unwrap().name("org.gnome.Shell").unwrap().build().await.unwrap();
    let client = bus.connect().await;
    let xml: String = client.call_method(Some("org.gnome.Shell"), PATH, Some("org.freedesktop.DBus.Introspectable"), "Introspect", &()).await.unwrap().body().deserialize().unwrap();
    assert_conformant(IFACE, &check(IFACE, &load_dbus_contract("org.helixdesktop.PortalDialog.xml"), &xml, &[]));

    let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
    let options: HashMap<String, OwnedValue> = HashMap::from([
        ("title".to_owned(), v(Value::from("Export Image"))),
        ("mode".to_owned(), v(Value::from("save"))),
        ("folder".to_owned(), v(Value::from(b"/srv/pictures".to_vec()))),
        ("name".to_owned(), v(Value::from("photo.png"))),
        ("filters".to_owned(), v(Value::from(vec![("PNG".to_owned(), vec![(1u32, "image/png".to_owned())])]))),
        ("filter".to_owned(), v(Value::from(0i32))),
        ("check-label".to_owned(), v(Value::from("Keep metadata"))),
        ("check".to_owned(), v(Value::from(true))),
    ]);
    let call = {
        let client = client.clone();
        tokio::spawn(async move { client.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "ChooseFiles", &(options,)).await.unwrap().body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap() })
    };
    let request = loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("the UI is asked").expect("events") {
            UiEvent::ChooseFiles(slot) => break slot.take().expect("a request"),
            _ => continue,
        }
    };
    let o = &request.options;
    assert_eq!((o.title.as_str(), o.accept.as_str(), o.mode, o.folder.clone(), o.name.as_str(), o.filter, o.check.clone()), ("Export Image", "Save", Mode::Save, Some(PathBuf::from("/srv/pictures")), "photo.png", Some(0), Some(("Keep metadata".to_owned(), true))));
    assert!(o.places.iter().any(|p| p.kind == helix_settings::files::PlaceKind::Home), "places are read for the dialog: {:?}", o.places);
    request.reply.send(Some(Chosen { paths: vec![PathBuf::from("/srv/pictures/photo 2.png")], folder: PathBuf::from("/srv/pictures"), filter: Some(0), check: false })).unwrap();
    let (response, results) = call.await.unwrap();
    assert_eq!(response, 0);
    let paths: Vec<Vec<u8>> = results["paths"].clone().try_into().unwrap();
    assert_eq!(paths, [b"/srv/pictures/photo 2.png".to_vec()]);
    let folder: Vec<u8> = results["folder"].clone().try_into().unwrap();
    assert_eq!(folder, b"/srv/pictures");
    assert_eq!((i32::try_from(results["filter"].clone()).unwrap(), bool::try_from(results["check"].clone()).unwrap()), (0, false));

    // Canceled: response 1, no results.
    let options: HashMap<String, OwnedValue> = HashMap::from([("mode".to_owned(), v(Value::from("folder")))]);
    let call = {
        let client = client.clone();
        tokio::spawn(async move { client.call_method(Some("org.gnome.Shell"), PATH, Some(IFACE), "ChooseFiles", &(options,)).await.unwrap().body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap() })
    };
    let request = loop {
        if let UiEvent::ChooseFiles(slot) = events.recv().await.unwrap() {
            break slot.take().unwrap();
        }
    };
    assert_eq!((request.options.mode, request.options.accept.as_str(), request.options.folder.clone()), (Mode::Folder, "Select", None));
    drop(request);
    assert_eq!(call.await.unwrap(), (1, HashMap::new()));
    let _ = std::fs::remove_dir_all(&home);
}
