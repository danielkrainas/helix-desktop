//! Copy, cut and paste in the shell's own text fields (helixdesktop/spec
//! `contracts/dbus/org.a11y.atspi.md`, "Text fields", and
//! `contracts/wayland/README.md`, "Shell text fields and the
//! clipboard"): the launcher's search, the run dialog and the prompts'
//! fields, and the lock screen's password. The shell takes part in the
//! Wayland clipboard through its seat's data device.
//!
//! The fields have no selection and type at their end, so Ctrl+C copies
//! a field's whole text, Ctrl+X cuts it all, Ctrl+V adds the clipboard's
//! text at the end. Assistive technologies copy, cut and paste a range
//! (`EditableText`). Password fields take a paste but are never copied or
//! cut.

use std::io::{Read, Write};

use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;

use super::*;

/// What the shell offers its copied text as, and reads a paste as (in
/// that order of preference).
const TEXT_TYPES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"];
/// The most read from a paste.
const PASTE_MAX: u64 = 1 << 20;
/// The middle pointer button: pastes the primary selection.
const BTN_MIDDLE: u32 = 0x112;

/// The type to read an offer of `types` as text, if any.
fn text_type(types: &[String]) -> Option<String> {
    TEXT_TYPES.iter().find(|t| types.iter().any(|m| m == *t)).map(|t| t.to_string())
}

/// The custom action ids AccessKit's AT-SPI adapter sends for
/// `EditableText.CopyText`, `CutText` and `PasteText` (Helix's vendored
/// `accesskit_atspi_common`, `CLIPBOARD_*`).
pub(super) const ATSPI_COPY: i32 = 0x4858_0001;
pub(super) const ATSPI_CUT: i32 = 0x4858_0002;
pub(super) const ATSPI_PASTE: i32 = 0x4858_0003;

/// A clipboard operation on a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    Copy,
    Cut,
    Paste,
}

impl Op {
    /// The operation a key is (Ctrl+C/X/V, Ctrl+Insert, Shift+Insert,
    /// Shift+Delete), if it is one.
    pub(super) fn of_key(keysym: Keysym, ctrl: bool, shift: bool, alt: bool) -> Option<Op> {
        if alt {
            return None;
        }
        match keysym {
            Keysym::c | Keysym::C if ctrl => Some(Op::Copy),
            Keysym::x | Keysym::X if ctrl => Some(Op::Cut),
            Keysym::v | Keysym::V if ctrl => Some(Op::Paste),
            Keysym::Insert if ctrl && !shift => Some(Op::Copy),
            Keysym::Insert if shift && !ctrl => Some(Op::Paste),
            Keysym::Delete if shift && !ctrl => Some(Op::Cut),
            _ => None,
        }
    }

    /// The operation an AT-SPI custom action is.
    pub(super) fn of_action(id: i32) -> Option<Op> {
        match id {
            ATSPI_COPY => Some(Op::Copy),
            ATSPI_CUT => Some(Op::Cut),
            ATSPI_PASTE => Some(Op::Paste),
            _ => None,
        }
    }
}

/// The shell's side of the clipboard.
#[derive(Default)]
pub(super) struct Clipboard {
    /// What the shell copied, while it owns the clipboard.
    source: Option<(CopyPasteSource, String)>,
    /// The last input serial (for taking the clipboard).
    pub serial: u32,
}

/// `text` with characters `range` (counted in characters, clamped)
/// replaced by `with`.
pub(super) fn replaced(text: &str, (start, end): (usize, usize), with: &str) -> String {
    let len = text.chars().count();
    let end = end.min(len);
    let start = start.min(end);
    let before: String = text.chars().take(start).collect();
    let after: String = text.chars().skip(end).collect();
    format!("{before}{with}{after}")
}

/// What a clipboard operation on a field does.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Text to put on the clipboard.
    pub copy: Option<String>,
    /// The field's new text (a cut).
    pub text: Option<String>,
    /// Where the clipboard's text goes (a paste), in characters.
    pub paste_at: Option<usize>,
    /// Copying or cutting a password: nothing happens.
    pub refused: bool,
}

