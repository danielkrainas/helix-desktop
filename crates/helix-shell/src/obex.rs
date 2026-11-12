//! Bluetooth file transfer through BlueZ's OBEX daemon (helix-desktop-spec
//! `contracts/dbus/org.bluez.obex.md`): an `org.bluez.obex.Agent1` that
//! asks before accepting pushed files and moves them to Downloads, and
//! `org.helix.BluetoothSendTo`, which sends files to a paired device.
//!
//! Not yet tried with a real Bluetooth device: the tests use a fake obexd.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

use crate::notifications::Center;
use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const OBEX: &str = "org.bluez.obex";
pub const OBEX_PATH: &str = "/org/bluez/obex";
pub const AGENT_PATH: &str = "/org/helix/ObexAgent";
pub const SEND_PATH: &str = "/org/helix/BluetoothSendTo";
const BLUEZ: &str = "org.bluez";
const TRANSFER: &str = "org.bluez.obex.Transfer1";
/// Progress notifications are updated in steps of this many percent.
const STEP: u32 = 10;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.obex.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// A pushed object's name made safe to store: no directories, no leading
/// dots, not empty.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let trimmed = base.trim().trim_start_matches('.').replace('\0', "");
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed
    }
}

/// `dir/name`, or `dir/stem (2).ext` and so on if that is taken.
pub fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

/// A size as people read it (1 kB = 1000 bytes).
pub fn size_text(bytes: u64) -> String {
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let units = ["kB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < units.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

pub fn percent(done: u64, size: u64) -> u32 {
    (done.min(size) * 100).checked_div(size).unwrap_or(0) as u32
}

/// A local path from an absolute path or a `file://` URI.
pub fn file_of(arg: &str) -> Option<PathBuf> {
    if arg.starts_with("file:") {
        gio::prelude::FileExt::path(&gio::File::for_uri(arg))
    } else {
        Some(PathBuf::from(arg)).filter(|p| p.is_absolute())
    }
}

/// Where received files go.
pub fn downloads_dir() -> PathBuf {
    gio::glib::user_special_dir(gio::glib::UserDirectory::Downloads)
        .unwrap_or_else(|| gio::glib::home_dir().join("Downloads"))
}

fn prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| v.try_clone().ok()).and_then(|v| T::try_from(v).ok())
}

async fn all_props(conn: &Connection, path: &str, iface: &str) -> zbus::Result<HashMap<String, OwnedValue>> {
    let reply = conn.call_method(Some(OBEX), path, Some("org.freedesktop.DBus.Properties"), "GetAll", &(iface,)).await?;
    reply.body().deserialize()
}

/// BlueZ's paired devices: (address, name), by name.
pub async fn paired_devices(system: &Connection) -> Vec<(String, String)> {
    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
    let objects: Objects = match system.call_method(Some(BLUEZ), "/", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = objects
        .values()
        .filter_map(|ifaces| ifaces.get("org.bluez.Device1"))
        .filter(|d| prop::<bool>(d, "Paired").unwrap_or(false))
        .filter_map(|d| {
            let address: String = prop(d, "Address")?;
            let name = prop::<String>(d, "Alias").or_else(|| prop(d, "Name")).unwrap_or_else(|| address.clone());
            Some((address, name))
        })
        .collect();
    out.sort_by_key(|d| d.1.to_lowercase());
    out
}

/// The name BlueZ knows `address` by, else the address.
pub async fn device_name(system: Option<&Connection>, address: &str) -> String {
    let Some(system) = system else { return address.to_owned() };
    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
    let objects: Objects = match system.call_method(Some(BLUEZ), "/", Some("org.freedesktop.DBus.ObjectManager"), "GetManagedObjects", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(_) => return address.to_owned(),
    };
    objects
        .values()
        .filter_map(|ifaces| ifaces.get("org.bluez.Device1"))
        .find(|d| prop::<String>(d, "Address").is_some_and(|a| a.eq_ignore_ascii_case(address)))
        .and_then(|d| prop::<String>(d, "Alias").or_else(|| prop(d, "Name")))
        .unwrap_or_else(|| address.to_owned())
}

/// How a transfer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Complete,
    Error,
}

