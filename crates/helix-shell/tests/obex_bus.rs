//! Bluetooth file transfer against a fake obexd on a private bus
//! (helix-desktop-spec `contracts/dbus/org.bluez.obex.md`). No real
//! Bluetooth device is involved.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_shell::obex::{self, Ask, ObexAgent, Offer, Received, Sending, Tell};
use helix_test_support::TestBus;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{interface, Connection};

#[derive(Default)]
struct Log {
    calls: Vec<String>,
    agent: Option<(String, OwnedObjectPath)>,
    /// Make transfers fail.
    fail: bool,
    next: u32,
}

type Shared = Arc<Mutex<Log>>;

struct Transfer {
    status: String,
    name: String,
    size: u64,
    transferred: u64,
    filename: String,
    session: OwnedObjectPath,
}

#[interface(name = "org.bluez.obex.Transfer1")]
impl Transfer {
    #[zbus(property)]
    fn status(&self) -> String {
        self.status.clone()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        self.size
    }
    #[zbus(property)]
    fn transferred(&self) -> u64 {
        self.transferred
    }
    #[zbus(property)]
    fn filename(&self) -> String {
        self.filename.clone()
    }
    #[zbus(property)]
    fn session(&self) -> OwnedObjectPath {
        self.session.clone()
    }
}

struct Session {
    destination: String,
}

#[interface(name = "org.bluez.obex.Session1")]
impl Session {
    #[zbus(property)]
    fn destination(&self) -> String {
        self.destination.clone()
    }
}

