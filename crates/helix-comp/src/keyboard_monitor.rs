//! `org.freedesktop.a11y.KeyboardMonitor`: key events and key grabs for
//! screen readers.
//!
//! The compositor asks [`Monitor::key`] about every key (on its own
//! thread, through [`Shared`]) and gets back whether the key is grabbed and
//! which clients to tell; the D-Bus side changes the clients' watches and
//! grabs and sends the `KeyEvent` signals ([`Report`]).
//! Contract: helix-desktop-spec `contracts/dbus/org.freedesktop.a11y.KeyboardMonitor.md`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{interface, Connection};

pub const BUS_NAME: &str = "org.freedesktop.a11y.Manager";
pub const OBJECT_PATH: &str = "/org/freedesktop/a11y/Manager";
/// Only the owner of this name (the screen reader) may use the interface.
pub const ALLOWED_CALLER: &str = "org.gnome.Orca.KeyboardMonitor";

/// XKB modifier bits left out when comparing keystroke masks: Lock and
/// Mod2 (Num Lock).
const IGNORED_MODS: u32 = 2 | 16;

/// Keysyms of the virtual terminal switching keys, which a keyboard grab
/// does not take.
const SWITCH_VT: std::ops::RangeInclusive<u32> = 0x1008_FE01..=0x1008_FE0C;

/// Keysyms of keys that change a lock or latch (Caps Lock, Num Lock,
/// layout switching): grabbed, they must not reach the keyboard state at
/// all. Plain modifiers (Shift, ISO_Level3_Shift…) are not among them.
pub fn is_lock_keysym(keysym: u32) -> bool {
    matches!(
        keysym,
        0xffe5 // Caps_Lock
            | 0xffe6 // Shift_Lock
            | 0xff7f // Num_Lock
            | 0xff14 // Scroll_Lock
            | 0xfe01 // ISO_Lock
            | 0xfe02 // ISO_Level2_Latch
            | 0xfe04..=0xfe0f // ISO_Level3_Latch … ISO_Last_Group_Lock
            | 0xfe12 // ISO_Level5_Latch
            | 0xfe13 // ISO_Level5_Lock
    )
}

/// What one client set up.
#[derive(Debug, Default, Clone, PartialEq)]
struct Client {
    watch: bool,
    grab_all: bool,
    /// Keysyms of keys used as the client's own modifier.
    modifiers: Vec<u32>,
    /// (keysym, XKB modifier mask).
    keystrokes: Vec<(u32, u32)>,
}

impl Client {
    fn is_empty(&self) -> bool {
        !self.watch && !self.grab_all && self.modifiers.is_empty() && self.keystrokes.is_empty()
    }
}

/// A key press whose release must go the same way.
#[derive(Debug, Clone)]
struct Held {
    /// Clients the press was grabbed for.
    to: Vec<String>,
    /// Pressed as a client's modifier: (keysym, time of press in ms, no
    /// other key pressed since).
    modifier: Option<(u32, u64, bool)>,
}

/// A key as the compositor sees it, before it changes the keyboard state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// XKB keycode (kernel code + 8).
    pub keycode: u32,
    pub pressed: bool,
    /// The keysym at the current shift level and layout.
    pub keysym: u32,
    /// The keysym of the key's first shift level in the current layout.
    pub base_keysym: u32,
    /// The character it types, 0 for none.
    pub unichar: u32,
    /// The XKB modifier mask in effect (effective modifiers).
    pub state: u32,
}

/// One `KeyEvent` to send to `to` (unique bus names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub to: Vec<String>,
    pub released: bool,
    pub state: u32,
    pub keysym: u32,
    pub unichar: u32,
    pub keycode: u16,
}

/// What to do with a key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Verdict {
    /// The key is grabbed: report it only.
    pub grabbed: bool,
    /// Whom to tell (`None`: nobody).
    pub report: Option<Report>,
}

