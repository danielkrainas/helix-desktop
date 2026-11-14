//! One AccessKit adapter per shown surface.
//!
//! AccessKit's Unix adapter registers the application on the
//! accessibility bus once a client asks for it (`org.a11y.Status`
//! `IsEnabled`), serves AT-SPI for every tree, and turns tree changes into
//! AT-SPI events. Trees are only built for adapters that are active.

use std::collections::HashMap;
use std::sync::Arc;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, NodeId, TreeUpdate};
use accesskit_unix::Adapter;

use super::tree::{self, Target};
use super::{Rect, Window};

/// What the adapters tell the UI (from their own thread).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A client wants window `serial`'s tree.
    Activated(u64),
    /// Nobody wants it any more.
    Deactivated(u64),
    /// An action on a node of window `serial`.
    Action(u64, ActionRequest),
}

/// Where adapter events go: called on the adapters' thread.
pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

struct Handler {
    serial: u64,
    notify: Notify,
}

impl ActivationHandler for Handler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        // Built on the UI thread, which pushes it at once.
        (self.notify)(Event::Activated(self.serial));
        None
    }
}

impl ActionHandler for Handler {
    fn do_action(&mut self, request: ActionRequest) {
        (self.notify)(Event::Action(self.serial, request));
    }
}

impl DeactivationHandler for Handler {
    fn deactivate_accessibility(&mut self) {
        (self.notify)(Event::Deactivated(self.serial));
    }
}

struct Published<H> {
    serial: u64,
    adapter: Adapter,
    handle: H,
    active: bool,
    /// What the adapter has (`None`: nothing yet, or it must be sent again).
    last: Option<Window>,
    focused: Option<bool>,
    bounds: Option<Rect>,
    targets: HashMap<NodeId, Target>,
}

/// The shell's windows on the accessibility bus, by key.
pub struct Publisher<H> {
    notify: Notify,
    windows: Vec<(String, Published<H>)>,
    next: u64,
}

impl<H: Clone> Publisher<H> {
    pub fn new(notify: Notify) -> Self {
        Self { notify, windows: Vec::new(), next: 1 }
    }

    /// Bring the adapters in line with the surfaces `shown` (key, handle),
    /// describing the windows a client is reading with `describe`.
    pub fn sync(&mut self, shown: Vec<(String, H)>, mut describe: impl FnMut(&str) -> Option<Window>) {
        // Gone surfaces first: their windows leave the application.
        self.windows.retain(|(key, _)| shown.iter().any(|(k, _)| k == key));
        for (key, handle) in shown {
            let index = match self.windows.iter().position(|(k, _)| *k == key) {
                Some(i) => i,
                None => {
                    let serial = self.next;
                    self.next += 1;
                    let handler = || Handler { serial, notify: self.notify.clone() };
                    let adapter = Adapter::new(handler(), handler(), handler());
                    self.windows.push((key.clone(), Published { serial, adapter, handle: handle.clone(), active: false, last: None, focused: None, bounds: None, targets: HashMap::new() }));
                    self.windows.len() - 1
                }
            };
            let published = &mut self.windows[index].1;
            published.handle = handle;
            if !published.active {
                continue;
            }
            let Some(window) = describe(&key) else { continue };
            if published.bounds != Some(window.bounds) {
                let b = window.bounds;
                let r = accesskit::Rect::new(f64::from(b.x), f64::from(b.y), f64::from(b.x + b.w), f64::from(b.y + b.h));
                published.adapter.set_root_window_bounds(r, r);
                published.bounds = Some(b);
            }
            if published.last.as_ref() != Some(&window) {
                let built = tree::build(&window);
                published.targets = built.targets;
                let update = built.update;
                published.adapter.update_if_active(move || update);
            }
            if published.focused != Some(window.active) {
                published.adapter.update_window_focus_state(window.active);
                published.focused = Some(window.active);
            }
            published.last = Some(window);
        }
    }

    fn by_serial(&mut self, serial: u64) -> Option<&mut Published<H>> {
        self.windows.iter_mut().map(|(_, p)| p).find(|p| p.serial == serial)
    }

    /// Note an adapter's event; for an action, what it acts on: the
    /// surface's handle, the node's target, and the request.
    pub fn event(&mut self, event: Event) -> Option<(H, Target, ActionRequest)> {
        match event {
            Event::Activated(serial) => {
                if let Some(p) = self.by_serial(serial) {
                    p.active = true;
                    p.last = None;
                    p.focused = None;
                    p.bounds = None;
                }
                None
            }
            Event::Deactivated(serial) => {
                if let Some(p) = self.by_serial(serial) {
                    p.active = false;
                    p.last = None;
                }
                None
            }
            Event::Action(serial, request) => {
                let p = self.by_serial(serial)?;
                let target = *p.targets.get(&request.target_node)?;
                Some((p.handle.clone(), target, request))
            }
        }
    }

    /// Some window is being read (a client asked for the trees).
    pub fn any_active(&self) -> bool {
        self.windows.iter().any(|(_, p)| p.active)
    }

    /// The windows published, by key.
    pub fn keys(&self) -> Vec<&str> {
        self.windows.iter().map(|(k, _)| k.as_str()).collect()
    }
}
