//! Removable media (helix-desktop-spec
//! `contracts/gsettings/removable-media.md`): automounting, opening new
//! media, and ejecting them from their notification. GIO's volume monitor
//! runs on a GLib loop of its own thread.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use gio::prelude::*;

use crate::notifications::{open_uri, Center};
use crate::prompt::{self, Answer, Prompt};
use crate::UiEvent;

pub const SCHEMA: &str = "org.gnome.desktop.media-handling";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub automount: bool,
    pub automount_open: bool,
    pub autorun_never: bool,
    pub ignore: Vec<String>,
    pub open_folder: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { automount: true, automount_open: true, autorun_never: false, ignore: Vec::new(), open_folder: Vec::new() }
    }
}

pub fn read() -> Settings {
    if !helix_settings::has_schema(SCHEMA) {
        return Settings::default();
    }
    let s = gio::Settings::new(SCHEMA);
    let strv = |key: &str| s.strv(key).iter().map(|v| v.to_string()).collect();
    Settings {
        automount: s.boolean("automount"),
        automount_open: s.boolean("automount-open"),
        autorun_never: s.boolean("autorun-never"),
        ignore: strv("autorun-x-content-ignore"),
        open_folder: strv("autorun-x-content-open-folder"),
    }
}

/// What to do about a medium that was just mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    Nothing,
    Open,
    Notify,
}

/// For a mount with content `types`; `ours`: the shell mounted it.
pub fn on_mounted(s: &Settings, types: &[String], ours: bool) -> Arrival {
    let any_in = |list: &[String]| types.iter().any(|t| list.contains(t));
    if any_in(&s.ignore) || s.autorun_never {
        Arrival::Nothing
    } else if any_in(&s.open_folder) || (ours && s.automount_open) {
        Arrival::Open
    } else {
        Arrival::Notify
    }
}

/// A mounted drive the dock shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub name: String,
    /// Its root, a URI.
    pub root: String,
    /// Icon names, best first.
    pub icons: Vec<String>,
    /// It can be ejected (else only unmounted).
    pub ejectable: bool,
    /// A network share.
    pub network: bool,
    /// Mounted (else a volume that can be mounted: `root` is
    /// `volume:<its identifier>`).
    pub mounted: bool,
}

/// The `root` of a volume that is not mounted.
const VOLUME: &str = "volume:";

fn volume_key(v: &gio::Volume) -> String {
    let id = v.uuid().or_else(|| v.identifier("unix-device")).map(|s| s.to_string()).unwrap_or_else(|| v.name().to_string());
    format!("{VOLUME}{id}")
}

fn is_network_volume(v: &gio::Volume) -> bool {
    v.identifier("class").is_some_and(|c| c == "network")
}

/// Whether the dock shows `m`: network shares only with `network`,
/// volumes that are not mounted only without `only_mounted`.
pub fn shown(m: &MountInfo, network: bool, only_mounted: bool) -> bool {
    (network || !m.network) && (m.mounted || !only_mounted)
}

/// The mounts to show: those of volumes (removable drives, discs, phones,
/// network shares) and network shares without one, that can be unmounted,
/// not shadowed; then the volumes that are not mounted but can be.
fn mount_infos(monitor: &gio::VolumeMonitor) -> Vec<MountInfo> {
    let icons = |icon: gio::Icon| icon.downcast::<gio::ThemedIcon>().map(|t| t.names().iter().map(|n| n.to_string()).collect()).unwrap_or_default();
    let mut out: Vec<MountInfo> = monitor
        .mounts()
        .into_iter()
        .filter(|m| !m.is_shadowed() && (m.can_unmount() || m.can_eject()))
        .filter_map(|m| {
            let root = m.root().uri().to_string();
            let network = match m.volume() {
                Some(v) => is_network_volume(&v),
                // Only network shares are mounted without a volume.
                None if !root.starts_with("file://") => true,
                None => return None,
            };
            Some(MountInfo { name: m.name().to_string(), root, icons: icons(m.icon()), ejectable: m.can_eject(), network, mounted: true })
        })
        .collect();
    out.extend(monitor.volumes().into_iter().filter(|v| v.get_mount().is_none() && v.can_mount()).map(|v| MountInfo {
        name: v.name().to_string(),
        root: volume_key(&v),
        icons: icons(v.icon()),
        ejectable: v.can_eject(),
        network: is_network_volume(&v),
        mounted: false,
    }));
    out
}

