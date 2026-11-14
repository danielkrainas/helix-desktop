//! The prompt dialog (helix-desktop-spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`, "Prompt dialog").

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::shell::WaylandSurface;
use tokio::sync::oneshot;

use super::{baseline, caret, label, text_field, physical, wipe, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, ACCENT, BANNER_BG, BANNER_BORDER, DIM_FG, FG, FIELD_BG, SCRIM, SHARING};
use crate::auth::Secret;
use crate::prompt::{Answer, Prompt, Request};

const LIST_ROW: i32 = 32;
const LIST_SHOWN: usize = 10;
/// The places column's width, and the most places shown.
const SIDEBAR_W: i32 = 180;
const PLACES_SHOWN: usize = 14;

/// A prompt on screen.
pub(super) struct PromptView {
    prompt: Prompt,
    reply: Option<oneshot::Sender<Answer>>,
    /// The password and confirmation fields' text.
    fields: [String; 2],
    /// The plain text field's text.
    entry: String,
    /// 0, 1: a password field; 2: the check box; 3: the text field.
    focus: usize,
    choice: bool,
    /// The list row picked (with `multiple`: the current row).
    selected: usize,
    /// With `multiple`: which rows are picked.
    picked: Vec<bool>,
    /// Answered; waiting for the next prompt or the end.
    busy: bool,
    /// The place clicked.
    place: Option<usize>,
    /// Previews by file: asked for (`None` until decoded, or when it
    /// cannot be shown).
    previews: std::collections::HashMap<String, Option<crate::Preview>>,
    /// The password fields shown as text ("Show").
    shown: [bool; 2],
    /// Whether "Show" is offered (`disable-show-password` off).
    can_show: bool,
    /// Where Up/Down are in the history (None: the text typed).
    history_at: Option<usize>,
    /// Command names for Tab, read at the first Tab.
    commands: Option<Vec<String>>,
    /// The extra check boxes' states.
    checks: Vec<bool>,
}

/// Focus numbers of the extra check boxes (from here up).
const CHECK_FOCUS: usize = 10;

/// The width of a password field's "Show" button.
const SHOW_W: i32 = 56;

/// The preview area's height.
const PREVIEW_H: i32 = 160;

impl PromptView {
    fn new(request: Request) -> Self {
        let choice = request.prompt.choice.as_ref().is_some_and(|c| c.1);
        let entry = request.prompt.entry.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
        let focus = match (&request.prompt.entry, &request.prompt.password) {
            (Some(_), Some(_)) if !entry.is_empty() => 0,
            (Some(_), _) => 3,
            (None, Some(_)) => 0,
            (None, None) => 2,
        };
        let selected = request.prompt.selected.min(request.prompt.list.len().saturating_sub(1));
        let picked = vec![false; request.prompt.list.len()];
        let checks = request.prompt.checks.iter().map(|(_, on)| *on).collect();
        Self { prompt: request.prompt, reply: Some(request.reply), fields: Default::default(), entry, focus, choice, selected, picked, busy: false, place: None, previews: Default::default(), shown: [false; 2], can_show: !crate::locked_down("disable-show-password"), history_at: None, commands: None, checks }
    }

    /// Answer "cancel" if nobody answered yet.
    pub(super) fn cancel(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Answer::Cancel);
        }
    }

    fn focusable(&self) -> Vec<usize> {
        let mut out = Vec::new();
        if self.prompt.entry.is_some() {
            out.push(3);
        }
        if self.prompt.password.is_some() {
            out.push(0);
        }
        if self.prompt.confirm.is_some() {
            out.push(1);
        }
        if self.prompt.choice.is_some() {
            out.push(2);
        }
        out.extend((0..self.prompt.checks.len()).map(|i| CHECK_FOCUS + i));
        out
    }
}

impl Drop for PromptView {
    fn drop(&mut self) {
        self.cancel();
        for f in &mut self.fields {
            wipe(f);
        }
    }
}

