//! The GlobalShortcuts portal backend against a fake shell (its dialog
//! says yes; it grabs accelerators and fires them) on a test bus.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use helix_portals::settings::Snapshot;
use helix_portals::{PortalService, BUS_NAME, OBJECT_PATH};
use helix_session::SessionService;
use helix_test_support::TestBus;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Calls = Arc<Mutex<Vec<String>>>;

struct Dialogs {
    calls: Calls,
}

#[zbus::interface(name = "org.helix.PortalDialog")]
impl Dialogs {
    async fn dialog(&self, options: HashMap<String, OwnedValue>) -> (u32, HashMap<String, OwnedValue>) {
        let rows: Vec<String> = options.get("rows").and_then(|v| Vec::<String>::try_from(v.clone()).ok()).unwrap_or_default();
        self.calls.lock().unwrap().push(format!("Dialog {}", rows.join(" | ")));
        (0, HashMap::new())
    }
}

struct Shell {
    calls: Calls,
    next: Mutex<u32>,
}

#[zbus::interface(name = "org.gnome.Shell")]
impl Shell {
    async fn grab_accelerators(&self, accelerators: Vec<(String, u32, u32)>) -> Vec<u32> {
        let mut out = Vec::new();
        for (accel, modes, flags) in accelerators {
            self.calls.lock().unwrap().push(format!("Grab {accel} {modes} {flags}"));
            // "<Super>x" is taken.
            if accel == "<Super>x" {
                out.push(0);
            } else {
                let mut next = self.next.lock().unwrap();
                *next += 1;
                out.push(*next);
            }
        }
        out
    }
    async fn ungrab_accelerators(&self, actions: Vec<u32>) -> bool {
        self.calls.lock().unwrap().push(format!("Ungrab {actions:?}"));
        true
    }
    #[zbus(signal)]
    async fn accelerator_activated(emitter: &SignalEmitter<'_>, action: u32, parameters: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn accelerator_deactivated(emitter: &SignalEmitter<'_>, action: u32, parameters: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

fn o(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).unwrap()
}

fn shortcut(id: &str, description: &str, trigger: Option<&str>) -> (String, HashMap<String, OwnedValue>) {
    let mut props = HashMap::from([("description".to_owned(), o(Value::from(description)))]);
    if let Some(t) = trigger {
        props.insert("preferred_trigger".to_owned(), o(Value::from(t)));
    }
    (id.to_owned(), props)
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcuts_flow() {
    let Some(bus) = TestBus::start() else { return };
    let data = std::env::temp_dir().join(format!("helix-gs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::env::set_var("XDG_DATA_HOME", &data);
    let calls: Calls = Arc::default();
    let shell = bus
        .builder()
        .serve_at("/org/helix/PortalDialog", Dialogs { calls: calls.clone() })
        .unwrap()
        .serve_at("/org/gnome/Shell", Shell { calls: calls.clone(), next: Mutex::new(0) })
        .unwrap()
        .name("org.gnome.Shell")
        .unwrap()
        .build()
        .await
        .unwrap();
    let session = SessionService::start(bus.builder()).await.unwrap();
    let _session = tokio::spawn(session.run(std::future::pending()));
    let settings: Arc<Mutex<Snapshot>> = Arc::new(Mutex::new(BTreeMap::new()));
    let _portals = PortalService::start(bus.builder(), settings).await.unwrap();
    let app = bus.connect().await;
    const IFACE: &str = "org.freedesktop.impl.portal.GlobalShortcuts";
    let session_path = "/org/freedesktop/portal/desktop/session/1_1/gs";
    let req = |n: u32| OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/request/1_1/r{n}")).unwrap();
    let sp = || OwnedObjectPath::try_from(session_path).unwrap();
    let none = HashMap::<String, OwnedValue>::new;

    let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "CreateSession", &(req(1), sp(), "com.example.Talk", none())).await.unwrap();
    assert_eq!(reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap().0, 0);

    let shortcuts = vec![shortcut("talk", "Push to talk", Some("CTRL+SHIFT+t")), shortcut("mute", "Mute", None), shortcut("x", "Taken", Some("LOGO+x"))];
    let bind = |n: u32, list: Vec<(String, HashMap<String, OwnedValue>)>| {
        let app = app.clone();
        async move {
            let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "BindShortcuts", &(req(n), sp(), list, "", none())).await.unwrap();
            reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap()
        }
    };
    let (code, results) = bind(2, shortcuts.clone()).await;
    assert_eq!(code, 0);
    let listed = <Vec<(String, HashMap<String, OwnedValue>)>>::try_from(results["shortcuts"].clone()).unwrap();
    let trig: Vec<String> = listed.iter().map(|(_, p)| String::try_from(p["trigger_description"].clone()).unwrap()).collect();
    assert_eq!(trig, ["Ctrl+Shift+T", "Not assigned", "Not assigned"], "unbound and taken ones are not assigned");

    // A press and release come out as the portal's.
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).unwrap().build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &app, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let emitter = SignalEmitter::new(&shell, "/org/gnome/Shell").unwrap();
    Shell::accelerator_activated(&emitter, 1, HashMap::from([("timestamp", Value::from(1234u32))])).await.unwrap();
    Shell::accelerator_deactivated(&emitter, 1, HashMap::from([("timestamp", Value::from(1300u32))])).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let m = tokio::time::timeout(Duration::from_secs(5), signals.next()).await.unwrap().unwrap().unwrap();
        let (path, id, time, _): (OwnedObjectPath, String, u64, HashMap<String, OwnedValue>) = m.body().deserialize().unwrap();
        seen.push(format!("{} {} {id} {time}", m.header().member().unwrap(), path.as_str()));
    }
    assert_eq!(seen, [format!("Activated {session_path} talk 1234"), format!("Deactivated {session_path} talk 1300")]);

    // The same set again: not asked; the old grab released first.
    let (code, _) = bind(3, shortcuts.clone()).await;
    assert_eq!(code, 0);
    let reply = app.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "ListShortcuts", &(req(4), sp())).await.unwrap();
    let (code, results) = reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>().unwrap();
    assert_eq!((code, <Vec<(String, HashMap<String, OwnedValue>)>>::try_from(results["shortcuts"].clone()).unwrap().len()), (0, 3));
    app.call_method(Some(BUS_NAME), session_path, Some("org.freedesktop.impl.portal.Session"), "Close", &()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "Dialog Push to talk — Ctrl+Shift+T | Mute — not assigned | Taken — Super+X".to_owned(),
            "Grab <Control><Shift>t 3 128".into(),
            "Grab <Super>x 3 128".into(),
            "Ungrab [1]".into(),
            "Grab <Control><Shift>t 3 128".into(),
            "Grab <Super>x 3 128".into(),
            "Ungrab [2]".into(),
        ]
    );
    let _ = std::fs::remove_dir_all(&data);
}