/// Tell the UI the mounts and whether the trash holds anything.
fn publish(state: &State) {
    let Some(asker) = ASKER.get() else { return };
    if let Some(monitor) = state.monitor.as_ref() {
        let _ = asker.ui.send(UiEvent::Mounts(mount_infos(monitor)));
    }
}

fn publish_trash() {
    let Some(asker) = ASKER.get() else { return };
    let trash = gio::File::for_uri("trash:///");
    let full = trash
        .query_info("trash::item-count", gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>)
        .map(|i| i.attribute_uint32("trash::item-count") > 0)
        .unwrap_or(false);
    let _ = asker.ui.send(UiEvent::Trash(full));
}

/// Open the trash, or empty it (after the user agreed).
pub fn trash_request(empty: bool) {
    if empty {
        // gio empties the trash of every mount, as the file manager does.
        if let Err(err) = std::process::Command::new("gio").args(["trash", "--empty"]).spawn() {
            tracing::warn!(%err, "cannot empty the trash");
        }
    } else {
        open_uri("trash:///");
    }
}

/// What a notification button asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Open,
    Eject,
}

static CONTEXT: OnceLock<gio::glib::MainContext> = OnceLock::new();

/// A notification's button for the mount at `root` (a URI).
pub fn request(root: String, what: Request) {
    match what {
        // A volume that is not mounted: mount it (it then opens).
        Request::Open if root.starts_with(VOLUME) => {
            if let Some(context) = CONTEXT.get() {
                context.invoke(move || {
                    let volume = STATE.with(|s| s.borrow().monitor.as_ref().and_then(|m| m.volumes().into_iter().find(|v| volume_key(v) == root)));
                    if let Some(volume) = volume {
                        mount_volume(&volume, false);
                    }
                });
            }
        }
        Request::Open => open_uri(&root),
        Request::Eject => {
            if let Some(context) = CONTEXT.get() {
                context.invoke(move || STATE.with(|s| eject(&s.borrow(), &root)));
            }
        }
    }
}

// ----- Questions (mount operations) ---------------------------------------

/// Where questions go: the UI, and the runtime that waits for answers.
struct Asker {
    ui: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    runtime: tokio::runtime::Handle,
}

static ASKER: OnceLock<Asker> = OnceLock::new();

/// An answer for a mount operation.
enum Reply {
    Password { username: Option<String>, domain: Option<String>, password: Option<String>, save: bool, extras: crate::mount_handler::Extras },
    Choice(i32),
    Abort,
}

thread_local! {
    static OPS: RefCell<HashMap<u64, gio::MountOperation>> = RefCell::new(HashMap::new());
    static NEXT_OP: Cell<u64> = const { Cell::new(1) };
    /// Operations whose dialog is open.
    static ASKING: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Unmount progress notifications, by operation.
    static WRITING: RefCell<HashMap<u64, u32>> = RefCell::new(HashMap::new());
}

/// "a, b, c" for at most five programs, then "and N more".
pub fn program_list(names: &[String]) -> String {
    let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    match names.len() {
        n if n > 5 => format!("{shown} and {} more", n - 5),
        _ => shown,
    }
}

fn answer_op(id: u64, reply: Reply) {
    ASKING.with(|a| a.borrow_mut().remove(&id));
    let Some(op) = OPS.with(|o| o.borrow().get(&id).cloned()) else { return };
    match reply {
        Reply::Password { username, domain, password, save, extras } => {
            op.set_anonymous(extras.anonymous);
            op.set_is_tcrypt_hidden_volume(extras.hidden_volume);
            op.set_is_tcrypt_system_volume(extras.system_volume);
            if let Some(pim) = extras.pim {
                op.set_pim(pim);
            }
            if let Some(username) = username {
                op.set_username(Some(&username));
            }
            if let Some(domain) = domain {
                op.set_domain(Some(&domain));
            }
            if let Some(password) = password {
                op.set_password(Some(&password));
            }
            op.set_password_save(if save { gio::PasswordSave::Permanently } else { gio::PasswordSave::Never });
            op.reply(gio::MountOperationResult::Handled);
        }
        Reply::Choice(i) => {
            op.set_choice(i);
            op.reply(gio::MountOperationResult::Handled);
        }
        Reply::Abort => op.reply(gio::MountOperationResult::Aborted),
    }
}

