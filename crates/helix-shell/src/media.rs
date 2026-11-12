//! Removable media (helix-desktop-spec
//! `contracts/gsettings/removable-media.md`): automounting, opening new
//! media, and ejecting them from their notification. GIO's volume monitor
//! runs on a GLib loop of its own thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use gio::prelude::*;

use crate::notifications::{open_uri, Center};

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
        Request::Open => open_uri(&root),
        Request::Eject => {
            if let Some(context) = CONTEXT.get() {
                context.invoke(move || STATE.with(|s| eject(&s.borrow(), &root)));
            }
        }
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
    volume.mount(gio::MountMountFlags::NONE, None::<&gio::MountOperation>, None::<&gio::Cancellable>, move |result| {
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
}

fn eject(state: &State, root: &str) {
    let (Some(monitor), Some(center)) = (state.monitor.as_ref(), state.center.clone()) else { return };
    let Some(mount) = monitor.mounts().into_iter().find(|m| m.root().uri() == root) else { return };
    let name = mount.name().to_string();
    let done = move |result: Result<(), gio::glib::Error>| match result {
        Ok(()) => {
            center.media_notice(&format!("{name} can be removed"), "", None);
        }
        Err(err) => {
            tracing::info!(%err, name, "cannot eject");
            center.media_notice(&format!("Cannot eject {name}"), err.message(), None);
        }
    };
    let (flags, op, none) = (gio::MountUnmountFlags::NONE, None::<&gio::MountOperation>, None::<&gio::Cancellable>);
    if mount.can_eject() {
        mount.eject_with_operation(flags, op, none, done);
    } else {
        mount.unmount_with_operation(flags, op, none, done);
    }
}

/// Watch media for the rest of the process.
pub fn run(center: Center) {
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
            monitor.connect_volume_added(|_, v| volume_added(v));
            monitor.connect_volume_removed(|_, v| STATE.with(|s| s.borrow_mut().waiting.retain(|w| w != v)));
            monitor.connect_mount_added(|_, m| mount_added(m));
            monitor.connect_mount_removed(|_, m| mount_removed(m));
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
    use super::*;

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