/// What is known of a transfer so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    pub transferred: u64,
    pub filename: Option<String>,
}

/// A transfer being followed; subscribed from creation on, so nothing
/// that happens after [`TransferWatch::new`] is missed.
pub struct TransferWatch {
    conn: Connection,
    transfer: String,
    changes: zbus::fdo::PropertiesChangedStream,
    owners: zbus::fdo::NameOwnerChangedStream,
}

impl TransferWatch {
    pub async fn new(conn: &Connection, transfer: &str) -> zbus::Result<Self> {
        let properties = zbus::fdo::PropertiesProxy::builder(conn).destination(OBEX)?.path(transfer.to_owned())?.build().await?;
        let changes = properties.receive_properties_changed().await?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let owners = dbus.receive_name_owner_changed_with_args(&[(0, OBEX)]).await?;
        Ok(Self { conn: conn.clone(), transfer: transfer.to_owned(), changes, owners })
    }

    /// Follow the transfer until it ends, reporting what is seen. A
    /// transfer already gone counts as complete (obexd removes transfers
    /// only once they have ended).
    pub async fn run(mut self, mut seen_now: impl FnMut(&Seen)) -> zbus::Result<(Ended, Seen)> {
        let ended = |status: &str| match status {
            "complete" => Some(Ended::Complete),
            "error" => Some(Ended::Error),
            _ => None,
        };
        let mut seen = Seen::default();
        match all_props(&self.conn, &self.transfer, TRANSFER).await {
            Ok(now) => {
                seen.transferred = prop::<u64>(&now, "Transferred").unwrap_or(0);
                seen.filename = prop::<String>(&now, "Filename").filter(|f| !f.is_empty());
                seen_now(&seen);
                if let Some(end) = prop::<String>(&now, "Status").as_deref().and_then(ended) {
                    return Ok((end, seen));
                }
            }
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" => {
                return Ok((Ended::Complete, seen))
            }
            Err(err) => return Err(err),
        }
        loop {
            tokio::select! {
                change = self.changes.next() => {
                    let Some(change) = change else { return Ok((Ended::Error, seen)) };
                    let Ok(args) = change.args() else { continue };
                    if args.interface_name().as_str() != TRANSFER {
                        continue;
                    }
                    let changed = args.changed_properties();
                    if let Some(done) = changed.get("Transferred").and_then(|v| u64::try_from(v).ok()) {
                        seen.transferred = done;
                    }
                    if let Some(file) = changed.get("Filename").and_then(|v| <&str>::try_from(v).ok()).filter(|f| !f.is_empty()) {
                        seen.filename = Some(file.to_owned());
                    }
                    seen_now(&seen);
                    if let Some(end) = changed.get("Status").and_then(|v| <&str>::try_from(v).ok()).and_then(ended) {
                        return Ok((end, seen));
                    }
                }
                change = self.owners.next() => {
                    if change.is_none_or(|c| c.args().is_ok_and(|a| a.new_owner().is_none())) {
                        return Ok((Ended::Error, seen));
                    }
                }
            }
        }
    }
}

/// Progress of [`send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sending {
    /// File `index` of `count` (from 0), `percent` done.
    File { index: usize, count: usize, name: String, percent: u32 },
}

