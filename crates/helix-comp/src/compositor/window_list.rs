//! Server side of the private `helix_window_list_v1` protocol: tells the
//! shell about open windows and focus, and activates windows it asks for.
//! Changes are found by comparing snapshots a few times a second.

use std::collections::HashMap;

use helix_protocols::server::helix_window_list_v1::{self, HelixWindowListV1};
use smithay::desktop::Window;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::wm::is_override_redirect;
use super::{ClientState, Helix};

/// What the shell was last told.
#[derive(Debug, Default)]
pub struct WindowList {
    listeners: Vec<HelixWindowListV1>,
    ids: Vec<(Window, u32)>,
    next_id: u32,
    sent: HashMap<u32, (String, String)>,
    sent_focus: u32,
}

/// (app id, title)
fn describe(window: &Window) -> (String, String) {
    if let Some(toplevel) = window.toplevel() {
        return with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| {
                    let data = data.lock().unwrap();
                    (data.app_id.clone().unwrap_or_default(), data.title.clone().unwrap_or_default())
                })
                .unwrap_or_default()
        });
    }
    if let Some(x11) = window.x11_surface() {
        return (x11.class(), x11.title());
    }
    Default::default()
}

impl Helix {
    /// Windows the shell may list, topmost first, with their ids.
    fn listed_windows(&mut self) -> Vec<(u32, Window)> {
        let windows: Vec<Window> = self
            .space
            .elements()
            .rev()
            .chain(self.minimized.iter())
            .filter(|w| !is_override_redirect(w))
            .cloned()
            .collect();
        let list = &mut self.window_list;
        list.ids.retain(|(w, _)| windows.contains(w));
        windows
            .into_iter()
            .map(|w| {
                let id = match list.ids.iter().find(|(known, _)| *known == w) {
                    Some((_, id)) => *id,
                    None => {
                        list.next_id = list.next_id.wrapping_add(1).max(1);
                        list.ids.push((w.clone(), list.next_id));
                        list.next_id
                    }
                };
                (id, w)
            })
            .collect()
    }

    fn focused_window_id(&self) -> u32 {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let Some(window) = focus.and_then(|s| self.window_for_surface(&s)) else { return 0 };
        self.window_list.ids.iter().find(|(w, _)| *w == window).map(|(_, id)| *id).unwrap_or(0)
    }

    /// Send the shell whatever changed since last time.
    pub fn sync_window_list(&mut self) {
        if self.window_list.listeners.is_empty() {
            return;
        }
        let now: Vec<(u32, (String, String))> = self
            .listed_windows()
            .into_iter()
            .map(|(id, w)| (id, describe(&w)))
            .collect();
        let focus = self.focused_window_id();
        let list = &mut self.window_list;
        let closed: Vec<u32> = list.sent.keys().filter(|id| !now.iter().any(|(n, _)| n == *id)).copied().collect();
        for id in closed {
            list.sent.remove(&id);
            for l in &list.listeners {
                l.closed(id);
            }
        }
        for (id, info) in now {
            if list.sent.get(&id) != Some(&info) {
                for l in &list.listeners {
                    l.window(id, info.0.clone(), info.1.clone());
                }
                list.sent.insert(id, info);
            }
        }
        if focus != list.sent_focus {
            list.sent_focus = focus;
            for l in &list.listeners {
                l.focused(focus);
            }
        }
    }

    fn activate_window(&mut self, id: u32) {
        let window = self.window_list.ids.iter().find(|(_, i)| *i == id).map(|(w, _)| w.clone());
        match window {
            Some(window) => {
                tracing::debug!(id, "activating window for the shell");
                self.focus_window(&window);
                self.sync_window_list();
            }
            None => tracing::debug!(id, "shell asked to activate an unknown window"),
        }
    }
}

impl GlobalDispatch<HelixWindowListV1, ()> for Helix {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<HelixWindowListV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let listener = data_init.init(resource, ());
        // Everything as it is now, to this listener only.
        let windows: Vec<(u32, Window)> = state.listed_windows();
        for (id, window) in &windows {
            let (app_id, title) = describe(window);
            listener.window(*id, app_id, title);
        }
        listener.focused(state.focused_window_id());
        state.window_list.listeners.push(listener);
        if state.window_list.listeners.len() == 1 {
            // First listener: what it was told is the baseline.
            state.window_list.sent = windows.iter().map(|(id, w)| (*id, describe(w))).collect();
            state.window_list.sent_focus = state.focused_window_id();
        }
    }

    fn can_view(client: Client, _data: &()) -> bool {
        client.get_data::<ClientState>().is_some_and(|c| c.privileged)
    }
}

impl Dispatch<HelixWindowListV1, ()> for Helix {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &HelixWindowListV1,
        request: helix_window_list_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let helix_window_list_v1::Request::Activate { id } = request {
            state.activate_window(id);
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &HelixWindowListV1,
        _data: &(),
    ) {
        state.window_list.listeners.retain(|l| l != resource);
    }
}
