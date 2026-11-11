//! Server side of the private `helix_window_list_v1` protocol: tells the
//! shell about open windows, focus and workspaces, and activates windows,
//! switches workspaces and moves windows between them when asked.
//! Changes are found by comparing snapshots a few times a second.

use std::collections::HashMap;

use helix_protocols::server::helix_window_list_v1::{self, HelixWindowListV1};
use smithay::desktop::Window;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::wm::is_override_redirect;
use super::{ClientState, Helix};

/// What the shell was last told.
#[derive(Debug, Default)]
pub struct WindowList {
    pub(super) listeners: Vec<HelixWindowListV1>,
    pub(super) ids: Vec<(Window, u32)>,
    next_id: u32,
    sent: HashMap<u32, (String, String)>,
    sent_focus: u32,
    sent_workspace: HashMap<u32, u32>,
    /// (count, active)
    sent_workspaces: (u32, u32),
    /// Window icons apps set, as last sent.
    sent_icons: HashMap<u32, super::icons::IconKey>,
    /// Window geometry (x, y, w, h, minimized), as last sent.
    sent_geometry: HashMap<u32, Geometry>,
    /// The primary monitor's area, as last sent.
    sent_primary: Option<(i32, i32, i32, i32)>,
}

type Geometry = (i32, i32, i32, i32, u32);

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
    /// The primary monitor's area (x, y, w, h).
    fn primary_area(&self) -> (i32, i32, i32, i32) {
        let g = self.primary().and_then(|o| self.space.output_geometry(&o)).unwrap_or_default();
        (g.loc.x, g.loc.y, g.size.w, g.size.h)
    }

    /// Where `window` is (or will be shown).
    pub fn window_geometry_location(&self, window: &Window) -> smithay::utils::Point<i32, smithay::utils::Logical> {
        let g = self.window_geometry(window);
        (g.0, g.1).into()
    }

    /// Where `window` is (or will be shown), and whether it is minimized.
    fn window_geometry(&self, window: &Window) -> Geometry {
        let size = window.geometry().size;
        let (at, minimized) = if let Some(at) = self.space.element_location(window) {
            (at, false)
        } else if let Some(at) = self.workspaces.parked_at(window) {
            (at, false)
        } else if let Some(i) = self.minimized.iter().position(|w| w == window) {
            (self.minimized_at.get(i).copied().unwrap_or_default(), true)
        } else {
            Default::default()
        };
        (at.x, at.y, size.w, size.h, u32::from(minimized))
    }

    /// Windows the shell may list, topmost first, with their ids.
    fn listed_windows(&mut self) -> Vec<(u32, Window)> {
        let windows: Vec<Window> = self
            .space
            .elements()
            .rev()
            .chain(self.minimized.iter())
            .chain(self.workspaces.parked())
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

    /// The window the shell knows by `id`.
    pub fn window_by_list_id(&self, id: u32) -> Option<Window> {
        self.window_list.ids.iter().find(|(_, i)| *i == id).map(|(w, _)| w.clone())
    }

    fn focused_window_id(&self) -> u32 {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let Some(window) = focus.and_then(|f| self.window_for_focus(&f)) else { return 0 };
        self.window_list.ids.iter().find(|(w, _)| *w == window).map(|(_, id)| *id).unwrap_or(0)
    }

    /// A window's workspace for the protocol (every workspace: 0xffffffff).
    fn wire_workspace(&mut self, window: &Window) -> u32 {
        if self.workspaces.is_sticky(window) {
            u32::MAX
        } else {
            self.workspace_of(window) as u32
        }
    }

    fn layout_wire(&self) -> (u32, u32) {
        let layout = self.workspaces.layout;
        (layout.count as u32, layout.active as u32)
    }

    /// Send the shell whatever changed since last time.
    pub fn sync_window_list(&mut self) {
        self.tidy_workspaces();
        if self.window_list.listeners.is_empty() {
            return;
        }
        let listed = self.listed_windows();
        let workspaces: Vec<(u32, u32)> = listed.iter().map(|(id, w)| (*id, self.wire_workspace(w))).collect();
        let now: Vec<(u32, (String, String))> = listed.into_iter().map(|(id, w)| (id, describe(&w))).collect();
        let focus = self.focused_window_id();
        let layout = self.layout_wire();
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
        let v2: Vec<&HelixWindowListV1> = list.listeners.iter().filter(|l| l.version() >= 2).collect();
        if layout != list.sent_workspaces {
            list.sent_workspaces = layout;
            for l in &v2 {
                l.workspaces(layout.0, layout.1);
            }
        }
        list.sent_workspace.retain(|id, _| list.sent.contains_key(id));
        for (id, ws) in workspaces {
            if list.sent_workspace.get(&id) != Some(&ws) {
                list.sent_workspace.insert(id, ws);
                for l in &v2 {
                    l.workspace(id, ws);
                }
            }
        }
        // The primary monitor (version 8).
        let primary = self.primary_area();
        if self.window_list.sent_primary != Some(primary) {
            self.window_list.sent_primary = Some(primary);
            for l in self.window_list.listeners.iter().filter(|l| l.version() >= 8) {
                l.primary(primary.0, primary.1, primary.2, primary.3);
            }
        }
        // Where windows are (version 8).
        let geometry: Vec<(u32, Geometry)> = self.listed_windows().into_iter().map(|(id, w)| (id, self.window_geometry(&w))).collect();
        let list = &mut self.window_list;
        list.sent_geometry.retain(|id, _| list.sent.contains_key(id));
        for (id, g) in geometry {
            if list.sent_geometry.get(&id) != Some(&g) {
                for l in list.listeners.iter().filter(|l| l.version() >= 8) {
                    l.geometry(id, g.0, g.1, g.2, g.3, g.4);
                }
                list.sent_geometry.insert(id, g);
            }
        }
        // Icons apps set (version 7).
        let listed = self.listed_windows();
        let x11_alive: Vec<u32> = listed.iter().filter_map(|(_, w)| w.x11_surface().map(|x| x.window_id())).collect();
        self.forget_x11_icons(&x11_alive);
        let icons: Vec<(u32, Window, super::icons::IconKey)> = listed
            .into_iter()
            .map(|(id, w)| {
                let key = self.icon_key(&w);
                (id, w, key)
            })
            .collect();
        self.window_list.sent_icons.retain(|id, _| self.window_list.sent.contains_key(id));
        for (id, window, key) in icons {
            if self.window_list.sent_icons.get(&id).unwrap_or(&Default::default()) != &key {
                for l in &self.window_list.listeners {
                    self.send_icon(l, id, &window);
                }
                self.window_list.sent_icons.insert(id, key);
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
        let layout = state.layout_wire();
        let workspaces: HashMap<u32, u32> = windows.iter().map(|(id, w)| (*id, state.wire_workspace(w))).collect();
        if listener.version() >= 2 {
            listener.workspaces(layout.0, layout.1);
            for (id, ws) in &workspaces {
                listener.workspace(*id, *ws);
            }
        }
        let primary = state.primary_area();
        if listener.version() >= 8 {
            listener.primary(primary.0, primary.1, primary.2, primary.3);
        }
        let mut geometry = HashMap::new();
        for (id, window) in &windows {
            let g = state.window_geometry(window);
            if listener.version() >= 8 {
                listener.geometry(*id, g.0, g.1, g.2, g.3, g.4);
            }
            geometry.insert(*id, g);
        }
        let mut icons = HashMap::new();
        for (id, window) in &windows {
            let key = state.icon_key(window);
            if key != super::icons::IconKey::default() {
                state.send_icon(&listener, *id, window);
            }
            icons.insert(*id, key);
        }
        state.window_list.listeners.push(listener);
        if state.window_list.listeners.len() == 1 {
            // First listener: what it was told is the baseline.
            state.window_list.sent = windows.iter().map(|(id, w)| (*id, describe(w))).collect();
            state.window_list.sent_focus = state.focused_window_id();
            state.window_list.sent_workspaces = layout;
            state.window_list.sent_workspace = workspaces;
            state.window_list.sent_icons = icons;
            state.window_list.sent_geometry = geometry;
            state.window_list.sent_primary = Some(primary);
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
        match request {
            helix_window_list_v1::Request::Activate { id } => state.activate_window(id),
            helix_window_list_v1::Request::SwitchWorkspace { workspace } => state.switch_workspace(workspace as usize),
            helix_window_list_v1::Request::SetOnAllWorkspaces { id, on } => {
                if let Some(window) = state.window_by_list_id(id) {
                    state.set_on_all_workspaces(&window, on != 0);
                }
            }
            helix_window_list_v1::Request::WindowAction { id, action: smithay::reexports::wayland_server::WEnum::Value(action) } => {
                state.window_action(id, action);
            }
            helix_window_list_v1::Request::AllowShortcutsInhibit { id, allowed } => {
                state.answer_shortcuts_inhibit(id, allowed != 0);
            }
            helix_window_list_v1::Request::MoveWindow { id, workspace } => {
                let window = state.window_list.ids.iter().find(|(_, i)| *i == id).map(|(w, _)| w.clone());
                if let Some(window) = window {
                    state.move_to_workspace(&window, workspace as usize);
                }
            }
            _ => {}
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