/// Send `files` to the device at `address` over OPP, one after another;
/// `report` hears about progress (in steps of [`STEP`] percent).
pub async fn send(conn: &Connection, address: &str, files: &[PathBuf], mut report: impl FnMut(Sending)) -> Result<(), String> {
    let mut args: HashMap<&str, Value<'_>> = HashMap::new();
    args.insert("Target", Value::from("opp"));
    let reply = conn
        .call_method(Some(OBEX), OBEX_PATH, Some("org.bluez.obex.Client1"), "CreateSession", &(address, args))
        .await
        .map_err(|e| format!("cannot connect to the device: {e}"))?;
    let session: OwnedObjectPath = reply.body().deserialize().map_err(|e| e.to_string())?;
    let result = async {
        for (index, file) in files.iter().enumerate() {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            report(Sending::File { index, count: files.len(), name: name.clone(), percent: 0 });
            let path = file.to_str().ok_or("the file name is not UTF-8")?;
            let reply = conn
                .call_method(Some(OBEX), session.as_str(), Some("org.bluez.obex.ObjectPush1"), "SendFile", &(path,))
                .await
                .map_err(|e| format!("cannot send {name}: {e}"))?;
            let (transfer, _props): (OwnedObjectPath, HashMap<String, OwnedValue>) = reply.body().deserialize().map_err(|e| e.to_string())?;
            let mut shown = 0;
            let watch = TransferWatch::new(conn, transfer.as_str()).await.map_err(|e| e.to_string())?;
            let (ended, _) = watch.run(|seen| {
                let now = percent(seen.transferred, size) / STEP * STEP;
                if now > shown {
                    shown = now;
                    report(Sending::File { index, count: files.len(), name: name.clone(), percent: now });
                }
            })
            .await
            .map_err(|e| e.to_string())?;
            if ended == Ended::Error {
                return Err(format!("{name} was not sent"));
            }
        }
        Ok(())
    }
    .await;
    let _ = conn.call_method(Some(OBEX), OBEX_PATH, Some("org.bluez.obex.Client1"), "RemoveSession", &(&session,)).await;
    result
}

/// A push a device offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub name: String,
    pub size: u64,
    pub device: String,
}

/// What became of an accepted push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    Saved { offer: Offer, path: PathBuf },
    Failed { offer: Offer },
}

pub type Ask = Arc<dyn Fn(Offer) -> BoxFuture<'static, bool> + Send + Sync>;
pub type Tell = Arc<dyn Fn(Received) + Send + Sync>;

/// Move `from` to `to`, copying across file systems.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// Where obexd stores received files by default.
fn obexd_folder() -> PathBuf {
    gio::glib::user_cache_dir().join("obexd")
}

/// After a push ended: move the file to `downloads` as `name` (made
/// unique), or remove what was stored of a failed one.
pub fn finish(ended: Ended, file: &Path, downloads: &Path, name: &str, offer: Offer) -> Received {
    if ended == Ended::Complete {
        let _ = std::fs::create_dir_all(downloads);
        let target = unique_in(downloads, name);
        match move_file(file, &target) {
            Ok(()) => return Received::Saved { offer, path: target },
            Err(err) => tracing::warn!(%err, file = %file.display(), "cannot move the received file to Downloads"),
        }
    } else {
        let _ = std::fs::remove_file(file);
    }
    Received::Failed { offer }
}

pub struct ObexAgent {
    session: Connection,
    system: Option<Connection>,
    ask: Ask,
    tell: Tell,
    downloads: PathBuf,
    cancel: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl ObexAgent {
    pub fn new(session: Connection, system: Option<Connection>, ask: Ask, tell: Tell, downloads: PathBuf) -> Self {
        Self { session, system, ask, tell, downloads, cancel: Default::default() }
    }
}

#[interface(name = "org.bluez.obex.Agent1")]
impl ObexAgent {
    async fn release(&self) {
        tracing::info!("obexd released the file transfer agent");
    }

    async fn authorize_push(&self, transfer: OwnedObjectPath) -> Result<String, AgentError> {
        let props = all_props(&self.session, transfer.as_str(), TRANSFER).await?;
        let name = prop::<String>(&props, "Name").unwrap_or_default();
        let size = prop::<u64>(&props, "Size").unwrap_or(0);
        let address = match prop::<OwnedObjectPath>(&props, "Session") {
            Some(session) => all_props(&self.session, session.as_str(), "org.bluez.obex.Session1").await.ok().and_then(|p| prop::<String>(&p, "Destination")),
            None => None,
        };
        let device = match address {
            Some(address) => device_name(self.system.as_ref(), &address).await,
            None => "A device".into(),
        };
        let offer = Offer { name: name.clone(), size, device };
        tracing::info!(?offer, "Bluetooth file offered");
        let (cancel, canceled) = oneshot::channel();
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel);
        let accepted = tokio::select! {
            yes = (self.ask)(offer.clone()) => yes,
            _ = canceled => return Err(AgentError::Canceled("canceled by obexd".into())),
        };
        self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take();
        if !accepted {
            return Err(AgentError::Rejected("declined".into()));
        }
        let stored = safe_name(&name);
        // Followed from before the reply, so a quick transfer is not missed.
        let watch = TransferWatch::new(&self.session, transfer.as_str()).await?;
        let (tell, downloads) = (self.tell.clone(), self.downloads.clone());
        let fallback = obexd_folder().join(&stored);
        let target = stored.clone();
        tokio::spawn(async move {
            let result = watch.run(|_| {}).await;
            let file = match &result {
                Ok((_, seen)) => seen.filename.as_ref().map(PathBuf::from).unwrap_or(fallback),
                Err(_) => fallback,
            };
            tell(finish(result.map(|r| r.0).unwrap_or(Ended::Error), &file, &downloads, &target, offer));
        });
        Ok(stored)
    }

