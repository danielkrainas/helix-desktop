//! A fake `org.freedesktop.login1` for tests (contract:
//! helix-desktop-spec `org.freedesktop.login1.xml`). Never talk to the real
//! logind from tests: it would power off the machine.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};
use zbus::{fdo, interface, Connection};

pub const BUS_NAME: &str = "org.freedesktop.login1";
pub const MANAGER_PATH: &str = "/org/freedesktop/login1";
pub const SESSION_PATH: &str = "/org/freedesktop/login1/session/_31";

/// What the fake saw and how it answers.
#[derive(Default)]
pub struct Record {
    /// Calls such as `"PowerOff false"`.
    pub calls: Vec<String>,
    /// `CanPowerOff` etc. answers by method name; missing means `"yes"`.
    pub can: HashMap<String, String>,
    /// Inhibitors handed out: (what, mode, our end of the socket).
    pub inhibitors: Vec<(String, String, UnixStream)>,
    /// Refuse `Suspend` (as logind does when sleep is blocked).
    pub refuse_suspend: bool,
}

pub type Shared = Arc<Mutex<Record>>;

fn record(shared: &Shared) -> std::sync::MutexGuard<'_, Record> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

struct Manager {
    shared: Shared,
}

impl Manager {
    fn can(&self, what: &str) -> String {
        record(&self.shared).can.get(what).cloned().unwrap_or_else(|| "yes".into())
    }

    fn call(&self, call: String) {
        record(&self.shared).calls.push(call);
    }
}

#[interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    async fn get_session(&self, _session_id: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(SESSION_PATH).expect("valid path")
    }

    #[zbus(name = "GetSessionByPID")]
    async fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        OwnedObjectPath::try_from(SESSION_PATH).expect("valid path")
    }

    async fn power_off(&self, interactive: bool) {
        self.call(format!("PowerOff {interactive}"));
    }

    async fn reboot(&self, interactive: bool) {
        self.call(format!("Reboot {interactive}"));
    }

    async fn suspend(&self, interactive: bool) -> fdo::Result<()> {
        if record(&self.shared).refuse_suspend {
            return Err(fdo::Error::AccessDenied("Operation inhibited".into()));
        }
        self.call(format!("Suspend {interactive}"));
        Ok(())
    }

    async fn can_power_off(&self) -> String {
        self.can("CanPowerOff")
    }

    async fn can_reboot(&self) -> String {
        self.can("CanReboot")
    }

    async fn can_suspend(&self) -> String {
        self.can("CanSuspend")
    }

    async fn inhibit(&self, what: &str, _who: &str, _why: &str, mode: &str) -> fdo::Result<OwnedFd> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        ours.set_nonblocking(true).map_err(|e| fdo::Error::Failed(e.to_string()))?;
        record(&self.shared).inhibitors.push((what.into(), mode.into(), ours));
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(theirs)))
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;
}

struct Session {
    active: bool,
}

#[interface(name = "org.freedesktop.login1.Session")]
impl Session {
    #[zbus(property)]
    async fn id(&self) -> String {
        "1".into()
    }

    #[zbus(property)]
    async fn active(&self) -> bool {
        self.active
    }
}

/// The fake, serving on its own connection to a test bus.
pub struct FakeLogind {
    conn: Connection,
    pub shared: Shared,
}

impl FakeLogind {
    pub async fn start(bus: &crate::TestBus) -> Self {
        let shared = Shared::default();
        let conn = bus
            .builder()
            .serve_at(MANAGER_PATH, Manager { shared: shared.clone() })
            .expect("serve manager")
            .serve_at(SESSION_PATH, Session { active: true })
            .expect("serve session")
            .name(BUS_NAME)
            .expect("valid name")
            .build()
            .await
            .expect("start fake logind");
        Self { conn, shared }
    }

    pub fn calls(&self) -> Vec<String> {
        record(&self.shared).calls.clone()
    }

    pub fn set_can(&self, method: &str, answer: &str) {
        record(&self.shared).can.insert(method.into(), answer.into());
    }

    pub fn refuse_suspend(&self, refuse: bool) {
        record(&self.shared).refuse_suspend = refuse;
    }

    /// Inhibitors still held by their taker: (what, mode).
    pub fn held_inhibitors(&self) -> Vec<(String, String)> {
        let mut rec = record(&self.shared);
        rec.inhibitors.retain_mut(|(_, _, stream)| {
            let mut buf = [0u8; 1];
            // EOF means the taker closed its end.
            !matches!(stream.read(&mut buf), Ok(0))
        });
        rec.inhibitors.iter().map(|(w, m, _)| (w.clone(), m.clone())).collect()
    }

    pub async fn set_active(&self, active: bool) {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Session>(SESSION_PATH)
            .await
            .expect("session object");
        iface.get_mut().await.active = active;
        iface.get().await.active_changed(iface.signal_emitter()).await.expect("emit");
    }

    pub async fn prepare_for_sleep(&self, start: bool) {
        let emitter = SignalEmitter::new(&self.conn, MANAGER_PATH).expect("emitter");
        Manager::prepare_for_sleep(&emitter, start).await.expect("emit");
    }
}