/// Operation `op` on a field holding `text` (a password or not):
/// `range` is the characters copied or cut, or where a paste goes
/// (`None`: the end). Ranges are clamped to the text.
pub(super) fn plan(op: Op, text: &str, password: bool, range: Option<(usize, usize)>) -> Plan {
    let len = text.chars().count();
    let (start, end) = range.map_or((len, len), |(s, e)| (s.min(len), e.min(len)));
    let start = start.min(end);
    match op {
        Op::Copy | Op::Cut if password => Plan { refused: true, ..Plan::default() },
        Op::Copy | Op::Cut => {
            let part: String = text.chars().skip(start).take(end - start).collect();
            Plan {
                copy: (!part.is_empty()).then_some(part),
                text: (op == Op::Cut && start < end).then(|| replaced(text, (start, end), "")),
                ..Plan::default()
            }
        }
        Op::Paste => Plan { paste_at: Some(start), ..Plan::default() },
    }
}

/// Pasted text as a one-line field takes it: line breaks become spaces,
/// other control characters are dropped.
pub(super) fn one_line(text: &str) -> String {
    let joined = text.trim_end_matches(['\r', '\n']).replace("\r\n", " ").replace(['\n', '\r'], " ");
    joined.chars().filter(|c| !c.is_control()).collect()
}

impl Ui {
    /// The text field the keyboard types into now (its surface and
    /// accessible id), following the order keys are handed out in.
    pub(super) fn focused_text_field(&self) -> Option<(wl_surface::WlSurface, String)> {
        if self.lock.session.is_some() {
            let view = self.lock_primary()?;
            return Some((view.surface.wl_surface().clone(), "password".into()));
        }
        if self.window_menu.is_some() || self.picker.is_some() {
            return None;
        }
        if let Some(dialog) = self.dialog.as_ref() {
            return self.prompt_focused_field().or_else(|| self.files_focused_field()).map(|id| (dialog.layer.wl_surface().clone(), id));
        }
        let elsewhere = self.switcher.is_some()
            || self.banner.as_ref().is_some_and(|b| b.focused)
            || self.overview.is_some()
            || self.power_menu.is_some()
            || self.notify.view.is_some()
            || self.panel_focus.is_some()
            || self.dock_keyboard_view().is_some()
            || self.break_screen.is_some()
            || self.panel_editor.is_some()
            || self.extensions_dialog.is_some();
        if elsewhere {
            return None;
        }
        self.launcher.as_ref().map(|v| (v.layer.wl_surface().clone(), "search".to_owned()))
    }

    /// Field `id` on `surface`: its text, and whether it is a password.
    fn field_text(&self, surface: &wl_surface::WlSurface, id: &str) -> Option<(String, bool)> {
        if self.lock.session.is_some() {
            let on_lock = self.lock_primary().is_some_and(|v| v.surface.wl_surface() == surface);
            return (on_lock && id == "password").then(|| (self.lock.password.clone(), true));
        }
        if let Some(view) = self.launcher.as_ref().filter(|v| v.layer.wl_surface() == surface) {
            return (id == "search").then(|| (view.model.query.clone(), false));
        }
        if self.dialog.as_ref().is_some_and(|d| d.layer.wl_surface() == surface) {
            return self.prompt_field_text(id).or_else(|| self.files_field_text(id));
        }
        None
    }

    /// A key typed into a text field: true when it was a clipboard key
    /// (and was handled).
    pub(super) fn field_clipboard_key(&mut self, event: &KeyEvent) -> bool {
        let Some(op) = Op::of_key(event.keysym, self.mods.ctrl, self.mods.shift, self.mods.alt) else { return false };
        let Some((surface, id)) = self.focused_text_field() else { return false };
        // No selection: the whole text, and pastes go at the end.
        let range = match op {
            Op::Paste => None,
            Op::Copy | Op::Cut => Some((0, usize::MAX)),
        };
        self.field_clipboard(&surface, &id, op, range);
        true
    }

