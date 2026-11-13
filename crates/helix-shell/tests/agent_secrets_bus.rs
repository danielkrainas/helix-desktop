//! Agent-owned network secrets kept in a fake Secret Service on a test bus
//! (helix-desktop-spec `org.freedesktop.NetworkManager.SecretAgent.md`,
//! "Agent-owned secrets").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_test_support::TestBus;
use zbus::object_server::ObjectServer;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Store = Arc<Mutex<Vec<(OwnedObjectPath, HashMap<String, String>, Vec<u8>)>>>;
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

struct Service(Store);

#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    fn open_session(&self, _algorithm: String, _input: OwnedValue) -> (OwnedValue, OwnedObjectPath) {
        (OwnedValue::try_from(Value::from("")).unwrap(), OwnedObjectPath::try_from("/org/freedesktop/secrets/session/1").unwrap())
    }
    fn search_items(&self, attrs: HashMap<String, String>) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
        let found = self.0.lock().unwrap().iter().filter(|(_, a, _)| attrs.iter().all(|(k, v)| a.get(k) == Some(v))).map(|(p, _, _)| p.clone()).collect();
        (found, Vec::new())
    }
    fn unlock(&self, items: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        (items, OwnedObjectPath::try_from("/").unwrap())
    }
    fn get_secrets(&self, items: Vec<OwnedObjectPath>, session: OwnedObjectPath) -> HashMap<OwnedObjectPath, Secret> {
        let store = self.0.lock().unwrap();
        items.into_iter().filter_map(|i| store.iter().find(|(p, _, _)| *p == i).map(|(p, _, v)| (p.clone(), (session.clone(), Vec::new(), v.clone(), "text/plain".into())))).collect()
    }
}

struct Collection(Store);

#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    async fn create_item(&self, props: HashMap<String, OwnedValue>, secret: Secret, replace: bool, #[zbus(object_server)] server: &ObjectServer) -> (OwnedObjectPath, OwnedObjectPath) {
        let attrs: HashMap<String, String> = props.get("org.freedesktop.Secret.Item.Attributes").and_then(|v| HashMap::try_from(v.clone()).ok()).unwrap_or_default();
        let path = {
            let mut store = self.0.lock().unwrap();
            if replace {
                store.retain(|(_, a, _)| *a != attrs);
            }
            let path = OwnedObjectPath::try_from(format!("/org/freedesktop/secrets/collection/test/{}", store.len() + 1 + rand_suffix())).unwrap();
            store.push((path.clone(), attrs.clone(), secret.2));
            path
        };
        server.at(path.as_str(), Item { store: self.0.clone(), path: path.clone(), attrs }).await.unwrap();
        (path, OwnedObjectPath::try_from("/").unwrap())
    }
}

fn rand_suffix() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(100, Ordering::SeqCst)
}

struct Item {
    store: Store,
    path: OwnedObjectPath,
    attrs: HashMap<String, String>,
}

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    #[zbus(property)]
    fn attributes(&self) -> HashMap<String, String> {
        self.attrs.clone()
    }
    fn delete(&self) -> OwnedObjectPath {
        self.store.lock().unwrap().retain(|(p, _, _)| *p != self.path);
        OwnedObjectPath::try_from("/").unwrap()
    }
}

fn v<'a>(x: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(x.into()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_owned_secrets_are_kept() {
    let Some(bus) = TestBus::start() else { return };
    let store: Store = Default::default();
    let _secrets = bus
        .builder()
        .serve_at("/org/freedesktop/secrets", Service(store.clone()))
        .unwrap()
        .serve_at("/org/freedesktop/secrets/aliases/default", Collection(store.clone()))
        .unwrap()
        .name("org.freedesktop.secrets")
        .unwrap()
        .build()
        .await
        .unwrap();
    let conn = bus.connect().await;
    let (ui, _rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(helix_shell::network_agent::run_with_secrets(conn.clone(), Some(conn.clone()), ui));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let me = conn.unique_name().unwrap().to_owned();
    type Settings = HashMap<String, HashMap<String, OwnedValue>>;
    let path = OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/7").unwrap();
    let saved: Settings = HashMap::from([
        ("connection".to_owned(), HashMap::from([("uuid".to_owned(), v("u-1")), ("id".to_owned(), v("Home"))])),
        ("802-11-wireless-security".to_owned(), HashMap::from([("key-mgmt".to_owned(), v("wpa-psk")), ("psk".to_owned(), v("hunter2hunter2")), ("psk-flags".to_owned(), v(1u32))])),
    ]);
    conn.call_method(Some(me.as_str()), helix_shell::network_agent::AGENT_PATH, Some("org.freedesktop.NetworkManager.SecretAgent"), "SaveSecrets", &(&saved, &path)).await.unwrap();
    {
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1.get("setting-key").map(String::as_str), Some("psk"));
        assert_eq!(s[0].2, b"hunter2hunter2");
    }
    // Asked again (no interaction allowed): answered from the keyring.
    let asked: Settings = HashMap::from([
        ("connection".to_owned(), HashMap::from([("uuid".to_owned(), v("u-1")), ("id".to_owned(), v("Home"))])),
        ("802-11-wireless-security".to_owned(), HashMap::from([("key-mgmt".to_owned(), v("wpa-psk"))])),
    ]);
    let reply = conn
        .call_method(Some(me.as_str()), helix_shell::network_agent::AGENT_PATH, Some("org.freedesktop.NetworkManager.SecretAgent"), "GetSecrets", &(&asked, &path, "802-11-wireless-security", Vec::<String>::new(), 0u32))
        .await
        .unwrap();
    let answer: Settings = reply.body().deserialize().unwrap();
    assert_eq!(String::try_from(answer["802-11-wireless-security"]["psk"].clone()).unwrap(), "hunter2hunter2");
    // Forgotten with the connection.
    conn.call_method(Some(me.as_str()), helix_shell::network_agent::AGENT_PATH, Some("org.freedesktop.NetworkManager.SecretAgent"), "DeleteSecrets", &(&saved, &path)).await.unwrap();
    assert!(store.lock().unwrap().is_empty());
}