/// Ask `prompt` from the UI; `answer` turns the answer into a reply,
/// which goes back to the operation on the GLib thread.
fn ask(id: u64, prompt: Prompt, answer: impl FnOnce(Answer) -> Reply + Send + 'static) {
    let Some(asker) = ASKER.get() else {
        answer_op(id, Reply::Abort);
        return;
    };
    if !ASKING.with(|a| a.borrow_mut().insert(id)) {
        return;
    }
    let ui = asker.ui.clone();
    asker.runtime.spawn(async move {
        let _turn = prompt::turn().await;
        let reply = answer(prompt::ask(&ui, prompt).await);
        prompt::done(&ui);
        if let Some(context) = CONTEXT.get() {
            context.invoke(move || answer_op(id, reply));
        }
    });
}

pub(crate) fn process_name(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_owned()).unwrap_or_else(|_| format!("process {pid}"))
}

/// A choice question: `name` titles it, `description` explains it.
fn ask_choice(id: u64, name: &str, message: &str, description: String, choices: Vec<String>) {
    let prompt = Prompt { title: name.to_owned(), message: message.to_owned(), description, ..Prompt::default() };
    let Some((prompt, map)) = crate::mount_handler::choice_prompt(prompt, &choices) else {
        answer_op(id, Reply::Abort);
        return;
    };
    ask(id, prompt, move |answer| map(answer).map_or(Reply::Abort, Reply::Choice));
}

/// A mount operation that asks the user, for the medium `name`.
fn mount_operation(name: &str) -> (gio::MountOperation, u64) {
    let op = gio::MountOperation::new();
    let id = NEXT_OP.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    OPS.with(|o| o.borrow_mut().insert(id, op.clone()));
    let title = name.to_owned();
    // GIO's own handlers would answer "unhandled" after ours: stop them.
    op.connect_ask_password(move |op, message, user, domain, flags| {
        op.stop_signal_emission_by_name("ask-password");
        tracing::info!(?flags, "mount asks for a password");
        let bits = flags.bits();
        let Some(mut prompt) = crate::mount_handler::password_prompt(message, user, domain, bits) else {
            answer_op(id, Reply::Abort);
            return;
        };
        let has_user = prompt.entry.is_some();
        crate::mount_handler::add_extras(&mut prompt, bits);
        prompt.title = format!("Unlock {title}");
        prompt.message = message.to_owned();
        prompt.continue_label = "Unlock".into();
        let (user, domain) = (user.to_owned(), domain.to_owned());
        ask(id, prompt, move |answer| match answer {
            Answer::Continue { entry, password, choice, checks, .. } => {
                if flags.contains(gio::AskPasswordFlags::SAVING_SUPPORTED) {
                    crate::mount_handler::remember_chosen(choice);
                }
                let extras = crate::mount_handler::extras_of(bits, has_user, entry.as_deref(), &checks);
                // The PIM field is not a user name.
                let named = if has_user { entry.as_deref() } else { None };
                let details = crate::mount_handler::password_details(bits, &user, &domain, named, None, choice);
                let get = |k: &str| details.get(k).and_then(|v| String::try_from(v.clone()).ok());
                Reply::Password {
                    username: get("username"),
                    domain: get("domain"),
                    password: password.map(|p| String::from_utf8_lossy(p.bytes()).into_owned()),
                    save: choice && flags.contains(gio::AskPasswordFlags::SAVING_SUPPORTED),
                    extras,
                }
            }
            Answer::Cancel => Reply::Abort,
        });
    });
    let title = name.to_owned();
    op.connect_local("ask-question", false, move |values| {
        if let Some(op) = values.first().and_then(|v| v.get::<gio::MountOperation>().ok()) {
            op.stop_signal_emission_by_name("ask-question");
        }
        let message: String = values.get(1).and_then(|v| v.get().ok()).unwrap_or_default();
        let choices: Vec<String> = values.get(2).and_then(|v| v.get::<Vec<String>>().ok()).unwrap_or_default();
        ask_choice(id, &title, &message, String::new(), choices);
        None
    });
    let title = name.to_owned();
    op.connect_local("show-processes", false, move |values| {
        if let Some(op) = values.first().and_then(|v| v.get::<gio::MountOperation>().ok()) {
            op.stop_signal_emission_by_name("show-processes");
        }
        let message: String = values.get(1).and_then(|v| v.get().ok()).unwrap_or_default();
        let pids = values.get(2).map(pids_of).unwrap_or_default();
        let choices: Vec<String> = values.get(3).and_then(|v| v.get::<Vec<String>>().ok()).unwrap_or_default();
        let names: Vec<String> = pids.iter().map(|p| process_name(*p)).collect();
        let description = if names.is_empty() { String::new() } else { format!("In use by: {}", program_list(&names)) };
        ask_choice(id, &title, &message, description, choices);
        None
    });
    let title = name.to_owned();
    op.connect_show_unmount_progress(move |_, _message, _time_left, bytes_left| {
        let center = STATE.with(|s| s.borrow().center.clone());
        let Some(center) = center else { return };
        let shown = WRITING.with(|w| w.borrow().get(&id).copied());
        match (shown, bytes_left > 0) {
            (None, true) => {
                let n = center.notice("Removable Media", &format!("Writing to {title}"), "Do not unplug until finished.", "media-removable-symbolic", false);
                WRITING.with(|w| w.borrow_mut().insert(id, n));
            }
            (Some(n), false) => {
                center.withdraw(n);
                WRITING.with(|w| w.borrow_mut().remove(&id));
            }
            _ => {}
        }
    });
    (op, id)
}