    /// Copy, cut or paste on field `id` of `surface`: `range` is the
    /// characters copied or cut, or where a paste goes (`None`: the end).
    pub(super) fn field_clipboard(&mut self, surface: &wl_surface::WlSurface, id: &str, op: Op, range: Option<(usize, usize)>) {
        let Some((mut text, password)) = self.field_text(surface, id) else { return };
        let plan = plan(op, &text, password, range);
        if password {
            wipe(&mut text);
        }
        if plan.refused {
            tracing::debug!(id, "a password field is not copied or cut");
        }
        if let Some(part) = plan.copy {
            self.clipboard_copy(part);
        }
        if let Some(rest) = plan.text {
            self.atspi_set_text(surface, id, &rest, None);
        }
        if let Some(at) = plan.paste_at {
            self.clipboard_paste(surface.clone(), id.to_owned(), at);
        }
    }

    /// Put `text` on the clipboard.
    fn clipboard_copy(&mut self, text: String) {
        let (Some(manager), Some(device)) = (self.data_devices.as_ref(), self.data_device.as_ref()) else {
            tracing::warn!("no data device; cannot copy");
            return;
        };
        let source = manager.create_copy_paste_source(&self.qh, TEXT_TYPES);
        source.set_selection(device, self.clipboard.serial);
        self.clipboard.source = Some((source, text));
    }

    /// Paste the clipboard's text into field `id` of `surface` at
    /// character `at`.
    fn clipboard_paste(&mut self, surface: wl_surface::WlSurface, id: String, at: usize) {
        // The shell's own copy: no need to go through a pipe.
        if let Some((_, text)) = self.clipboard.source.as_ref() {
            let text = text.clone();
            self.insert_pasted(&surface, &id, at, &text);
            return;
        }
        let Some(offer) = self.data_device.as_ref().and_then(|d| d.data().selection_offer()) else { return };
        let Some(mime) = offer.with_mime_types(text_type) else {
            tracing::debug!("nothing to paste as text");
            return;
        };
        match offer.receive(mime) {
            Ok(pipe) => self.paste_from(pipe, surface, id, at),
            Err(err) => tracing::warn!(%err, "cannot read the clipboard"),
        }
    }

    /// A middle click on the text field the keyboard types into pastes
    /// the primary selection at its end (fields have no selection to
    /// place it in). True when it was such a click.
    pub(super) fn field_middle_click(&mut self, event: &PointerEvent) -> bool {
        let PointerEventKind::Press { button: BTN_MIDDLE, serial, .. } = event.kind else { return false };
        let Some((surface, id)) = self.focused_text_field().filter(|(s, _)| *s == event.surface) else { return false };
        self.clipboard.serial = serial;
        let Some((text, password)) = self.field_text(&surface, &id) else { return true };
        let at = text.chars().count();
        let mut text = text;
        if password {
            wipe(&mut text);
        }
        let Some(offer) = self.primary_device.as_ref().and_then(|d| d.data().selection_offer()) else { return true };
        let Some(mime) = offer.with_mime_types(text_type) else { return true };
        match offer.receive(mime) {
            Ok(pipe) => self.paste_from(pipe, surface, id, at),
            Err(err) => tracing::warn!(%err, "cannot read the primary selection"),
        }
        true
    }