    async fn cancel(&self) {
        if let Some(cancel) = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = cancel.send(());
        }
    }
}

/// Register the agent at [`AGENT_PATH`] with obexd.
async fn register(session: &Connection) {
    let path = OwnedObjectPath::try_from(AGENT_PATH).expect("valid path");
    match session.call_method(Some(OBEX), OBEX_PATH, Some("org.bluez.obex.AgentManager1"), "RegisterAgent", &(&path,)).await {
        Ok(_) => tracing::info!("Bluetooth file transfer agent registered"),
        Err(err) => tracing::info!(%err, "obexd did not take the file transfer agent"),
    }
}

/// Serve `agent` for the rest of the session, registering it whenever
/// obexd appears; with `start`, obexd is started first.
pub async fn serve_agent(session: Connection, agent: ObexAgent, start: bool) -> zbus::Result<()> {
    session.object_server().at(AGENT_PATH, agent).await?;
    let dbus = zbus::fdo::DBusProxy::new(&session).await?;
    let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, OBEX)]).await?;
    let name = zbus::names::BusName::from_static_str(OBEX)?;
    if start && !dbus.name_has_owner(name.clone()).await.unwrap_or(false) {
        if let Err(err) = dbus.start_service_by_name(zbus::names::WellKnownName::from_static_str(OBEX)?, 0).await {
            tracing::info!(%err, "cannot start obexd; no Bluetooth file transfers");
        }
    }
    if dbus.name_has_owner(name).await.unwrap_or(false) {
        register(&session).await;
    }
    while let Some(change) = owners.next().await {
        if change.args().is_ok_and(|a| a.new_owner().is_some()) {
            register(&session).await;
        }
    }
    Ok(())
}

/// The shell's side: dialogs and notifications.
#[derive(Clone)]
struct Shell {
    session: Connection,
    system: Option<Connection>,
    ui: mpsc::UnboundedSender<UiEvent>,
    center: Center,
}

static SHELL: std::sync::OnceLock<Shell> = std::sync::OnceLock::new();

/// Closes the prompt dialog when dropped (also when the question is
/// abandoned).
struct PromptOpen(mpsc::UnboundedSender<UiEvent>);

impl Drop for PromptOpen {
    fn drop(&mut self) {
        prompt::done(&self.0);
    }
}

/// The question asked before accepting a pushed file.
pub fn offer_prompt(offer: &Offer) -> Prompt {
    let size = if offer.size > 0 { format!(" ({})", size_text(offer.size)) } else { String::new() };
    Prompt {
        title: "Receive a File?".into(),
        message: format!("{} wants to send you “{}”{size}.", offer.device, offer.name),
        description: "Accepted files are saved in Downloads.".into(),
        continue_label: "Accept".into(),
        cancel_label: "Decline".into(),
        ..Prompt::default()
    }
}

/// A sending notification's summary and body.
pub fn sending_text(device: &str, progress: &Sending) -> (String, String) {
    let Sending::File { index, count, name, percent } = progress;
    let which = if *count > 1 { format!(" (file {} of {count})", index + 1) } else { String::new() };
    (format!("Sending files to {device}"), format!("{name}{which}: {percent}%"))
}

impl Shell {
    fn notice(&self, replaces: u32, summary: &str, body: &str) -> u32 {
        self.center.notice_replacing(replaces, "Bluetooth", summary, body, "bluetooth-active-symbolic")
    }

