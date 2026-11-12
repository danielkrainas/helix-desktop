//! Notifications, independent of D-Bus: the queue of banners, the list
//! notifications stay in after their banner, replacement, expiry and close
//! reasons. Behavior:
//! `helix-desktop-spec/contracts/dbus/org.freedesktop.Notifications.md` and
//! `org.gtk.Notifications.md`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Banners last this long unless the app asks otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// The most notifications kept in the list.
pub const LIST_LIMIT: usize = 50;

/// `NotificationClosed` reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// What to do when the notification (or a button) is activated.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// org.freedesktop.Notifications: report the action key by signal.
    Freedesktop { resident: bool },
    /// org.gtk.Notifications: activate the app's actions.
    /// (Action parameters are kept by the D-Bus layer.)
    Gtk {
        app_id: String,
        id: String,
        /// Default action's detailed name, if any.
        default_action: Option<String>,
        /// Buttons' action names, by index (key `button-N`).
        buttons: Vec<String>,
        /// The button whose purpose is `im.reply-with-text`: choosing it
        /// asks for the reply first.
        reply: Option<usize>,
    },
    /// The shell's own: a screenshot it saved (click opens it; the
    /// `show-in-files` button shows it in the file manager).
    Screenshot { path: std::path::PathBuf },
    /// The shell's own: removable media; `open`/`eject` buttons act on
    /// the mount at `root` (a URI), when there is one.
    Media { root: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    /// Buttons as (key, label); the click action has key `default`.
    pub actions: Vec<(String, String)>,
    pub has_default: bool,
    pub urgency: Urgency,
    /// How long the banner shows; `None`: until dismissed.
    pub timeout: Option<Duration>,
    pub origin: Origin,
    pub icon: Option<crate::icons::IconSource>,
    /// The body uses notification markup (else plain text).
    pub markup: bool,
    /// Not kept in the list once its banner goes.
    pub transient: bool,
    /// The app it is from (see `app_notify`); empty for the shell's own
    /// and apps that did not say.
    pub app_id: String,
}

impl Notification {
    /// Timeout from `Notify`'s `expire_timeout` (ms; -1 default, 0 never).
    pub fn timeout_from(expire_timeout: i32, urgency: Urgency) -> Option<Duration> {
        if urgency == Urgency::Critical {
            return None;
        }
        match expire_timeout {
            0 => None,
            ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => Some(DEFAULT_TIMEOUT),
        }
    }
}

#[derive(Debug, Default)]
pub struct Store {
    items: BTreeMap<u32, Notification>,
    /// Waiting to be shown, oldest first.
    queue: Vec<u32>,
    /// On screen now, and since when.
    current: Option<(u32, Instant)>,
    next_id: u32,
    /// List order: when each notification was posted or last replaced.
    order: BTreeMap<u32, u64>,
    posted: u64,
    /// Posted since the list was last looked at.
    pub unseen: bool,
}

impl Store {
    /// Add, or replace in place if `notification.id` is a live id. Returns
    /// the id (fresh ids are never 0). With `banner` false (do not disturb)
    /// it only goes to the list.
    pub fn add(&mut self, mut notification: Notification, banner: bool) -> u32 {
        if notification.id == 0 || !self.items.contains_key(&notification.id) {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            while self.items.contains_key(&self.next_id) {
                self.next_id = self.next_id.wrapping_add(1).max(1);
            }
            notification.id = self.next_id;
            if banner {
                self.queue.push(notification.id);
            }
        } else if self.current.is_some_and(|(id, _)| id == notification.id) {
            // Replacing what is on screen restarts its timer.
            self.current = Some((notification.id, Instant::now()));
        }
        let id = notification.id;
        self.items.insert(id, notification);
        self.posted += 1;
        self.order.insert(id, self.posted);
        self.unseen = true;
        id
    }

    /// The list, newest first.
    pub fn list(&self) -> Vec<&Notification> {
        let mut ids: Vec<(&u32, &u64)> = self.order.iter().collect();
        ids.sort_by(|a, b| b.1.cmp(a.1));
        ids.into_iter().filter_map(|(id, _)| self.items.get(id)).collect()
    }