/// The PIDs of a `show-processes` signal (a `GArray` of `GPid`).
fn pids_of(value: &gio::glib::Value) -> Vec<i32> {
    use gio::glib::translate::ToGlibPtr;
    // SAFETY: the signal's second argument is a boxed GArray of gint
    // (GPid), alive for the length of the emission.
    unsafe {
        let array = gio::glib::gobject_ffi::g_value_get_boxed(value.to_glib_none().0) as *const gio::glib::ffi::GArray;
        if array.is_null() || (*array).data.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts((*array).data as *const i32, (*array).len as usize).to_vec()
    }
}

/// The operation is over: forget it.
fn finish_op(id: u64) {
    OPS.with(|o| o.borrow_mut().remove(&id));
    ASKING.with(|a| a.borrow_mut().remove(&id));
    let left = WRITING.with(|w| w.borrow_mut().remove(&id));
    if let (Some(n), Some(center)) = (left, STATE.with(|s| s.borrow().center.clone())) {
        center.withdraw(n);
    }
}

#[derive(Default)]
struct State {
    center: Option<Center>,
    monitor: Option<gio::VolumeMonitor>,
    /// Volumes the shell is mounting: true when it was at login (quietly).
    mounting: Vec<(gio::Volume, bool)>,
    /// Volumes waiting for the session to be unlocked.
    waiting: Vec<gio::Volume>,
    /// Notifications by mount root.
    notices: HashMap<String, u32>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn can_automount(v: &gio::Volume) -> bool {
    v.should_automount() && v.can_mount() && v.get_mount().is_none()
}

fn mount_volume(volume: &gio::Volume, quiet: bool) {
    STATE.with(|s| s.borrow_mut().mounting.push((volume.clone(), quiet)));
    let name = volume.name().to_string();
    let v = volume.clone();
    let (op, op_id) = mount_operation(&name);
    volume.mount(gio::MountMountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |result| {
        finish_op(op_id);
        if let Err(err) = result {
            tracing::info!(%err, name, "cannot mount");
            STATE.with(|s| s.borrow_mut().mounting.retain(|(m, _)| *m != v));
        } else {
            tracing::info!(name, "mounted");
        }
    });
}

fn volume_added(volume: &gio::Volume) {
    tracing::debug!(name = %volume.name(), automount = volume.should_automount(), "volume added");
    if !read().automount || !can_automount(volume) {
        return;
    }
    let locked = STATE.with(|s| s.borrow().center.as_ref().is_some_and(Center::locked));
    if locked {
        STATE.with(|s| s.borrow_mut().waiting.push(volume.clone()));
    } else {
        mount_volume(volume, false);
    }
}

fn mount_added(mount: &gio::Mount) {
    tracing::debug!(name = %mount.name(), "mount added");
    STATE.with(|s| publish(&s.borrow()));
    let Some(volume) = mount.volume() else { return };
    if mount.is_shadowed() {
        return;
    }
    let ours = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let found = s.mounting.iter().position(|(v, _)| *v == volume);
        found.map(|i| s.mounting.remove(i).1)
    });
    // Mounted quietly at login.
    if ours == Some(true) {
        return;
    }
    let m = mount.clone();
    mount.guess_content_type(false, None::<&gio::Cancellable>, move |types| {
        let types: Vec<String> = types.unwrap_or_default().iter().map(|t| t.to_string()).collect();
        let root = m.root().uri().to_string();
        let arrival = on_mounted(&read(), &types, ours.is_some());
        tracing::info!(root, ?types, ?arrival, "medium mounted");
        match arrival {
            Arrival::Nothing => {}
            Arrival::Open => open_uri(&root),
            Arrival::Notify => STATE.with(|s| {
                let mut s = s.borrow_mut();
                if let Some(center) = s.center.clone() {
                    let id = center.media_notice(&m.name(), "Removable medium connected", Some(root.clone()));
                    s.notices.insert(root, id);
                }
            }),
        }
    });
}