    /// Ask which paired device to send to: (address, name).
    async fn pick_device(&self) -> Option<(String, String)> {
        let devices = match &self.system {
            Some(system) => paired_devices(system).await,
            None => Vec::new(),
        };
        if devices.is_empty() {
            self.notice(0, "No Bluetooth devices", "Pair a device to send files to it.");
            return None;
        }
        let _turn = prompt::turn().await;
        let _open = PromptOpen(self.ui.clone());
        let prompt = Prompt {
            title: "Send Files".into(),
            message: "Send files to which device?".into(),
            list: devices.iter().map(|d| d.1.clone()).collect(),
            continue_label: "Choose".into(),
            cancel_label: "Cancel".into(),
            ..Prompt::default()
        };
        match prompt::ask(&self.ui, prompt).await {
            Answer::Continue { selected, .. } => devices.get(selected).cloned(),
            Answer::Cancel => None,
        }
    }

    /// Ask for files with Helix's file chooser.
    async fn pick_files(&self) -> Vec<PathBuf> {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Ok(handle) = OwnedObjectPath::try_from(format!("{SEND_PATH}/request{n}")) else { return Vec::new() };
        let portal = std::env::var("HELIX_PORTALS_BUS_NAME").unwrap_or_else(|_| "org.freedesktop.impl.portal.desktop.helix".into());
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("multiple", Value::from(true));
        options.insert("accept_label", Value::from("Send"));
        let reply = self
            .session
            .call_method(
                Some(portal.as_str()),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.impl.portal.FileChooser"),
                "OpenFile",
                &(&handle, "org.gnome.Shell", "", "Choose Files to Send", options),
            )
            .await;
        let Ok(reply) = reply else {
            tracing::warn!("no file chooser for sending files");
            return Vec::new();
        };
        let Ok((code, results)) = reply.body().deserialize::<(u32, HashMap<String, OwnedValue>)>() else { return Vec::new() };
        if code != 0 {
            return Vec::new();
        }
        prop::<Vec<String>>(&results, "uris").unwrap_or_default().iter().filter_map(|u| file_of(u)).collect()
    }

    async fn send_files(self, device: String, files: Vec<String>) {
        let (address, name) = if device.is_empty() {
            match self.pick_device().await {
                Some(picked) => picked,
                None => return,
            }
        } else {
            let name = device_name(self.system.as_ref(), &device).await;
            (device, name)
        };
        let mut paths: Vec<PathBuf> = files.iter().filter_map(|f| file_of(f)).filter(|p| p.is_file()).collect();
        if files.is_empty() {
            paths = self.pick_files().await;
        }
        if paths.is_empty() {
            return;
        }
        tracing::info!(device = name, files = paths.len(), "sending files over Bluetooth");
        let id = Arc::new(Mutex::new(self.notice(0, &format!("Sending files to {name}"), "Connecting…")));
        let progress = {
            let (shell, id, name) = (self.clone(), id.clone(), name.clone());
            move |sending: Sending| {
                let (summary, body) = sending_text(&name, &sending);
                let mut id = id.lock().unwrap_or_else(|e| e.into_inner());
                *id = shell.notice(*id, &summary, &body);
            }
        };
        let result = send(&self.session, &address, &paths, progress).await;
        let id = *id.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(()) => {
                let what = if paths.len() == 1 { "1 file".to_owned() } else { format!("{} files", paths.len()) };
                self.notice(id, &format!("Files sent to {name}"), &format!("Sent {what}."));
            }
            Err(err) => {
                tracing::warn!(err, "Bluetooth sending failed");
                self.notice(id, "Sending failed", &err);
            }
        }
    }
}

/// Send files, asking for what is missing (see [`SendTo::send_files`]).
pub fn start_sending(device: String, files: Vec<String>) {
    match SHELL.get() {
        Some(shell) => {
            tokio::spawn(shell.clone().send_files(device, files));
        }
        None => tracing::warn!("Bluetooth file sending is not running"),
    }
}

/// `org.helix.BluetoothSendTo`.
pub struct SendTo;