/// The clients and the keys held down for them.
#[derive(Debug, Default)]
pub struct Monitor {
    clients: BTreeMap<String, Client>,
    /// Grabbed presses by keycode.
    held: HashMap<u32, Held>,
    /// The last client modifier pressed alone: (keysym, time of its press).
    tap: Option<(u32, u64)>,
}

pub type Shared = Arc<Mutex<Monitor>>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Monitor> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

impl Monitor {
    /// Nothing to do for any key (no client, no grabbed key down).
    pub fn is_idle(&self) -> bool {
        self.clients.is_empty() && self.held.is_empty()
    }

    fn client(&mut self, name: &str) -> &mut Client {
        self.clients.entry(name.to_owned()).or_default()
    }

    fn tidy(&mut self, name: &str) {
        if self.clients.get(name).is_some_and(Client::is_empty) {
            self.clients.remove(name);
        }
    }

    pub fn watch(&mut self, name: &str, on: bool) {
        self.client(name).watch = on;
        self.tidy(name);
    }

    pub fn grab_keyboard(&mut self, name: &str, on: bool) {
        self.client(name).grab_all = on;
        self.tidy(name);
    }

    pub fn set_key_grabs(&mut self, name: &str, modifiers: Vec<u32>, keystrokes: Vec<(u32, u32)>) {
        let client = self.client(name);
        client.modifiers = modifiers;
        client.keystrokes = keystrokes;
        self.tidy(name);
    }

    /// The client left the bus: everything it set up goes.
    pub fn remove(&mut self, name: &str) {
        self.clients.remove(name);
        for held in self.held.values_mut() {
            held.to.retain(|n| n != name);
        }
    }

    fn watchers(&self) -> impl Iterator<Item = &String> {
        self.clients.iter().filter(|(_, c)| c.watch).map(|(n, _)| n)
    }

    /// Decide about a key. `now` is in ms; `repeat_delay` (ms) bounds a
    /// double press of a client modifier; `locked`: the lock screen is up.
    pub fn key(&mut self, key: &Key, now: u64, repeat_delay: u64, locked: bool) -> Verdict {
        if !key.pressed {
            return self.release(key, locked);
        }
        if locked {
            return Verdict::default();
        }
        // Another key while a client modifier is down: the modifier was not
        // pressed alone.
        for held in self.held.values_mut() {
            if let Some((_, _, alone)) = &mut held.modifier {
                *alone = false;
            }
        }
        let (to, modifier) = self.grabbers(key, now, repeat_delay);
        if !to.is_empty() {
            self.held.insert(key.keycode, Held { to: to.clone(), modifier: modifier.map(|sym| (sym, now, true)) });
        }
        self.verdict(key, !to.is_empty(), to)
    }

    /// Who grabs a pressed key, and whether as their modifier.
    fn grabbers(&mut self, key: &Key, now: u64, repeat_delay: u64) -> (Vec<String>, Option<u32>) {
        let grabbing: Vec<String> = self.clients.iter().filter(|(_, c)| c.grab_all).map(|(n, _)| n.clone()).collect();
        if !grabbing.is_empty() {
            self.tap = None;
            if SWITCH_VT.contains(&key.keysym) {
                return (Vec::new(), None);
            }
            return (grabbing, None);
        }
        // Keys pressed while a client modifier is held (by a client still
        // on the bus).
        let mut to: Vec<String> = Vec::new();
        for held in self.held.values() {
            if held.modifier.is_some() {
                for name in &held.to {
                    if !to.contains(name) {
                        to.push(name.clone());
                    }
                }
            }
        }
        if !to.is_empty() {
            self.tap = None;
            return (to, None);
        }
        let owners: Vec<String> =
            self.clients.iter().filter(|(_, c)| c.modifiers.contains(&key.keysym)).map(|(n, _)| n.clone()).collect();
        if !owners.is_empty() {
            let double = self.tap.take().is_some_and(|(sym, at)| sym == key.keysym && now.saturating_sub(at) <= repeat_delay);
            if double {
                return (Vec::new(), None);
            }
            return (owners, Some(key.keysym));
        }
        self.tap = None;
        let mods = key.state & !IGNORED_MODS;
        let to = self
            .clients
            .iter()
            .filter(|(_, c)| {
                c.keystrokes.iter().any(|&(sym, mask)| (sym == key.keysym || sym == key.base_keysym) && mask & !IGNORED_MODS == mods)
            })
            .map(|(n, _)| n.clone())
            .collect();
        (to, None)
    }