fn mount_removed(mount: &gio::Mount) {
    let root = mount.root().uri().to_string();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let (Some(id), Some(center)) = (s.notices.remove(&root), s.center.clone()) {
            center.withdraw(id);
        }
    });
    STATE.with(|s| publish(&s.borrow()));
}

fn eject(state: &State, root: &str) {
    let (Some(monitor), Some(center)) = (state.monitor.as_ref(), state.center.clone()) else { return };
    let Some(mount) = monitor.mounts().into_iter().find(|m| m.root().uri() == root) else { return };
    let name = mount.name().to_string();
    let (op, op_id) = mount_operation(&name);
    let done = move |result: Result<(), gio::glib::Error>| {
        finish_op(op_id);
        match result {
            Ok(()) => {
                center.media_notice(&format!("{name} can be removed"), "", None);
            }
            Err(err) => {
                tracing::info!(%err, name, "cannot eject");
                center.media_notice(&format!("Cannot eject {name}"), err.message(), None);
            }
        }
    };
    let (flags, op, none) = (gio::MountUnmountFlags::NONE, Some(&op), None::<&gio::Cancellable>);
    if mount.can_eject() {
        mount.eject_with_operation(flags, op, none, done);
    } else {
        mount.unmount_with_operation(flags, op, none, done);
    }
}

/// What the eject key ejects, of drives given as (unix device, can
/// eject): the first ejectable optical drive (`/dev/sr*`), else the first
/// ejectable drive.
pub fn pick_eject(drives: &[(String, bool)]) -> Option<usize> {
    let ejectable = || drives.iter().enumerate().filter(|(_, d)| d.1);
    ejectable().find(|(_, d)| d.0.starts_with("/dev/sr")).or_else(|| ejectable().next()).map(|(i, _)| i)
}

/// The eject key: eject a drive (see [`pick_eject`]).
pub fn eject_drive() {
    let Some(context) = CONTEXT.get() else { return };
    context.invoke(|| {
        STATE.with(|s| {
            let state = s.borrow();
            let Some(monitor) = state.monitor.as_ref() else { return };
            let drives = monitor.connected_drives();
            let described: Vec<(String, bool)> = drives
                .iter()
                .map(|d| (d.identifier("unix-device").map(|i| i.to_string()).unwrap_or_default(), d.can_eject()))
                .collect();
            let Some(drive) = pick_eject(&described).and_then(|i| drives.get(i).cloned()) else {
                tracing::info!("eject key: no drive to eject");
                return;
            };
            let name = drive.name().to_string();
            tracing::info!(name, "eject key");
            let center = state.center.clone();
            let (op, op_id) = mount_operation(&name);
            drive.eject_with_operation(gio::MountUnmountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |result| {
                finish_op(op_id);
                if let (Err(err), Some(center)) = (result, center) {
                    center.media_notice(&format!("Cannot eject {name}"), err.message(), None);
                }
            });
        });
    });
}