    /// The oldest notifications beyond [`LIST_LIMIT`], to close.
    pub fn overflow(&self) -> Vec<u32> {
        let list = self.list();
        list.iter().skip(LIST_LIMIT).map(|n| n.id).collect()
    }

    /// The banner of `id` is done (timed out); the notification stays in
    /// the list.
    pub fn banner_done(&mut self, id: u32) {
        if self.current.is_some_and(|(c, _)| c == id) {
            self.current = None;
        }
    }

    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.items.get(&id)
    }

    /// The live notification an app posted over org.gtk.Notifications.
    pub fn find_gtk(&self, app_id: &str, gtk_id: &str) -> Option<u32> {
        self.items.values().find_map(|n| match &n.origin {
            Origin::Gtk { app_id: a, id, .. } if a == app_id && id == gtk_id => Some(n.id),
            _ => None,
        })
    }

    /// Remove a notification (from the screen or the queue).
    pub fn close(&mut self, id: u32) -> Option<Notification> {
        let n = self.items.remove(&id)?;
        self.order.remove(&id);
        self.queue.retain(|q| *q != id);
        if self.current.is_some_and(|(c, _)| c == id) {
            self.current = None;
        }
        Some(n)
    }

    /// The banner to show now. When `visible` is false (locked, or banners
    /// switched off for this urgency) nothing is shown and nothing starts
    /// expiring.
    pub fn current(&mut self, now: Instant, may_show: impl Fn(&Notification) -> bool) -> Option<&Notification> {
        if self.current.is_none() {
            let next = self.queue.iter().position(|id| self.items.get(id).is_some_and(&may_show))?;
            let id = self.queue.remove(next);
            self.current = Some((id, now));
        }
        let (id, _) = self.current?;
        let n = self.items.get(&id)?;
        if may_show(n) { Some(n) } else { None }
    }

    /// Ids of shown notifications whose time is up.
    pub fn expired(&self, now: Instant) -> Option<u32> {
        let (id, since) = self.current?;
        let timeout = self.items.get(&id)?.timeout?;
        (now.duration_since(since) >= timeout).then_some(id)
    }

    /// Put the on-screen notification back in front of the queue (e.g. the
    /// screen locked before it was read).
    pub fn requeue_current(&mut self) {
        if let Some((id, _)) = self.current.take() {
            self.queue.insert(0, id);
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(summary: &str, urgency: Urgency, timeout: Option<Duration>) -> Notification {
        Notification {
            id: 0,
            app_name: "test".into(),
            summary: summary.into(),
            body: String::new(),
            actions: Vec::new(),
            has_default: false,
            urgency,
            timeout,
            origin: Origin::Freedesktop { resident: false },
            icon: None,
            markup: false,
            transient: false,
            app_id: String::new(),
        }
    }

    const ALL: fn(&Notification) -> bool = |_| true;

    #[test]
    fn timeouts() {
        assert_eq!(Notification::timeout_from(-1, Urgency::Normal), Some(DEFAULT_TIMEOUT));
        assert_eq!(Notification::timeout_from(0, Urgency::Normal), None);
        assert_eq!(Notification::timeout_from(1500, Urgency::Low), Some(Duration::from_millis(1500)));
        assert_eq!(Notification::timeout_from(1500, Urgency::Critical), None, "critical never expires");
    }

    #[test]
    fn queue_is_fifo_one_at_a_time() {
        let mut s = Store::default();
        let t = Instant::now();
        let a = s.add(n("a", Urgency::Normal, Some(DEFAULT_TIMEOUT)), true);
        let b = s.add(n("b", Urgency::Normal, Some(DEFAULT_TIMEOUT)), true);
        assert!(a != 0 && b != a);
        assert_eq!(s.current(t, ALL).unwrap().id, a);
        assert_eq!(s.current(t, ALL).unwrap().id, a, "stays until closed");
        s.close(a);
        assert_eq!(s.current(t, ALL).unwrap().id, b);
        s.close(b);
        assert!(s.current(t, ALL).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn replace_keeps_id_and_restarts_timer() {
        let mut s = Store::default();
        let t = Instant::now();
        let a = s.add(n("v1", Urgency::Normal, Some(Duration::from_secs(1))), true);
        s.current(t, ALL);
        let mut v2 = n("v2", Urgency::Normal, Some(Duration::from_secs(1)));
        v2.id = a;
        assert_eq!(s.add(v2, true), a);
        assert_eq!(s.get(a).unwrap().summary, "v2");
        assert_eq!(s.len(), 1);
        // Unknown replaces_id: a new notification.
        let mut v3 = n("v3", Urgency::Normal, None);
        v3.id = 999;
        assert_ne!(s.add(v3, true), 999);
    }

    #[test]
    fn expiry_and_critical() {
        let mut s = Store::default();
        let t = Instant::now();
        let a = s.add(n("a", Urgency::Normal, Some(Duration::from_secs(5))), true);
        s.current(t, ALL);
        assert_eq!(s.expired(t + Duration::from_secs(4)), None);
        assert_eq!(s.expired(t + Duration::from_secs(5)), Some(a));
        s.close(a);
        let c = s.add(n("c", Urgency::Critical, None), true);
        s.current(t, ALL);
        assert_eq!(s.expired(t + Duration::from_secs(3600)), None);
        assert_eq!(s.current(t, ALL).unwrap().id, c);
    }

    #[test]
    fn hidden_notifications_wait() {
        let mut s = Store::default();
        let t = Instant::now();
        let low = s.add(n("low", Urgency::Low, Some(DEFAULT_TIMEOUT)), true);
        let crit = s.add(n("crit", Urgency::Critical, None), true);
        // Do-not-disturb: only critical shows, skipping ahead of the queue.
        let dnd = |n: &Notification| n.urgency == Urgency::Critical;
        assert_eq!(s.current(t, dnd).unwrap().id, crit);
        s.close(crit);
        assert!(s.current(t, dnd).is_none());
        // Banners back on: the low one shows.
        assert_eq!(s.current(t, ALL).unwrap().id, low);
        // Locked: requeued, shown again later.
        s.requeue_current();
        assert_eq!(s.current(t, ALL).unwrap().id, low);
    }

    #[test]
    fn list_and_banners() {
        let mut s = Store::default();
        let t = Instant::now();
        let a = s.add(n("a", Urgency::Normal, Some(DEFAULT_TIMEOUT)), true);
        let quiet = s.add(n("quiet", Urgency::Normal, Some(DEFAULT_TIMEOUT)), false);
        assert!(s.unseen);
        assert_eq!(s.list().iter().map(|n| n.id).collect::<Vec<_>>(), [quiet, a], "newest first");
        assert_eq!(s.current(t, ALL).unwrap().id, a);
        s.banner_done(a);
        assert!(s.current(t, ALL).is_none(), "the quiet one never gets a banner");
        assert_eq!(s.list().len(), 2, "both stay listed");
        // Replacing moves it to the top.
        let mut again = n("a2", Urgency::Normal, None);
        again.id = a;
        s.add(again, false);
        assert_eq!(s.list()[0].id, a);
        for i in 0..LIST_LIMIT {
            s.add(n(&format!("{i}"), Urgency::Normal, None), false);
        }
        assert_eq!(s.overflow(), [a, quiet], "the oldest two, newest first");
    }

    #[test]
    fn gtk_lookup() {
        let mut s = Store::default();
        let mut g = n("g", Urgency::Normal, None);
        g.origin = Origin::Gtk { app_id: "org.example.App".into(), id: "x".into(), default_action: None, buttons: vec![], reply: None };
        let id = s.add(g, true);
        assert_eq!(s.find_gtk("org.example.App", "x"), Some(id));
        assert_eq!(s.find_gtk("org.example.App", "y"), None);
    }
}