    fn release(&mut self, key: &Key, locked: bool) -> Verdict {
        let Some(held) = self.held.remove(&key.keycode) else {
            if locked {
                return Verdict::default();
            }
            return self.verdict(key, false, Vec::new());
        };
        if let Some((sym, at, true)) = held.modifier {
            self.tap = Some((sym, at));
        }
        let to = held.to;
        if locked {
            let report = (!to.is_empty()).then(|| report(key, to));
            return Verdict { grabbed: true, report };
        }
        self.verdict(key, true, to)
    }

    /// `to` plus the watchers.
    fn verdict(&self, key: &Key, grabbed: bool, mut to: Vec<String>) -> Verdict {
        for name in self.watchers() {
            if !to.contains(name) {
                to.push(name.clone());
            }
        }
        Verdict { grabbed, report: (!to.is_empty()).then(|| report(key, to)) }
    }
}

fn report(key: &Key, to: Vec<String>) -> Report {
    Report {
        to,
        released: !key.pressed,
        state: key.state,
        keysym: key.keysym,
        unichar: key.unichar,
        keycode: u16::try_from(key.keycode).unwrap_or(u16::MAX),
    }
}

/// The served object.
pub struct KeyboardMonitor {
    pub shared: Shared,
}

/// The caller, if it owns [`ALLOWED_CALLER`].
async fn caller(conn: &Connection, header: &Header<'_>) -> fdo::Result<String> {
    let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
    let owner = match (DBusProxy::new(conn).await, zbus::names::BusName::try_from(ALLOWED_CALLER)) {
        (Ok(dbus), Ok(name)) => dbus.get_name_owner(name).await.ok(),
        _ => None,
    };
    if !sender.is_empty() && owner.is_some_and(|o| o.as_str() == sender) {
        Ok(sender)
    } else {
        Err(fdo::Error::AccessDenied(format!("only the owner of {ALLOWED_CALLER} may monitor the keyboard")))
    }
}

#[interface(name = "org.freedesktop.a11y.KeyboardMonitor")]
impl KeyboardMonitor {
    async fn grab_keyboard(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let name = caller(conn, &header).await?;
        lock(&self.shared).grab_keyboard(&name, true);
        Ok(())
    }

    async fn ungrab_keyboard(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let name = caller(conn, &header).await?;
        lock(&self.shared).grab_keyboard(&name, false);
        Ok(())
    }

    async fn watch_keyboard(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let name = caller(conn, &header).await?;
        lock(&self.shared).watch(&name, true);
        Ok(())
    }

    async fn unwatch_keyboard(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let name = caller(conn, &header).await?;
        lock(&self.shared).watch(&name, false);
        Ok(())
    }

    async fn set_key_grabs(
        &self,
        modifiers: Vec<u32>,
        keystrokes: Vec<(u32, u32)>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let name = caller(conn, &header).await?;
        lock(&self.shared).set_key_grabs(&name, modifiers, keystrokes);
        Ok(())
    }

