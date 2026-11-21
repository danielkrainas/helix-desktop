//! What a window chosen by the shell (overview, dock, switcher) looks like
//! a moment later, logged once: whether it has the keyboard and the
//! pointer, whether it is drawing (commits, frame callbacks waiting,
//! configures not acknowledged), what is above it and which grabs hold the
//! input. For windows that sometimes stay a still picture without taking
//! input after being chosen; the next time it happens the log says where
//! the input went and whether the app is drawing.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use smithay::desktop::{layer_map_for_output, PopupManager, Window};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::{get_parent, with_states, with_surface_tree_downward, SurfaceAttributes, TraversalAction};
use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use super::Helix;

/// How long after the choice the report is made.
const AFTER: Duration = Duration::from_millis(1500);

/// Commits of a window's surfaces (its root surface's data).
#[derive(Default)]
struct Commits {
    count: u64,
    last: Option<Instant>,
}

/// A commit to `surface` (any surface of a window's tree, or another).
pub fn count_commit(surface: &WlSurface) {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    with_states(&root, |states| {
        states.data_map.insert_if_missing(|| RefCell::new(Commits::default()));
        if let Some(c) = states.data_map.get::<RefCell<Commits>>() {
            let mut c = c.borrow_mut();
            c.count += 1;
            c.last = Some(Instant::now());
        }
    });
}

fn commits(root: &WlSurface) -> (u64, Option<Instant>) {
    with_states(root, |states| states.data_map.get::<RefCell<Commits>>().map_or((0, None), |c| (c.borrow().count, c.borrow().last)))
}

/// Frame callbacks asked for and not answered yet, in `root`'s tree.
fn callbacks_waiting(root: &WlSurface) -> usize {
    let mut n = 0;
    with_surface_tree_downward(
        root,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| n += states.cached_state.get::<SurfaceAttributes>().current().frame_callbacks.len(),
        |_, _, _| true,
    );
    n
}

/// Configures sent and not acknowledged yet.
fn configures_waiting(root: &WlSurface) -> Option<usize> {
    with_states(root, |states| states.data_map.get::<XdgToplevelSurfaceData>().map(|d| d.lock().unwrap().pending_configures().len()))
}

/// What `surface` is to `window`: its own (root, subsurface, popup), a
/// layer surface, another app's, or gone.
fn relation(state: &Helix, window: &Window, surface: Option<&WlSurface>) -> String {
    let Some(surface) = surface else { return "nothing".into() };
    if !surface.alive() {
        return "a destroyed surface".into();
    }
    let Some(root) = super::wm::surface_of(window) else { return "?".into() };
    let mut top = surface.clone();
    while let Some(parent) = get_parent(&top) {
        top = parent;
    }
    if top == root {
        return if surface == &root { "the window".into() } else { "a subsurface of the window".into() };
    }
    if PopupManager::popups_for_surface(&root).any(|(p, _)| p.wl_surface() == &top) {
        return "a popup of the window".into();
    }
    let role = with_states(surface, |s| s.role.unwrap_or("none"));
    let layer = state.space.outputs().find_map(|o| {
        let map = layer_map_for_output(o);
        let found = map.layers().find(|l| l.wl_surface() == &top).map(|l| l.namespace().to_owned());
        found
    });
    if let Some(namespace) = layer {
        return format!("layer surface {namespace}");
    }
    let same_client = surface.client().zip(root.client()).is_some_and(|(a, b)| a.id() == b.id());
    let window = state.window_for_surface(&top).map(|w| super::window_list::describe(&w).1);
    format!("{} surface (role {role}){}", if same_client { "another of the app's" } else { "another app's" }, window.map(|t| format!(" of window {t:?}")).unwrap_or_default())
}

impl Helix {
    /// The shell chose `window`: report on it a moment later.
    pub fn report_activation_later(&mut self, window: &Window) {
        let Some(root) = super::wm::surface_of(window) else { return };
        let (count, _) = commits(&root);
        let window = window.clone();
        let _ = self.loop_handle.insert_source(Timer::from_duration(AFTER), move |_, _, state| {
            state.report_activation(&window, &root, count);
            TimeoutAction::Drop
        });
    }

    fn report_activation(&mut self, window: &Window, root: &WlSurface, commits_before: u64) {
        if !window.alive() {
            return;
        }
        let (app_id, title) = super::window_list::describe(window);
        let (count, last) = commits(root);
        let keyboard = self.seat.get_keyboard();
        let pointer = self.seat.get_pointer();
        let keyboard_on = relation(self, window, self.focused_surface().as_ref());
        let pointer_focus = pointer.as_ref().and_then(|p| p.current_focus());
        let pointer_on = relation(self, window, pointer_focus.as_ref());
        let under = pointer.as_ref().map(|p| p.current_location()).and_then(|pos| self.surface_under(pos)).map(|(s, _)| s);
        let under_is = relation(self, window, under.as_ref());
        let mapped = self.space.elements().any(|w| w == window);
        let on_top = self.top_window().as_ref() == Some(window);
        let activated = window.toplevel().map(|t| t.current_state().states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated));
        let exclusive: Vec<String> = self
            .space
            .outputs()
            .flat_map(|o| {
                layer_map_for_output(o)
                    .layers()
                    .filter(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive)
                    .map(|l| l.namespace().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        tracing::info!(
            app_id,
            title,
            keyboard = keyboard_on,
            keyboard_grabbed = keyboard.as_ref().is_some_and(|k| k.is_grabbed()),
            pointer = pointer_on,
            pointer_grabbed = pointer.as_ref().is_some_and(|p| p.is_grabbed()),
            under_pointer = under_is,
            commits = count - commits_before,
            last_commit_ms = last.map(|t| t.elapsed().as_millis() as u64),
            frame_callbacks_waiting = callbacks_waiting(root),
            configures_unacked = configures_waiting(root),
            activated,
            mapped,
            on_top,
            exclusive_layers = exclusive.join(","),
            "chosen window, {} ms later",
            AFTER.as_millis()
        );
    }
}
