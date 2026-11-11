//! Session locking (`ext-session-lock-v1`).
//!
//! While locked, outputs show only the lock client's surfaces (or nothing),
//! and input goes only to them. The lock outlives its client: if the lock
//! client dies the session stays locked until a new lock client (the
//! restarted shell) takes over and unlocks.

use smithay::desktop::utils::send_frames_surface_tree;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use super::{ClientState, Helix};

#[derive(Default)]
pub struct LockState {
    pub locked: bool,
    /// The lock object of the current lock client, to tell if it died.
    current: Option<ExtSessionLockV1>,
    /// Confirmation to send once a frame without session content is shown.
    pending: Option<SessionLocker>,
    surfaces: Vec<(LockSurface, Output)>,
}

impl LockState {
    pub fn surface_for(&self, output: &Output) -> Option<&LockSurface> {
        self.surfaces.iter().find(|(_, o)| o == output).map(|(s, _)| s)
    }
}

/// Only the shell the compositor launched may lock the session.
pub fn can_lock(client: &smithay::reexports::wayland_server::Client) -> bool {
    client.get_data::<ClientState>().is_some_and(|c| c.privileged)
}

impl SessionLockHandler for Helix {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        let lock = confirmation.ext_session_lock().clone();
        if self.lock.locked && self.lock.current.as_ref().is_some_and(|c| c.is_alive()) {
            tracing::warn!("refusing a second lock while the session is locked");
            return; // Dropping the locker tells the client it failed.
        }
        if self.lock.locked {
            tracing::info!("lock client replaced (the previous one went away while locked)");
        } else {
            tracing::info!("locking the session");
        }
        self.lock.locked = true;
        self.lock.current = Some(lock);
        self.lock.surfaces.clear();
        self.lock.pending = Some(confirmation);
        // Nothing of the session may keep input.
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), 0);
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.unset_grab(self);
        keyboard.set_focus(self, Option::<super::focus::KeyboardFocusTarget>::None, SERIAL_COUNTER.next_serial());
        self.render_all_now();
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.lock = LockState::default();
        match self.top_window() {
            Some(top) => self.focus_window(&top),
            None => self.unfocus_all(),
        }
        self.render_all_now();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else { return };
        if let Some(mode) = output.current_mode() {
            let size = mode.size.to_logical(1);
            surface.with_pending_state(|state| {
                state.size = Some((size.w as u32, size.h as u32).into());
            });
            surface.send_configure();
        }
        let wl_surface = surface.wl_surface().clone();
        self.lock.surfaces.push((surface, output));
        // The (first) lock surface gets the keyboard.
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.current_focus().is_none() {
            keyboard.set_focus(self, Some(wl_surface.into()), SERIAL_COUNTER.next_serial());
        }
    }
}

impl Helix {
    /// A frame without session content has been shown: confirm the lock.
    pub fn lock_frame_shown(&mut self) {
        if let Some(locker) = self.lock.pending.take() {
            locker.lock();
            tracing::info!("session locked");
        }
    }

    /// While locked, the only surfaces that receive input.
    pub fn lock_surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.lock.surfaces.iter().find_map(|(surface, output)| {
            let geo = self.space.output_geometry(output)?;
            geo.to_f64()
                .contains(pos)
                .then(|| (surface.wl_surface().clone(), geo.loc.to_f64()))
        })
    }

    pub fn lock_send_frames(&self, output: &Output) {
        if let Some(surface) = self.lock.surface_for(output) {
            send_frames_surface_tree(
                surface.wl_surface(),
                output,
                self.start_time.elapsed(),
                Some(std::time::Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    fn render_all_now(&mut self) {
        // DRM renders on its own schedule; nested redraws continuously.
        self.render_all();
    }
}