impl Ui {
    /// A new prompt, or the next one of the same flow.
    pub(super) fn show_prompt(&mut self, request: Request) {
        if self.lock.session.is_some() {
            let _ = request.reply.send(Answer::Cancel);
            return;
        }
        let view = PromptView::new(request);
        if let Some(dialog) = self.dialog.as_mut() {
            if let DialogKind::Prompt(old) = &mut dialog.state.kind {
                old.cancel();
                dialog.state.kind = DialogKind::Prompt(Box::new(view));
                self.draw_dialog();
                return;
            }
        }
        self.show_dialog(DialogState { kind: DialogKind::Prompt(Box::new(view)), remaining: 0 });
    }

    /// The flow is over: close the prompt.
    pub(super) fn prompt_done(&mut self) {
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::Prompt(_))) {
            self.dialog = None;
        }
    }

    /// A preview was decoded (or cannot be shown).
    pub(super) fn prompt_preview(&mut self, path: String, image: Option<crate::Preview>) {
        let Some(view) = self.prompt_view() else { return };
        if !view.previews.contains_key(&path) {
            return;
        }
        view.previews.insert(path, image);
        self.draw_dialog();
    }

    /// New rows for the prompt shown (the app chooser's choices changed).
    pub(super) fn prompt_rows(&mut self, rows: Vec<String>) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy {
            return;
        }
        if rows.is_empty() {
            self.prompt_answer(false);
            return;
        }
        let current = view.prompt.list.get(view.selected).cloned();
        view.selected = current.and_then(|c| rows.iter().position(|r| *r == c)).unwrap_or(0);
        view.picked = vec![false; rows.len()];
        view.prompt.list = rows;
        self.draw_dialog();
    }

    fn prompt_view(&mut self) -> Option<&mut PromptView> {
        match &mut self.dialog.as_mut()?.state.kind {
            DialogKind::Prompt(view) => Some(view),
            _ => None,
        }
    }

    /// Continue (`true`) or cancel.
    pub(super) fn prompt_answer(&mut self, go: bool) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy {
            return;
        }
        if !go {
            view.cancel();
            self.dialog = None;
            return;
        }
        let take = |s: &mut String| Secret::new(std::mem::take(s));
        let password = view.prompt.password.is_some().then(|| take(&mut view.fields[0]));
        let confirm = view.prompt.confirm.is_some().then(|| take(&mut view.fields[1]));
        let entry = view.prompt.entry.is_some().then(|| view.entry.clone());
        let picked = view.picked.iter().enumerate().filter(|(_, p)| **p).map(|(i, _)| i).collect();
        let answer = Answer::Continue { entry, password, confirm, choice: view.choice, checks: view.checks.clone(), selected: view.selected, picked, place: view.place };
        if let Some(reply) = view.reply.take() {
            let _ = reply.send(answer);
        }
        view.busy = true;
        self.draw_dialog();
    }

    /// A place was clicked: answer with it.
    pub(super) fn prompt_place(&mut self, place: usize) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy || place >= view.prompt.places.len() {
            return;
        }
        view.place = Some(place);
        self.prompt_answer(true);
    }

    /// A list row was clicked.
    pub(super) fn prompt_pick(&mut self, row: usize) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy || row >= view.prompt.list.len() {
            return;
        }
        let pickable = view.prompt.multiple && !view.prompt.unpickable.contains(&row);
        // A second click on the current row answers (like a double click).
        let again = view.prompt.activate_rows && !pickable && view.selected == row;
        view.selected = row;
        if pickable {
            view.picked[row] = !view.picked[row];
        }
        if again {
            return self.prompt_answer(true);
        }
        self.draw_dialog();
    }

    /// A field was clicked.
    pub(super) fn prompt_focus(&mut self, field: usize) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy || !view.focusable().contains(&field) {
            return;
        }
        view.focus = field;
        self.draw_dialog();
    }

    /// "Show" / "Hide" beside password field `index`.
    pub(super) fn prompt_show_password(&mut self, index: usize) {
        let Some(view) = self.prompt_view() else { return };
        if !view.can_show || index > 1 {
            return;
        }
        view.shown[index] = !view.shown[index];
        self.draw_dialog();
    }

    /// Extra check box `i` clicked.
    pub(super) fn prompt_check(&mut self, i: usize) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy || i >= view.checks.len() {
            return;
        }
        view.checks[i] = !view.checks[i];
        view.focus = CHECK_FOCUS + i;
        self.draw_dialog();
    }

    pub(super) fn prompt_toggle(&mut self) {
        let Some(view) = self.prompt_view() else { return };
        if view.busy || view.prompt.choice.is_none() {
            return;
        }
        view.choice = !view.choice;
        self.draw_dialog();
    }

    pub(super) fn prompt_key(&mut self, event: &KeyEvent) {
        match event.keysym {
            Keysym::Escape => return self.prompt_answer(false),
            Keysym::Return | Keysym::KP_Enter => return self.prompt_answer(true),
            _ => {}
        }
        let titles: Vec<String> = self.ext.commands.iter().map(|c| c.title.clone()).collect();
        let Some(view) = self.prompt_view() else { return };
        if view.busy {
            return;
        }
        match event.keysym {
            Keysym::Tab if view.focus == 3 && view.prompt.complete_commands => {
                // Extension commands' titles first, then programs.
                match crate::run_dialog::complete_title(&view.entry, &titles) {
                    Some(done) => view.entry = done,
                    None => {
                        let commands = view.commands.get_or_insert_with(crate::run_dialog::path_commands);
                        view.entry = crate::run_dialog::complete(&view.entry, commands);
                    }
                }
            }
            Keysym::Up if view.focus == 3 && !view.prompt.history.is_empty() => {
                let at = view.history_at.map_or(view.prompt.history.len() - 1, |i| i.saturating_sub(1));
                view.history_at = Some(at);
                view.entry = view.prompt.history[at].clone();
            }
            Keysym::Down if view.focus == 3 && !view.prompt.history.is_empty() => match view.history_at {
                Some(i) if i + 1 < view.prompt.history.len() => {
                    view.history_at = Some(i + 1);
                    view.entry = view.prompt.history[i + 1].clone();
                }
                Some(_) => {
                    view.history_at = None;
                    view.entry.clear();
                }
                None => {}
            },
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                let order = view.focusable();
                if let Some(i) = order.iter().position(|f| *f == view.focus) {
                    let back = event.keysym == Keysym::ISO_Left_Tab;
                    let next = if back { (i + order.len() - 1) % order.len() } else { (i + 1) % order.len() };
                    view.focus = order[next];
                }
            }
            Keysym::Down if !view.prompt.list.is_empty() => view.selected = (view.selected + 1).min(view.prompt.list.len() - 1),
            Keysym::Up if !view.prompt.list.is_empty() => view.selected = view.selected.saturating_sub(1),
            Keysym::space if view.focus == 2 && view.prompt.choice.is_none() && view.prompt.multiple && !view.prompt.list.is_empty() => {
                let row = view.selected;
                if !view.prompt.unpickable.contains(&row) {
                    view.picked[row] = !view.picked[row];
                }
            }
            Keysym::space if view.focus == 2 => {
                view.choice = !view.choice;
            }
            Keysym::space if view.focus >= CHECK_FOCUS && view.focus - CHECK_FOCUS < view.checks.len() => {
                let i = view.focus - CHECK_FOCUS;
                view.checks[i] = !view.checks[i];
            }
            Keysym::BackSpace if view.focus < 2 => {
                view.fields[view.focus].pop();
            }
            Keysym::BackSpace if view.focus == 3 => {
                view.entry.pop();
            }
            _ if view.focus < 2 || view.focus == 3 => {
                if let Some(text) = &event.utf8 {
                    let field = if view.focus == 3 { &mut view.entry } else { &mut view.fields[view.focus] };
                    field.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            _ => {}
        }
        self.draw_dialog();
    }

    pub(super) fn paint_prompt(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::Prompt(view) = &mut overlay.state.kind else { return };
        // The selected file's preview, decoded once (off this thread).
        if let Some(path) = view.prompt.previews.get(view.selected).filter(|p| !p.is_empty()) {
            if !view.previews.contains_key(path) {
                view.previews.insert(path.clone(), None);
                let _ = self.requests.send(crate::UiRequest::Preview(path.clone()));
            }
        }
        let view = &**view;
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let p = &view.prompt;
        let side = if p.places.is_empty() { 0 } else { SIDEBAR_W };
        let bw = (440 + side).min(w as i32 - 40);
        let inner = (bw - 48) as f32;
        let wrap = |s: &str, px: f32| -> Vec<String> {
            if s.is_empty() {
                Vec::new()
            } else {
                text.map_or_else(|| vec![s.to_owned()], |t| t.wrap(s, px, inner, 4))
            }
        };
        let message = wrap(&p.message, 14.0);
        let description = wrap(&p.description, 13.0);
        let fields = usize::from(p.entry.is_some()) + usize::from(p.password.is_some()) + usize::from(p.confirm.is_some());
        // The picture: at most the inner width and 220 high.
        let picture = p.picture.as_deref().filter(|i| i.width > 0 && i.height > 0).map(|i| {
            let k = (f64::from(inner) / i.width as f64).min(220.0 / i.height as f64);
            (i, (i.width as f64 * k).round(), (i.height as f64 * k).round())
        });
        let picture_h = picture.map_or(0, |(_, _, ph)| ph as i32 + 12);
        // At most LIST_SHOWN rows, scrolled to keep the selection in view.
        let shown = p.list.len().min(LIST_SHOWN);
        let first = view.selected.saturating_sub(LIST_SHOWN - 1).min(p.list.len().saturating_sub(shown));
        let places_shown = p.places.len().min(PLACES_SHOWN);
        let list_rows = shown.max(places_shown);
        let list_h = if list_rows > 0 { list_rows as i32 * LIST_ROW + 8 } else { 0 };
        // An icon above the title.
        let icon = p.icon.as_deref().filter(|i| i.width > 0 && i.height > 0);
        let top = if icon.is_some() { 60 } else { 0 };
        let preview_h = if p.previews.iter().any(|f| !f.is_empty()) { PREVIEW_H + 12 } else { 0 };
        let bh = preview_h + top + 70 + message.len() as i32 * 19 + description.len() as i32 * 17 + 8 + picture_h + list_h + fields as i32 * 62 + if p.choice.is_some() { 34 } else { 0 } + p.checks.len() as i32 * 34 + 26 + 64;
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 3;
        canvas.rect(bx - 1, by - 1, bw + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
        if let Some(icon) = icon {
            canvas.image(f64::from(bx + (bw - 48) / 2), f64::from(by + 20), 48.0, 48.0, icon);
        }
        // Below the icon, laid out as without it.
        let (by, bh) = (by + top, bh - top);
        label(text, &mut canvas, &p.title, center(&p.title, 20.0), (by + 44) as f32, 20.0, FG, inner);
        let mut y = by + 76;
        for line in &message {
            label(text, &mut canvas, line, center(line, 14.0), y as f32, 14.0, FG, inner);
            y += 19;
        }
        for line in &description {
            label(text, &mut canvas, line, center(line, 13.0), y as f32, 13.0, DIM_FG, inner);
            y += 17;
        }
        y += 8;
        if let Some((image, pw, ph)) = picture {
            canvas.image(f64::from(bx) + (f64::from(bw) - pw) / 2.0, f64::from(y), pw, ph, image);
            y += picture_h;
        }
        let mut hits = Vec::new();
        // The places, left of the list.
        for (i, name) in p.places.iter().enumerate().take(places_shown) {
            let r = Rect { x: bx + 24, y: y + i as i32 * LIST_ROW, w: side - 12, h: LIST_ROW };
            if p.place_current == Some(i) {
                canvas.rect(r.x, r.y, r.w, r.h, FIELD_BG);
                canvas.rect(r.x, r.y + 6, 3, r.h - 12, ACCENT);
            }
            label(text, &mut canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, (r.w - 16) as f32);
            hits.push((r, OverlayHit::Place(i)));
        }
        if shown > 0 {
            canvas.rect(bx + 24 + side, y, bw - 48 - side, shown as i32 * LIST_ROW, FIELD_BG);
            for (i, name) in p.list.iter().enumerate().skip(first).take(shown) {
                let r = Rect { x: bx + 24 + side, y: y + (i - first) as i32 * LIST_ROW, w: bw - 48 - side, h: LIST_ROW };
                if p.multiple {
                    if view.picked[i] {
                        canvas.rect(r.x, r.y, r.w, r.h, ACCENT);
                    }
                    if i == view.selected {
                        // The current row: an outline.
                        for (x, y, w, h) in [(r.x, r.y, r.w, 1), (r.x, r.y + r.h - 1, r.w, 1), (r.x, r.y, 1, r.h), (r.x + r.w - 1, r.y, 1, r.h)] {
                            canvas.rect(x, y, w, h, FG);
                        }
                    }
                } else if i == view.selected {
                    canvas.rect(r.x, r.y, r.w, r.h, ACCENT);
                }
                label(text, &mut canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, (r.w - 24) as f32);
                hits.push((r, OverlayHit::Row(i)));
            }
        }
        y += list_h;
        if preview_h > 0 {
            let area = Rect { x: bx + 24, y: y + 4, w: bw - 48, h: PREVIEW_H };
            canvas.rect(area.x, area.y, area.w, area.h, FIELD_BG);
            let preview = p.previews.get(view.selected).and_then(|f| view.previews.get(f)).and_then(|i| i.as_ref());
            let image = match preview {
                Some(crate::Preview::Image(image)) => Some(&**image),
                _ => None,
            };
            if let Some(crate::Preview::Text(lines)) = preview {
                // A text file's first lines, small.
                for (i, line) in lines.iter().enumerate() {
                    label(text, &mut canvas, line, (area.x + 10) as f32, (area.y + 18 + i as i32 * 16) as f32, 12.0, FG, (area.w - 20) as f32);
                }
            } else if let Some(image) = image.filter(|i| i.width > 0 && i.height > 0) {
                let k = (f64::from(area.w) / image.width as f64).min(f64::from(area.h) / image.height as f64).min(1.0);
                let (iw, ih) = (image.width as f64 * k, image.height as f64 * k);
                canvas.image(f64::from(area.x) + (f64::from(area.w) - iw) / 2.0, f64::from(area.y) + (f64::from(area.h) - ih) / 2.0, iw, ih, image);
            } else {
                let none = "No preview";
                let tw = text.map_or(0.0, |t| t.width(none, 13.0));
                label(text, &mut canvas, none, area.x as f32 + (area.w as f32 - tw) / 2.0, baseline(text, area.y, area.h, 13.0), 13.0, DIM_FG, inner);
            }
            y += preview_h;
        }
        if let Some((name, _)) = &p.entry {
            label(text, &mut canvas, name, (bx + 24) as f32, (y + 14) as f32, 13.0, DIM_FG, inner);
            let r = Rect { x: bx + 24, y: y + 22, w: bw - 48, h: 34 };
            let focused = view.focus == 3 && !view.busy;
            text_field(&mut canvas, r, focused);
            // The end of the text when it does not fit, and a caret.
            let room = (r.w - 24) as f32;
            let mut shown = view.entry.as_str();
            while text.is_some_and(|t| t.width(shown, 14.0) > room) && !shown.is_empty() {
                let next = shown.char_indices().nth(1).map_or(shown.len(), |(i, _)| i);
                shown = &shown[next..];
            }
            label(text, &mut canvas, shown, (r.x + 10) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, room);
            if focused {
                caret(&mut canvas, r, r.x + 10 + text.map_or(0.0, |t| t.width(shown, 14.0)) as i32 + 1, 14.0);
            }
            hits.push((r, OverlayHit::Field(3)));
            y += 62;
        }
        for (index, field) in [&p.password, &p.confirm].into_iter().enumerate() {
            let Some(name) = field else { continue };
            label(text, &mut canvas, name, (bx + 24) as f32, (y + 14) as f32, 13.0, DIM_FG, inner);
            let r = Rect { x: bx + 24, y: y + 22, w: bw - 48, h: 34 };
            let focused = view.focus == index && !view.busy;
            text_field(&mut canvas, r, focused);
            let show_w = if view.can_show { SHOW_W } else { 0 };
            let end = if view.shown[index] {
                let room = (r.w - 24 - show_w) as f32;
                label(text, &mut canvas, &view.fields[index], (r.x + 10) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, room);
                r.x + 10 + text.map_or(0.0, |t| t.width(&view.fields[index], 14.0).min(room)) as i32 + 1
            } else {
                // One dot per character (drawn: not every font has a bullet).
                let count = view.fields[index].chars().count().min(((r.w - 28 - show_w) / 12) as usize);
                for i in 0..count {
                    canvas.circle(f64::from(r.x + 16 + i as i32 * 12), f64::from(r.y + r.h / 2), 4.0, FG);
                }
                r.x + 10 + count as i32 * 12
            };
            if focused {
                caret(&mut canvas, r, end, 14.0);
            }
            if view.can_show {
                let b = Rect { x: r.x + r.w - SHOW_W, y: r.y, w: SHOW_W, h: r.h };
                let s = if view.shown[index] { "Hide" } else { "Show" };
                let lw = text.map_or(0.0, |t| t.width(s, 13.0));
                label(text, &mut canvas, s, b.x as f32 + (b.w as f32 - lw) / 2.0, baseline(text, b.y, b.h, 13.0), 13.0, DIM_FG, b.w as f32);
                hits.push((b, OverlayHit::ShowPassword(index)));
            }
            hits.push((r, OverlayHit::Field(index)));
            y += 62;
        }
        if let Some((name, _)) = &p.choice {
            let r = Rect { x: bx + 24, y, w: bw - 48, h: 26 };
            // Outlined, so an empty box shows.
            canvas.rect(r.x - 1, r.y + 3, 20, 20, DIM_FG);
            canvas.rect(r.x, r.y + 4, 18, 18, if view.choice { ACCENT } else { FIELD_BG });
            if view.focus == 2 {
                canvas.rect(r.x, r.y + 23, 18, 2, ACCENT);
            }
            label(text, &mut canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, inner - 28.0);
            hits.push((r, OverlayHit::Toggle));
            y += 34;
        }
        for (i, (name, _)) in p.checks.iter().enumerate() {
            let r = Rect { x: bx + 24, y, w: bw - 48, h: 26 };
            // Outlined, so an empty box shows.
            canvas.rect(r.x - 1, r.y + 3, 20, 20, DIM_FG);
            canvas.rect(r.x, r.y + 4, 18, 18, if view.checks[i] { ACCENT } else { FIELD_BG });
            if view.focus == CHECK_FOCUS + i {
                canvas.rect(r.x, r.y + 23, 18, 2, ACCENT);
            }
            label(text, &mut canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, inner - 28.0);
            hits.push((r, OverlayHit::Check(i)));
            y += 34;
        }
        let status = if view.busy { "Authenticating…" } else { p.warning.as_str() };
        if !status.is_empty() {
            let color = if view.busy { DIM_FG } else { SHARING };
            label(text, &mut canvas, status, center(status, 13.0), (y + 14) as f32, 13.0, color, inner);
        }
        let by2 = by + bh - 56;
        let half = (bw - 24 * 2 - 12) / 2;
        let cancel = Rect { x: bx + 24, y: by2, w: half, h: 36 };
        let confirm = Rect { x: bx + 24 + half + 12, y: by2, w: half, h: 36 };
        canvas.rect(cancel.x, cancel.y, cancel.w, cancel.h, BANNER_BORDER);
        canvas.rect(confirm.x, confirm.y, confirm.w, confirm.h, if view.busy { FIELD_BG } else { ACCENT });
        let cancel_label = if p.cancel_label.is_empty() { "Cancel" } else { &p.cancel_label };
        let confirm_label = if p.continue_label.is_empty() { "Continue" } else { &p.continue_label };
        for (r, s) in [(cancel, cancel_label), (confirm, confirm_label)] {
            let lw = text.map_or(0.0, |t| t.width(s, 15.0));
            label(text, &mut canvas, s, r.x as f32 + (r.w as f32 - lw) / 2.0, baseline(text, r.y, r.h, 15.0), 15.0, FG, r.w as f32);
        }
        hits.push((cancel, OverlayHit::Cancel));
        hits.push((confirm, OverlayHit::Confirm));
        overlay.hits = hits;
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }
}