    #[zbus(signal)]
    pub async fn key_event(emitter: &SignalEmitter<'_>, released: bool, state: u32, keysym: u32, unichar: u32, keycode: u16) -> zbus::Result<()>;
}

/// Send a key to the clients it concerns, each its own signal.
pub async fn send(conn: &Connection, report: &Report) -> zbus::Result<()> {
    for name in &report.to {
        let Ok(dest) = zbus::names::BusName::try_from(name.as_str()) else { continue };
        let emitter = SignalEmitter::new(conn, OBJECT_PATH)?.set_destination(dest);
        if let Err(err) = KeyboardMonitor::key_event(&emitter, report.released, report.state, report.keysym, report.unichar, report.keycode).await {
            tracing::debug!(%err, name, "cannot send a key event");
        }
    }
    Ok(())
}

/// Drop what clients set up when they leave the bus.
pub async fn follow_owners(conn: &Connection, shared: Shared) -> zbus::Result<tokio::task::JoinHandle<()>> {
    let mut changes = DBusProxy::new(conn).await?.receive_name_owner_changed().await?;
    Ok(tokio::spawn(async move {
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            let name = args.name().as_str();
            if name.starts_with(':') && args.new_owner().is_none() {
                lock(&shared).remove(name);
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSERT: u32 = 0xff63;
    const CAPS: u32 = 0xffe5;
    const SHIFT_L: u32 = 0xffe1;
    const KP_ADD: u32 = 0xffab;
    const DELAY: u64 = 500;

    fn press(keycode: u32, keysym: u32) -> Key {
        Key { keycode, pressed: true, keysym, base_keysym: keysym, unichar: 0, state: 0 }
    }

    fn release(keycode: u32, keysym: u32) -> Key {
        Key { pressed: false, ..press(keycode, keysym) }
    }

    fn to(v: &Verdict) -> Vec<&str> {
        v.report.as_ref().map(|r| r.to.iter().map(String::as_str).collect()).unwrap_or_default()
    }

    #[test]
    fn idle_without_clients() {
        let mut m = Monitor::default();
        assert!(m.is_idle());
        let v = m.key(&press(38, 'a' as u32), 0, DELAY, false);
        assert_eq!(v, Verdict::default());
        m.watch(":1.1", true);
        assert!(!m.is_idle());
        m.watch(":1.1", false);
        assert!(m.is_idle(), "a client with nothing set up is forgotten");
    }

    #[test]
    fn watchers_hear_every_key_but_do_not_take_it() {
        let mut m = Monitor::default();
        m.watch(":1.1", true);
        m.watch(":1.2", true);
        let key = Key { unichar: 'a' as u32, state: 1, ..press(38, 'A' as u32) };
        let v = m.key(&key, 0, DELAY, false);
        assert!(!v.grabbed);
        let r = v.report.unwrap();
        assert_eq!(r.to, vec![":1.1".to_owned(), ":1.2".to_owned()]);
        assert_eq!((r.released, r.state, r.keysym, r.unichar, r.keycode), (false, 1, 'A' as u32, 'a' as u32, 38));
        let v = m.key(&release(38, 'A' as u32), 10, DELAY, false);
        assert!(!v.grabbed && v.report.unwrap().released);
    }

    #[test]
    fn keyboard_grab_takes_everything_but_vt_switching() {
        let mut m = Monitor::default();
        m.grab_keyboard(":1.1", true);
        m.watch(":1.2", true);
        let v = m.key(&press(38, 'a' as u32), 0, DELAY, false);
        assert!(v.grabbed);
        assert_eq!(to(&v), [":1.1", ":1.2"]);
        assert!(m.key(&release(38, 'a' as u32), 0, DELAY, false).grabbed);
        let v = m.key(&press(67, 0x1008_FE01), 0, DELAY, false);
        assert!(!v.grabbed, "VT switching still works");
        assert_eq!(to(&v), [":1.2"]);
        m.grab_keyboard(":1.1", false);
        assert!(!m.key(&press(38, 'a' as u32), 0, DELAY, false).grabbed);
    }

    #[test]
    fn release_goes_the_way_of_its_press() {
        let mut m = Monitor::default();
        m.watch(":1.2", true);
        // Pressed before the grab: the app gets the release.
        assert!(!m.key(&press(38, 'a' as u32), 0, DELAY, false).grabbed);
        m.grab_keyboard(":1.1", true);
        assert!(!m.key(&release(38, 'a' as u32), 0, DELAY, false).grabbed);
        // Pressed during the grab, released after it: still taken.
        assert!(m.key(&press(39, 's' as u32), 0, DELAY, false).grabbed);
        m.grab_keyboard(":1.1", false);
        let v = m.key(&release(39, 's' as u32), 0, DELAY, false);
        assert!(v.grabbed);
        assert_eq!(to(&v), [":1.1", ":1.2"], "the grabbing client hears the release too");
    }

    #[test]
    fn client_modifier_and_keys_with_it() {
        let mut m = Monitor::default();
        m.set_key_grabs(":1.1", vec![INSERT], vec![]);
        let v = m.key(&press(118, INSERT), 0, DELAY, false);
        assert!(v.grabbed);
        assert_eq!(to(&v), [":1.1"]);
        // Shift and H while Insert is down belong to the client.
        assert!(m.key(&press(50, SHIFT_L), 10, DELAY, false).grabbed);
        assert!(m.key(&press(43, 'H' as u32), 20, DELAY, false).grabbed);
        assert!(m.key(&release(43, 'H' as u32), 30, DELAY, false).grabbed);
        assert!(m.key(&release(50, SHIFT_L), 40, DELAY, false).grabbed);
        assert!(m.key(&release(118, INSERT), 50, DELAY, false).grabbed);
        // Insert was not alone: a quick second press is grabbed again.
        assert!(m.key(&press(118, INSERT), 100, DELAY, false).grabbed);
        assert!(m.key(&release(118, INSERT), 110, DELAY, false).grabbed);
        // Other keys go to the app again.
        let v = m.key(&press(38, 'a' as u32), 2000, DELAY, false);
        assert!(!v.grabbed);
        assert_eq!(v.report, None, "a modifier-only client does not watch");
    }

    #[test]
    fn double_press_of_a_client_modifier_passes_through() {
        let mut m = Monitor::default();
        m.set_key_grabs(":1.1", vec![CAPS], vec![]);
        assert!(m.key(&press(66, CAPS), 0, DELAY, false).grabbed);
        assert!(m.key(&release(66, CAPS), 80, DELAY, false).grabbed);
        let v = m.key(&press(66, CAPS), 300, DELAY, false);
        assert!(!v.grabbed, "the second press toggles Caps Lock");
        assert_eq!(v.report, None);
        assert!(!m.key(&release(66, CAPS), 380, DELAY, false).grabbed);
        // Too slow: both grabbed.
        assert!(m.key(&press(66, CAPS), 1000, DELAY, false).grabbed);
        assert!(m.key(&release(66, CAPS), 1080, DELAY, false).grabbed);
        assert!(m.key(&press(66, CAPS), 1600, DELAY, false).grabbed);
        assert!(m.key(&release(66, CAPS), 1650, DELAY, false).grabbed);
        // Another key in between: no double press.
        assert!(!m.key(&press(38, 'a' as u32), 1700, DELAY, false).grabbed);
        m.key(&release(38, 'a' as u32), 1710, DELAY, false);
        assert!(m.key(&press(66, CAPS), 1800, DELAY, false).grabbed);
    }

    #[test]
    fn keystrokes_match_keysym_and_mask() {
        let mut m = Monitor::default();
        m.set_key_grabs(":1.1", vec![], vec![(KP_ADD, 0), ('h' as u32, 4)]);
        assert!(m.key(&press(86, KP_ADD), 0, DELAY, false).grabbed);
        m.key(&release(86, KP_ADD), 0, DELAY, false);
        // Num Lock and Caps Lock do not matter.
        let with_locks = Key { state: 16 | 2, ..press(86, KP_ADD) };
        assert!(m.key(&with_locks, 0, DELAY, false).grabbed);
        m.key(&release(86, KP_ADD), 0, DELAY, false);
        // Other modifiers do.
        let shifted = Key { state: 1, ..press(86, KP_ADD) };
        assert!(!m.key(&shifted, 0, DELAY, false).grabbed);
        m.key(&release(86, KP_ADD), 0, DELAY, false);
        // Ctrl+h, also when the keysym is shifted (first level matches).
        assert!(m.key(&Key { state: 4, ..press(43, 'h' as u32) }, 0, DELAY, false).grabbed);
        m.key(&release(43, 'h' as u32), 0, DELAY, false);
        assert!(!m.key(&press(43, 'h' as u32), 0, DELAY, false).grabbed);
        m.key(&release(43, 'h' as u32), 0, DELAY, false);
        let caps_h = Key { state: 4 | 2, keysym: 'H' as u32, base_keysym: 'h' as u32, ..press(43, 'H' as u32) };
        assert!(m.key(&caps_h, 0, DELAY, false).grabbed);
        // SetKeyGrabs replaces.
        m.key(&release(43, 'H' as u32), 0, DELAY, false);
        m.set_key_grabs(":1.1", vec![], vec![]);
        assert!(!m.key(&press(86, KP_ADD), 0, DELAY, false).grabbed);
        assert!(m.is_idle() || m.held.is_empty());
    }

    #[test]
    fn several_clients_share_a_grab() {
        let mut m = Monitor::default();
        m.set_key_grabs(":1.1", vec![INSERT], vec![]);
        m.set_key_grabs(":1.2", vec![INSERT], vec![]);
        let v = m.key(&press(118, INSERT), 0, DELAY, false);
        assert_eq!(to(&v), [":1.1", ":1.2"]);
    }

    #[test]
    fn locked_session_stands_aside() {
        let mut m = Monitor::default();
        m.watch(":1.1", true);
        m.grab_keyboard(":1.1", true);
        // Pressed before locking: the release is still the client's.
        assert!(m.key(&press(118, INSERT), 0, DELAY, false).grabbed);
        let v = m.key(&press(38, 'a' as u32), 10, DELAY, true);
        assert_eq!(v, Verdict::default(), "the lock screen gets the password, nobody hears it");
        assert_eq!(m.key(&release(38, 'a' as u32), 20, DELAY, true), Verdict::default());
        let v = m.key(&release(118, INSERT), 30, DELAY, true);
        assert!(v.grabbed);
        assert_eq!(to(&v), [":1.1"]);
    }

    #[test]
    fn leaving_the_bus_drops_everything() {
        let mut m = Monitor::default();
        m.grab_keyboard(":1.1", true);
        m.set_key_grabs(":1.1", vec![INSERT], vec![(KP_ADD, 0)]);
        m.watch(":1.1", true);
        assert!(m.key(&press(38, 'a' as u32), 0, DELAY, false).grabbed);
        m.remove(":1.1");
        // The held key's release is still taken (its press was), but
        // nobody is told.
        let v = m.key(&release(38, 'a' as u32), 0, DELAY, false);
        assert!(v.grabbed);
        assert_eq!(v.report, None);
        assert!(m.is_idle());
        assert!(!m.key(&press(118, INSERT), 0, DELAY, false).grabbed);
    }

    #[test]
    fn lock_keysyms() {
        assert!(is_lock_keysym(CAPS));
        assert!(is_lock_keysym(0xff7f));
        assert!(is_lock_keysym(0xfe08), "ISO_Next_Group switches the layout");
        assert!(!is_lock_keysym(INSERT));
        assert!(!is_lock_keysym(SHIFT_L));
        assert!(!is_lock_keysym(0xfe03), "ISO_Level3_Shift is a plain modifier");
    }
}
