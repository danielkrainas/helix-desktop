//! Mirrored surfaces for right-to-left languages (helix-desktop-spec
//! `components/i18n.md`, "Right-to-left languages").
//!
//! A surface the shell lays out itself (the panel, menus, dialogs,
//! notifications, the lock screen…) is painted on a mirrored
//! [`helix_draw::Canvas`] when the interface reads right to left: the
//! layout code stays left to right and the drawing lands mirrored. Pointer
//! positions on such a surface are mirrored back before hit testing, so
//! the same hit rectangles serve both directions.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::Proxy;
use smithay_client_toolkit::seat::pointer::PointerEvent;

#[derive(Default)]
struct State {
    /// Logical widths, as last prepared.
    widths: HashMap<ObjectId, f64>,
    /// Surfaces drawn mirrored.
    mirrored: HashSet<ObjectId>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

/// Remember `surface`'s logical width (from `Scales::prepare`).
pub(super) fn note_width(surface: &WlSurface, w: i32) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.widths.insert(surface.id(), f64::from(w));
}

/// Whether `surface` is to be drawn mirrored (the interface reads right to
/// left); remembered for its pointer events. Use it as
/// `Canvas::scaled(..).mirrored(mirror::on(surface))`.
pub(super) fn on(surface: &WlSurface) -> bool {
    let rtl = helix_draw::rtl();
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    if rtl {
        s.mirrored.insert(surface.id());
    } else {
        s.mirrored.remove(&surface.id());
    }
    rtl
}

/// `event` with its position mirrored back on a mirrored surface.
pub(super) fn adjust(event: &PointerEvent) -> PointerEvent {
    let mut event = event.clone();
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let id = event.surface.id();
    if s.mirrored.contains(&id) {
        if let Some(w) = s.widths.get(&id) {
            event.position.0 = w - event.position.0;
        }
    }
    event
}

/// A surface-local point as drawn: mirrored on a mirrored surface (the
/// inverse of [`adjust`], for points computed from the layout, such as an
/// accessibility action's click).
pub(super) fn shown(surface: &WlSurface, (x, y): (f64, f64)) -> (f64, f64) {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let id = surface.id();
    match (s.mirrored.contains(&id), s.widths.get(&id)) {
        (true, Some(w)) => (w - x, y),
        _ => (x, y),
    }
}