/// Watch media for the rest of the process; questions go to `ui`,
/// waited for on `runtime`.
pub fn run(center: Center, ui: tokio::sync::mpsc::UnboundedSender<UiEvent>, runtime: tokio::runtime::Handle) {
    let _ = ASKER.set(Asker { ui, runtime });
    let spawned = std::thread::Builder::new().name("helix-media".into()).spawn(move || {
        // gvfs's volume monitors deliver their D-Bus signals on the global
        // default context, which nothing else in the shell runs.
        let context = gio::glib::MainContext::default();
        let Ok(_owner) = context.acquire() else {
            tracing::warn!("cannot watch removable media: the default main context is taken");
            return;
        };
        let main_loop = gio::glib::MainLoop::new(Some(&context), false);
        let _ = CONTEXT.set(context.clone());
        let _ = context.with_thread_default(|| {
            let monitor = gio::VolumeMonitor::get();
            tracing::debug!(monitor = %monitor.type_(), volumes = monitor.volumes().len(), "watching media");
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.center = Some(center.clone());
                s.monitor = Some(monitor.clone());
            });
            if read().automount {
                for volume in monitor.volumes().iter().filter(|v| can_automount(v)) {
                    mount_volume(volume, true);
                }
            }
            monitor.connect_volume_added(|_, v| {
                STATE.with(|s| publish(&s.borrow()));
                volume_added(v);
            });
            monitor.connect_volume_removed(|_, v| STATE.with(|s| {
                s.borrow_mut().waiting.retain(|w| w != v);
                publish(&s.borrow());
            }));
            monitor.connect_volume_changed(|_, _| STATE.with(|s| publish(&s.borrow())));
            monitor.connect_mount_added(|_, m| mount_added(m));
            monitor.connect_mount_removed(|_, m| mount_removed(m));
            monitor.connect_mount_changed(|_, _| STATE.with(|s| publish(&s.borrow())));
            STATE.with(|s| publish(&s.borrow()));
            // The trash's fill state, for the dock.
            let trash = gio::File::for_uri("trash:///");
            match trash.monitor_directory(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) {
                Ok(watch) => {
                    watch.connect_changed(|_, _, _, _| publish_trash());
                    // Kept for the life of the loop.
                    std::mem::forget(watch);
                }
                Err(err) => tracing::info!(%err, "cannot watch the trash"),
            }
            publish_trash();
            // Volumes that arrived while locked.
            gio::glib::timeout_add_local(Duration::from_secs(1), || {
                let ready = STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    let unlocked = s.center.as_ref().is_some_and(|c| !c.locked());
                    if unlocked { std::mem::take(&mut s.waiting) } else { Vec::new() }
                });
                for v in ready.iter().filter(|v| can_automount(v)) {
                    mount_volume(v, false);
                }
                gio::glib::ControlFlow::Continue
            });
            main_loop.run();
        });
    });
    if let Err(err) = spawned {
        tracing::warn!(%err, "cannot watch removable media");
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn dock_mount_filter() {
        let m = |network, mounted| MountInfo { name: String::new(), root: String::new(), icons: Vec::new(), ejectable: false, network, mounted };
        assert!(shown(&m(false, true), false, true));
        assert!(!shown(&m(true, true), false, true), "network shares only when asked");
        assert!(shown(&m(true, true), true, true));
        assert!(!shown(&m(false, false), false, true), "unmounted volumes only when asked");
        assert!(shown(&m(false, false), false, false));
    }

    use super::*;

    #[test]
    fn eject_choice() {
        let d = |dev: &str, can: bool| (dev.to_owned(), can);
        assert_eq!(pick_eject(&[d("/dev/sda", true), d("/dev/sr0", true)]), Some(1), "the optical drive first");
        assert_eq!(pick_eject(&[d("/dev/sr0", false), d("/dev/sdb", true)]), Some(1));
        assert_eq!(pick_eject(&[d("/dev/sda", false)]), None);
    }

    #[test]
    fn questions() {
        let names = |n: usize| (1..=n).map(|i| format!("p{i}")).collect::<Vec<_>>();
        assert_eq!(program_list(&names(2)), "p1, p2");
        assert_eq!(program_list(&names(7)), "p1, p2, p3, p4, p5 and 2 more");
    }

    #[test]
    fn arrivals() {
        let s = Settings::default();
        let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(on_mounted(&s, &[], true), Arrival::Open);
        assert_eq!(on_mounted(&s, &[], false), Arrival::Notify, "mounted by someone else");
        let no_open = Settings { automount_open: false, ..s.clone() };
        assert_eq!(on_mounted(&no_open, &[], true), Arrival::Notify);
        let cards = Settings { open_folder: t(&["x-content/image-dcf"]), ..no_open.clone() };
        assert_eq!(on_mounted(&cards, &t(&["x-content/image-dcf"]), false), Arrival::Open);
        let ignore = Settings { ignore: t(&["x-content/image-dcf"]), ..cards.clone() };
        assert_eq!(on_mounted(&ignore, &t(&["x-content/image-dcf"]), true), Arrival::Nothing, "ignore wins");
        let never = Settings { autorun_never: true, ..s };
        assert_eq!(on_mounted(&never, &[], true), Arrival::Nothing);
    }
}
