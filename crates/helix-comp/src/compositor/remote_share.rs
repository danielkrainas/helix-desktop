//! Remote desktop clipboard sharing and session keymaps (helix-desktop-spec
//! `org.gnome.Mutter.RemoteDesktop.md`, "Clipboard" and "Keymaps").

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use helix_comp::remote_desktop::{ClipboardRequest, KeymapRequest, RemoteError, RemoteReply, RemoteRequest, SessionKeymap};
use helix_comp::Event;
use smithay::input::keyboard::{xkb, Layout};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata, request_data_device_client_selection,
    set_data_device_selection,
};
use smithay::wayland::selection::SelectionTarget;

use super::{Helix, ServerSelection};

/// How long a paste waits for the session's `SelectionWrite`.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// A paste of a session's clipboard content, waiting for its data.
struct Transfer {
    session: u32,
    /// Where the app reads the data from.
    fd: OwnedFd,
    since: Instant,
}

/// A keymap a session set, while in effect.
#[derive(Debug, Clone)]
struct ActiveKeymap {
    session: u32,
    layouts: u32,
    lock: bool,
}

#[derive(Default)]
pub struct RemoteShare {
    /// Sessions sharing the clipboard.
    sharing: Vec<u32>,
    /// What the clipboard offers now.
    offered: Vec<String>,
    /// The session whose content is the clipboard.
    owner: Option<u32>,
    transfers: HashMap<u32, Transfer>,
    /// Serials written, waiting for `SelectionWriteDone`: their session.
    written: HashMap<u32, u32>,
    next_serial: u32,
    keymap: Option<ActiveKeymap>,
}

type Reply = Result<RemoteReply, RemoteError>;

fn failed(m: &str) -> RemoteError {
    RemoteError::Failed(m.to_owned())
}

fn invalid(m: &str) -> RemoteError {
    RemoteError::InvalidArgs(m.to_owned())
}