    /// Read a paste from `pipe` off the UI thread (the source may be slow
    /// or never close); put it in field `id` of `surface` at `at` back on
    /// it, if the field is still there.
    fn paste_from(&mut self, pipe: smithay_client_toolkit::data_device_manager::ReadPipe, surface: wl_surface::WlSurface, id: String, at: usize) {
        let Some(handle) = self.loop_handle.clone() else { return };
        let (tx, rx) = channel::channel::<String>();
        let _ = handle.insert_source(rx, move |event, _, ui: &mut Ui| {
            if let channel::Event::Msg(text) = event {
                ui.insert_pasted(&surface, &id, at, &text);
            }
        });
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(PASTE_MAX).read_to_end(&mut bytes);
            let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
        });
    }

    fn insert_pasted(&mut self, surface: &wl_surface::WlSurface, id: &str, at: usize, pasted: &str) {
        let pasted = one_line(pasted);
        if pasted.is_empty() {
            return;
        }
        let Some((mut text, password)) = self.field_text(surface, id) else { return };
        let mut new = replaced(&text, (at, at), &pasted);
        self.atspi_set_text(surface, id, &new, None);
        if password {
            wipe(&mut text);
            wipe(&mut new);
        }
    }

    /// The compositor asks for the text the shell copied.
    pub(super) fn clipboard_send(&mut self, source: &WlDataSource, mime: &str, pipe: WritePipe) {
        let Some((_, text)) = self.clipboard.source.as_ref().filter(|(s, _)| s.inner() == source) else { return };
        if !TEXT_TYPES.contains(&mime) {
            return;
        }
        let text = text.clone();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            let _ = pipe.write_all(text.as_bytes());
        });
    }

    /// Something else took the clipboard.
    pub(super) fn clipboard_cancelled(&mut self, source: &WlDataSource) {
        if self.clipboard.source.as_ref().is_some_and(|(s, _)| s.inner() == source) {
            self.clipboard.source = None;
        }
    }
}

/// The shell only reads the primary selection; it offers none.
impl smithay_client_toolkit::primary_selection::device::PrimarySelectionDeviceHandler for Ui {
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1) {}
}

impl smithay_client_toolkit::primary_selection::selection::PrimarySelectionSourceHandler for Ui {
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, _: String, _: WritePipe) {}
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(Op::of_key(Keysym::c, true, false, false), Some(Op::Copy));
        assert_eq!(Op::of_key(Keysym::X, true, true, false), Some(Op::Cut));
        assert_eq!(Op::of_key(Keysym::v, true, false, false), Some(Op::Paste));
        assert_eq!(Op::of_key(Keysym::Insert, false, true, false), Some(Op::Paste));
        assert_eq!(Op::of_key(Keysym::Insert, true, false, false), Some(Op::Copy));
        assert_eq!(Op::of_key(Keysym::Delete, false, true, false), Some(Op::Cut));
        assert_eq!(Op::of_key(Keysym::v, false, false, false), None, "plain typing");
        assert_eq!(Op::of_key(Keysym::v, true, false, true), None, "Ctrl+Alt+V is not a paste");
    }

    #[test]
    fn plans() {
        // The keys: no selection, so the whole text; pastes at the end.
        assert_eq!(plan(Op::Copy, "firefox", false, Some((0, usize::MAX))), Plan { copy: Some("firefox".into()), ..Plan::default() });
        assert_eq!(plan(Op::Cut, "firefox", false, Some((0, usize::MAX))), Plan { copy: Some("firefox".into()), text: Some(String::new()), ..Plan::default() });
        assert_eq!(plan(Op::Paste, "fire", false, None), Plan { paste_at: Some(4), ..Plan::default() });
        // Ranges (assistive technologies).
        assert_eq!(plan(Op::Cut, "héllo", false, Some((1, 3))), Plan { copy: Some("él".into()), text: Some("hlo".into()), ..Plan::default() });
        assert_eq!(plan(Op::Paste, "héllo", false, Some((2, 2))), Plan { paste_at: Some(2), ..Plan::default() });
        assert_eq!(plan(Op::Copy, "", false, Some((0, usize::MAX))), Plan::default(), "nothing to copy");
        // Passwords take a paste but are never copied or cut.
        assert_eq!(plan(Op::Copy, "hunter2", true, Some((0, 7))), Plan { refused: true, ..Plan::default() });
        assert_eq!(plan(Op::Cut, "hunter2", true, Some((0, 7))), Plan { refused: true, ..Plan::default() });
        assert_eq!(plan(Op::Paste, "hunter2", true, None), Plan { paste_at: Some(7), ..Plan::default() });
    }

    #[test]
    fn editing() {
        assert_eq!(replaced("héllo", (1, 3), ""), "hlo");
        assert_eq!(replaced("héllo", (5, 5), " world"), "héllo world");
        assert_eq!(replaced("ab", (9, 12), "c"), "abc", "clamped to the end");
        assert_eq!(one_line("two\nlines\n"), "two lines");
        assert_eq!(one_line("a\r\nb\tc"), "a bc");
    }
}