#[interface(name = "org.helix.BluetoothSendTo")]
impl SendTo {
    /// Send `files` (paths or `file://` URIs) to `device` (an address);
    /// either may be empty, and is then asked for.
    async fn send_files(&self, device: String, files: Vec<String>) -> zbus::fdo::Result<()> {
        if SHELL.get().is_none() {
            return Err(zbus::fdo::Error::Failed("Bluetooth file sending is not running".into()));
        }
        start_sending(device, files);
        Ok(())
    }
}

/// Receive (with `receive`) and send files for the rest of the session.
pub async fn run(session: Connection, ui: mpsc::UnboundedSender<UiEvent>, center: Center, receive: bool, start_obexd: bool) -> zbus::Result<()> {
    let system = Connection::system().await.ok();
    let shell = Shell { session: session.clone(), system: system.clone(), ui: ui.clone(), center: center.clone() };
    let _ = SHELL.set(shell.clone());
    session.object_server().at(SEND_PATH, SendTo).await?;
    if !receive {
        return Ok(());
    }
    let ask: Ask = {
        let ui = ui.clone();
        Arc::new(move |offer: Offer| {
            let ui = ui.clone();
            Box::pin(async move {
                let _turn = prompt::turn().await;
                let _open = PromptOpen(ui.clone());
                matches!(prompt::ask(&ui, offer_prompt(&offer)).await, Answer::Continue { .. })
            })
        })
    };
    let tell: Tell = Arc::new(move |received: Received| match received {
        Received::Saved { offer, path } => {
            let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            shell.notice(0, "File received", &format!("“{file}” from {} is in Downloads.", offer.device));
        }
        Received::Failed { offer } => {
            shell.notice(0, "File not received", &format!("“{}” from {} was not received.", offer.name, offer.device));
        }
    });
    let agent = ObexAgent::new(session.clone(), system, ask, tell, downloads_dir());
    serve_agent(session, agent, start_obexd).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\x\\notes.txt"), "notes.txt");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("dir/"), "file");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("helix-obex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_in(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("a (2).txt"), "").unwrap();
        assert_eq!(unique_in(&dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::write(dir.join("README"), "").unwrap();
        assert_eq!(unique_in(&dir, "README"), dir.join("README (2)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn texts() {
        assert_eq!(size_text(1), "1 byte");
        assert_eq!(size_text(999), "999 bytes");
        assert_eq!(size_text(1_500), "1.5 kB");
        assert_eq!(size_text(2_340_000), "2.3 MB");
        assert_eq!((percent(5, 0), percent(50, 200), percent(300, 200)), (0, 25, 100));
        assert_eq!(file_of("file:///tmp/a%20b.txt"), Some(PathBuf::from("/tmp/a b.txt")));
        assert_eq!(file_of("/tmp/x"), Some(PathBuf::from("/tmp/x")));
        assert_eq!(file_of("relative"), None);
        let offer = Offer { name: "a.jpg".into(), size: 2_000, device: "Phone".into() };
        assert_eq!(offer_prompt(&offer).message, "Phone wants to send you “a.jpg” (2.0 kB).");
        let progress = Sending::File { index: 1, count: 3, name: "b.png".into(), percent: 40 };
        assert_eq!(sending_text("Phone", &progress), ("Sending files to Phone".into(), "b.png (file 2 of 3): 40%".into()));
    }

    #[test]
    fn finishing() {
        let dir = std::env::temp_dir().join(format!("helix-obex-finish-{}", std::process::id()));
        let (cache, downloads) = (dir.join("cache"), dir.join("Downloads"));
        std::fs::create_dir_all(&cache).unwrap();
        let offer = Offer { name: "a.txt".into(), size: 3, device: "Phone".into() };
        std::fs::write(cache.join("a.txt"), "abc").unwrap();
        let got = finish(Ended::Complete, &cache.join("a.txt"), &downloads, "a.txt", offer.clone());
        assert_eq!(got, Received::Saved { offer: offer.clone(), path: downloads.join("a.txt") });
        assert_eq!(std::fs::read_to_string(downloads.join("a.txt")).unwrap(), "abc");
        std::fs::write(cache.join("a.txt"), "partial").unwrap();
        assert_eq!(finish(Ended::Error, &cache.join("a.txt"), &downloads, "a.txt", offer.clone()), Received::Failed { offer });
        assert!(!cache.join("a.txt").exists(), "partial file removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