/// A pipe: (read end, write end).
fn pipe() -> Result<(OwnedFd, OwnedFd), RemoteError> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills both fds on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(failed("cannot make a pipe"));
    }
    // SAFETY: fresh fds we own.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl Helix {
    pub fn remote_request(&mut self, session: u32, request: RemoteRequest) -> Reply {
        match request {
            RemoteRequest::Clipboard(request) => self.clipboard_request(session, request),
            RemoteRequest::Keymap(request) => self.keymap_request(session, request),
        }
    }

    // ---- Clipboard -----------------------------------------------------------

    fn clipboard_request(&mut self, session: u32, request: ClipboardRequest) -> Reply {
        let sharing = self.remote_share.sharing.contains(&session);
        match request {
            ClipboardRequest::Enable(mime_types) => {
                if sharing {
                    return Err(failed("the clipboard is already shared"));
                }
                self.remote_share.sharing.push(session);
                if !mime_types.is_empty() {
                    self.set_remote_selection(session, mime_types);
                } else if !self.remote_share.offered.is_empty() && self.remote_share.owner != Some(session) {
                    self.send(Event::RemoteSelectionOwner { session, mime_types: self.remote_share.offered.clone(), is_owner: false });
                }
                Ok(RemoteReply::Done)
            }
            _ if !sharing => Err(failed("the clipboard is not shared")),
            ClipboardRequest::Disable => {
                self.stop_sharing(session);
                Ok(RemoteReply::Done)
            }
            ClipboardRequest::SetSelection(mime_types) => {
                self.set_remote_selection(session, mime_types);
                Ok(RemoteReply::Done)
            }
            ClipboardRequest::Write(serial) => {
                let share = &mut self.remote_share;
                match share.transfers.remove(&serial) {
                    Some(t) if t.session == session => {
                        share.written.insert(serial, session);
                        Ok(RemoteReply::Fd(t.fd))
                    }
                    Some(t) => {
                        share.transfers.insert(serial, t);
                        Err(invalid("unknown serial"))
                    }
                    None => Err(invalid("unknown serial")),
                }
            }
            ClipboardRequest::WriteDone(serial, success) => {
                let share = &mut self.remote_share;
                if share.written.get(&serial) == Some(&session) {
                    share.written.remove(&serial);
                    Ok(RemoteReply::Done)
                } else if share.transfers.get(&serial).is_some_and(|t| t.session == session) {
                    // Answered without writing: the app gets nothing.
                    share.transfers.remove(&serial);
                    tracing::debug!(serial, success, "clipboard transfer ended without data");
                    Ok(RemoteReply::Done)
                } else {
                    Err(invalid("unknown serial"))
                }
            }
            ClipboardRequest::Read(mime_type) => {
                if self.remote_share.owner == Some(session) {
                    return Err(failed("the clipboard is the session's own"));
                }
                if self.remote_share.offered.is_empty() {
                    return Err(failed("the clipboard is empty"));
                }
                if !self.remote_share.offered.contains(&mime_type) {
                    return Err(invalid("the clipboard does not offer that type"));
                }
                let (read, write) = pipe()?;
                self.read_clipboard(mime_type, write)?;
                Ok(RemoteReply::Fd(read))
            }
        }
    }

    /// Have the clipboard's owner write it, as `mime_type`, into `fd`.
    fn read_clipboard(&mut self, mime_type: String, fd: OwnedFd) -> Result<(), RemoteError> {
        let current = current_data_device_selection_userdata(&self.seat).map(|r| r.clone());
        match current.as_ref() {
            Some(ServerSelection::Image(png)) => {
                super::capture::serve_image(png.clone(), &mime_type, fd);
                Ok(())
            }
            Some(ServerSelection::Remote { session }) => {
                self.start_transfer(*session, mime_type, fd);
                Ok(())
            }
            Some(ServerSelection::X11) => {
                let handle = self.loop_handle.clone();
                let xwm = self.xwm.as_mut().ok_or_else(|| failed("no X11"))?;
                xwm.send_selection(SelectionTarget::Clipboard, mime_type, fd, handle).map_err(|e| failed(&e.to_string()))
            }
            None => request_data_device_client_selection(&self.seat, mime_type, fd).map_err(|e| failed(&e.to_string())),
        }
    }

    /// Session `session`'s content becomes the clipboard.
    fn set_remote_selection(&mut self, session: u32, mime_types: Vec<String>) {
        set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), ServerSelection::Remote { session });
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(SelectionTarget::Clipboard, Some(mime_types.clone())) {
                tracing::warn!(?err, "cannot offer the remote clipboard to X11");
            }
        }
        self.clipboard_changed(mime_types, Some(session));
    }

    /// An app pastes session `session`'s content as `mime_type` into `fd`.
    pub fn start_transfer(&mut self, session: u32, mime_type: String, fd: OwnedFd) {
        if !self.remote_share.sharing.contains(&session) {
            return;
        }
        let share = &mut self.remote_share;
        share.next_serial = share.next_serial.wrapping_add(1);
        let serial = share.next_serial;
        share.transfers.insert(serial, Transfer { session, fd, since: Instant::now() });
        self.send(Event::RemoteSelectionTransfer { session, mime_type, serial });
    }

    /// The clipboard now offers `mime_types` (empty: cleared), set by
    /// `owner` (a remote desktop session) or else an app.
    pub fn clipboard_changed(&mut self, mime_types: Vec<String>, owner: Option<u32>) {
        self.remote_share.offered = mime_types.clone();
        self.remote_share.owner = owner;
        for session in self.remote_share.sharing.clone() {
            self.send(Event::RemoteSelectionOwner { session, mime_types: mime_types.clone(), is_owner: owner == Some(session) });
        }
    }

    fn stop_sharing(&mut self, session: u32) {
        let share = &mut self.remote_share;
        share.sharing.retain(|s| *s != session);
        share.transfers.retain(|_, t| t.session != session);
        share.written.retain(|_, s| *s != session);
        let owned = matches!(
            current_data_device_selection_userdata(&self.seat).as_deref(),
            Some(ServerSelection::Remote { session: s }) if *s == session
        );
        if owned {
            clear_data_device_selection(&self.display_handle, &self.seat);
            if let Some(xwm) = self.xwm.as_mut() {
                let _ = xwm.new_selection(SelectionTarget::Clipboard, None);
            }
            self.clipboard_changed(Vec::new(), None);
        }
    }

    /// Every tick: drop pastes nobody answered.
    pub fn remote_share_tick(&mut self) {
        self.remote_share.transfers.retain(|serial, t| {
            let keep = t.since.elapsed() < TRANSFER_TIMEOUT;
            if !keep {
                tracing::info!(serial, "clipboard transfer not answered; dropped");
            }
            keep
        });
    }

    /// A remote desktop session ended.
    pub fn end_remote_share(&mut self, session: u32) {
        self.stop_sharing(session);
        if self.remote_share.keymap.as_ref().is_some_and(|k| k.session == session) {
            self.restore_configured_keymap();
        }
    }

    // ---- Keymaps -------------------------------------------------------------

    fn keymap_request(&mut self, session: u32, request: KeymapRequest) -> Reply {
        match request {
            KeymapRequest::Current => {
                let source = u32::from(self.remote_share.keymap.as_ref().is_some_and(|k| k.session == session));
                Ok(RemoteReply::Keymap { name: self.layout_name(), source })
            }
            KeymapRequest::Set(None) => {
                if self.remote_share.keymap.as_ref().is_some_and(|k| k.session == session) {
                    self.restore_configured_keymap();
                }
                Ok(RemoteReply::Done)
            }
            KeymapRequest::Set(Some(keymap)) => self.set_session_keymap(session, keymap).map(|_| RemoteReply::Done),
            KeymapRequest::LayoutIndex(index) => {
                let Some(active) = self.remote_share.keymap.clone().filter(|k| k.session == session) else {
                    return Err(failed("the keymap in effect is not the session's"));
                };
                if index >= active.layouts {
                    return Err(invalid("no such layout"));
                }
                let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
                keyboard.with_xkb_state(self, |mut ctx| ctx.set_layout(Layout(index)));
                self.keymap_changed();
                Ok(RemoteReply::Done)
            }
        }
    }

    fn set_session_keymap(&mut self, session: u32, keymap: SessionKeymap) -> Result<(), RemoteError> {
        let layouts = {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let compiled = xkb::Keymap::new_from_string(&context, keymap.text.clone(), xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
                .ok_or_else(|| invalid("the keymap does not compile"))?;
            compiled.num_layouts()
        };
        if keymap.layout >= layouts {
            return Err(invalid("no such layout"));
        }
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        keyboard.set_keymap_from_string(self, keymap.text).map_err(|_| invalid("the keymap does not compile"))?;
        keyboard.with_xkb_state(self, |mut ctx| ctx.set_layout(Layout(keymap.layout)));
        self.remote_share.keymap = Some(ActiveKeymap { session, layouts, lock: keymap.lock });
        tracing::info!(session, layouts, lock = keymap.lock, "remote desktop keymap set");
        self.keymap_changed();
        Ok(())
    }

    /// Back to the configured input sources.
    fn restore_configured_keymap(&mut self) {
        self.remote_share.keymap = None;
        // Start from the default keymap and apply the input sources again.
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        if let Err(err) = keyboard.set_xkb_config(self, smithay::input::keyboard::XkbConfig::default()) {
            tracing::warn!(?err, "cannot reset the keymap");
        }
        self.input_settings = None;
        self.apply_input_settings();
    }

    /// Whether a session's locked keymap holds off the input sources.
    pub fn keymap_locked(&self) -> bool {
        self.remote_share.keymap.as_ref().is_some_and(|k| k.lock)
    }

    /// The configured keymap replaced a session's unlocked one.
    pub fn configured_keymap_applied(&mut self) {
        if self.remote_share.keymap.take().is_some() {
            tracing::info!("remote desktop keymap replaced by the input sources");
        }
        self.keymap_changed();
    }

    fn keymap_changed(&mut self) {
        self.send(Event::KeymapChanged);
        self.refresh_eis_keyboards();
    }

    /// The XKB name of the active layout.
    fn layout_name(&mut self) -> String {
        let keyboard = self.seat.get_keyboard().expect("seat has a keyboard");
        keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            let layout = xkb.active_layout();
            // SAFETY: only read while the lock is held.
            let keymap = unsafe { xkb.keymap() };
            keymap.layout_get_name(layout.0).to_owned()
        })
    }
}