/// Run a transfer: active halfway, then complete (or error); then gone.
async fn play(conn: Connection, path: OwnedObjectPath, fail: bool, write: Option<(PathBuf, Vec<u8>)>) {
    let iface = conn.object_server().interface::<_, Transfer>(path.as_str()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let mut t = iface.get_mut().await;
        t.status = "active".into();
        t.transferred = t.size / 2;
        if let Some((file, _)) = &write {
            t.filename = file.to_string_lossy().into_owned();
        }
        let emitter = iface.signal_emitter();
        t.status_changed(emitter).await.unwrap();
        t.transferred_changed(emitter).await.unwrap();
        t.filename_changed(emitter).await.unwrap();
    }
    if let Some((file, data)) = &write {
        std::fs::write(file, data).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let mut t = iface.get_mut().await;
        t.status = if fail { "error".into() } else { "complete".into() };
        t.transferred = if fail { t.transferred } else { t.size };
        let emitter = iface.signal_emitter();
        t.transferred_changed(emitter).await.unwrap();
        t.status_changed(emitter).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    conn.object_server().remove::<Transfer, _>(path.as_str()).await.unwrap();
}

struct Push {
    log: Shared,
    path: OwnedObjectPath,
}

#[interface(name = "org.bluez.obex.ObjectPush1")]
impl Push {
    async fn send_file(&self, sourcefile: String, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        let size = std::fs::metadata(&sourcefile).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?.len();
        let (n, fail) = {
            let mut log = self.log.lock().unwrap();
            log.calls.push(format!("SendFile {sourcefile}"));
            log.next += 1;
            (log.next, log.fail)
        };
        let path = OwnedObjectPath::try_from(format!("{}/transfer{n}", self.path.as_str())).unwrap();
        let name = sourcefile.rsplit('/').next().unwrap().to_owned();
        let transfer = Transfer { status: "queued".into(), name, size, transferred: 0, filename: sourcefile, session: self.path.clone() };
        conn.object_server().at(path.as_str(), transfer).await?;
        tokio::spawn(play(conn.clone(), path.clone(), fail, None));
        Ok((path, HashMap::new()))
    }
}

struct Client(Shared);

#[interface(name = "org.bluez.obex.Client1")]
impl Client {
    async fn create_session(&self, destination: String, args: HashMap<String, OwnedValue>, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<OwnedObjectPath> {
        let target = args.get("Target").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        self.0.lock().unwrap().calls.push(format!("CreateSession {destination} {target}"));
        let path = OwnedObjectPath::try_from("/org/bluez/obex/client/session0").unwrap();
        conn.object_server().at(path.as_str(), Session { destination }).await?;
        conn.object_server().at(path.as_str(), Push { log: self.0.clone(), path: path.clone() }).await?;
        Ok(path)
    }

    fn remove_session(&self, session: OwnedObjectPath) {
        self.0.lock().unwrap().calls.push(format!("RemoveSession {}", session.as_str()));
    }
}

struct Manager(Shared);

#[interface(name = "org.bluez.obex.AgentManager1")]
impl Manager {
    fn register_agent(&self, agent: OwnedObjectPath, #[zbus(header)] header: zbus::message::Header<'_>) {
        let sender = header.sender().unwrap().to_string();
        self.0.lock().unwrap().agent = Some((sender, agent));
    }
}

async fn fake_obexd(bus: &TestBus) -> (Connection, Shared) {
    let log = Shared::default();
    let conn = bus
        .builder()
        .name(obex::OBEX)
        .unwrap()
        .serve_at(obex::OBEX_PATH, Client(log.clone()))
        .unwrap()
        .serve_at(obex::OBEX_PATH, Manager(log.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, log)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("helix-obex-bus-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn sending_files() {
    let Some(bus) = TestBus::start() else { return };
    let (_obexd, log) = fake_obexd(&bus).await;
    let dir = scratch("send");
    let (a, b) = (dir.join("a.txt"), dir.join("b.bin"));
    std::fs::write(&a, vec![1u8; 100]).unwrap();
    std::fs::write(&b, vec![2u8; 1000]).unwrap();
    let conn = bus.connect().await;
    let mut seen = Vec::new();
    obex::send(&conn, "AA:BB:CC:DD:EE:FF", &[a.clone(), b.clone()], |s| seen.push(s)).await.unwrap();
    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(
        calls,
        [
            "CreateSession AA:BB:CC:DD:EE:FF opp".to_owned(),
            format!("SendFile {}", a.display()),
            format!("SendFile {}", b.display()),
            "RemoveSession /org/bluez/obex/client/session0".to_owned(),
        ]
    );
    let file = |index: usize, name: &str, percent: u32| Sending::File { index, count: 2, name: name.into(), percent };
    assert_eq!(seen, [file(0, "a.txt", 0), file(0, "a.txt", 50), file(0, "a.txt", 100), file(1, "b.bin", 0), file(1, "b.bin", 50), file(1, "b.bin", 100)]);

    // A transfer that fails ends the sending, and the session is removed.
    log.lock().unwrap().fail = true;
    log.lock().unwrap().calls.clear();
    let err = obex::send(&conn, "AA:BB:CC:DD:EE:FF", &[a.clone(), b.clone()], |_| {}).await.unwrap_err();
    assert_eq!(err, "a.txt was not sent");
    assert_eq!(log.lock().unwrap().calls.last().unwrap(), "RemoveSession /org/bluez/obex/client/session0");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What the test's "user" answers.
enum Reply {
    Yes,
    No,
    Never,
}

struct Receiver {
    obexd: Connection,
    log: Shared,
    offers: Arc<Mutex<Vec<Offer>>>,
    told: tokio::sync::mpsc::UnboundedReceiver<Received>,
    downloads: PathBuf,
    cache: PathBuf,
}

async fn receiver(bus: &TestBus, name: &str, reply: Reply) -> Receiver {
    let (obexd, log) = fake_obexd(bus).await;
    let dir = scratch(name);
    let (downloads, cache) = (dir.join("Downloads"), dir.join("cache"));
    std::fs::create_dir_all(&cache).unwrap();
    let offers: Arc<Mutex<Vec<Offer>>> = Default::default();
    let asked = offers.clone();
    let reply = Arc::new(reply);
    let ask: Ask = Arc::new(move |offer| {
        asked.lock().unwrap().push(offer);
        let reply = reply.clone();
        Box::pin(async move {
            match *reply {
                Reply::Yes => true,
                Reply::No => false,
                Reply::Never => std::future::pending().await,
            }
        })
    });
    let (tx, told) = tokio::sync::mpsc::unbounded_channel();
    let tell: Tell = Arc::new(move |r| {
        let _ = tx.send(r);
    });
    let session = bus.connect().await;
    let agent = ObexAgent::new(session.clone(), None, ask, tell, downloads.clone());
    tokio::spawn(obex::serve_agent(session, agent, false));
    for _ in 0..100 {
        if log.lock().unwrap().agent.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(log.lock().unwrap().agent.is_some(), "agent registered with obexd");
    Receiver { obexd, log, offers, told, downloads, cache }
}

impl Receiver {
    /// A device pushes `name`; obexd asks the agent, then stores the file
    /// under the name the agent gives.
    async fn push(&self, name: &str, data: &[u8]) -> zbus::Result<String> {
        let session = OwnedObjectPath::try_from("/org/bluez/obex/server/session1").unwrap();
        let _ = self.obexd.object_server().at(session.as_str(), Session { destination: "11:22:33:44:55:66".into() }).await;
        let path = OwnedObjectPath::try_from("/org/bluez/obex/server/session1/transfer1").unwrap();
        let transfer = Transfer { status: "queued".into(), name: name.into(), size: data.len() as u64, transferred: 0, filename: String::new(), session };
        self.obexd.object_server().at(path.as_str(), transfer).await?;
        let (sender, agent) = self.log.lock().unwrap().agent.clone().unwrap();
        let reply = self.obexd.call_method(Some(sender.as_str()), agent.as_str(), Some("org.bluez.obex.Agent1"), "AuthorizePush", &(&path,)).await;
        let stored: String = match reply {
            Ok(reply) => reply.body().deserialize()?,
            Err(err) => {
                self.obexd.object_server().remove::<Transfer, _>(path.as_str()).await?;
                return Err(err);
            }
        };
        assert!(!stored.contains('/'), "a bare name: {stored}");
        tokio::spawn(play(self.obexd.clone(), path, false, Some((self.cache.join(&stored), data.to_vec()))));
        Ok(stored)
    }
}

#[tokio::test]
async fn receiving_a_file() {
    let Some(bus) = TestBus::start() else { return };
    let mut r = receiver(&bus, "accept", Reply::Yes).await;
    std::fs::create_dir_all(&r.downloads).unwrap();
    std::fs::write(r.downloads.join("photo.jpg"), "older").unwrap();
    let stored = r.push("../photo.jpg", b"JPEG").await.unwrap();
    assert_eq!(stored, "photo.jpg");
    assert_eq!(r.offers.lock().unwrap()[0], Offer { name: "../photo.jpg".into(), size: 4, device: "11:22:33:44:55:66".into() });
    let told = tokio::time::timeout(Duration::from_secs(5), r.told.recv()).await.unwrap().unwrap();
    let saved = r.downloads.join("photo (2).jpg");
    assert!(matches!(&told, Received::Saved { path, .. } if *path == saved), "{told:?}");
    assert_eq!(std::fs::read(&saved).unwrap(), b"JPEG");
    assert!(!r.cache.join("photo.jpg").exists(), "moved out of obexd's folder");
}

#[tokio::test]
async fn declining_a_file() {
    let Some(bus) = TestBus::start() else { return };
    let r = receiver(&bus, "decline", Reply::No).await;
    let err = r.push("virus.exe", b"MZ").await.unwrap_err();
    assert!(matches!(&err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.obex.Error.Rejected"), "{err:?}");
    assert!(!r.downloads.exists());
}

#[tokio::test]
async fn obexd_cancels_the_question() {
    let Some(bus) = TestBus::start() else { return };
    let r = receiver(&bus, "cancel", Reply::Never).await;
    let (sender, agent) = r.log.lock().unwrap().agent.clone().unwrap();
    let obexd = r.obexd.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        obexd.call_method(Some(sender.as_str()), agent.as_str(), Some("org.bluez.obex.Agent1"), "Cancel", &()).await.unwrap();
    });
    let err = tokio::time::timeout(Duration::from_secs(5), r.push("a.txt", b"x")).await.expect("answered after Cancel").unwrap_err();
    assert!(matches!(&err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.obex.Error.Canceled"), "{err:?}");
}
