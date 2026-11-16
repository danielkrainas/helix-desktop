//! The prompt dialog (helix-desktop-spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`, "Prompt dialog").

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::shell::WaylandSurface;
use tokio::sync::oneshot;

use super::{baseline, caret, label, text_field, physical, size, theme, wipe, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, DIM_FG, FG, SCRIM, ERROR_TEXT};
use crate::auth::Secret;
use crate::prompt::{Answer, Prompt, Request};
use helix_i18n::tr;

const LIST_ROW: i32 = 32;
const LIST_SHOWN: usize = 10;
/// The least room kept between a prompt and the screen's edges.
const PROMPT_MARGIN: i32 = 16;

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
    /// Where the dialog's top edge was first drawn, on a screen of which
    /// size: it stays there (a dialog that changes height grows
    /// downwards rather than moving what is under the pointer).
    top: std::cell::Cell<Option<(i32, (usize, usize))>>,
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
/// Focus on a password field's "Show" button (+ the field's index), on
/// Cancel and on the confirm button.
const SHOW_FOCUS: usize = 30;
const CANCEL_FOCUS: usize = 40;
const CONFIRM_FOCUS: usize = 41;

/// The width of a password field's "Show" button.
const SHOW_W: i32 = 56;

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
        Self { prompt: request.prompt, reply: Some(request.reply), fields: Default::default(), entry, focus, choice, selected, picked, busy: false, top: Default::default(), shown: [false; 2], can_show: !crate::locked_down("disable-show-password"), history_at: None, commands: None, checks }
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
        // Each password field, then its "Show" button.
        for (index, field) in [&self.prompt.password, &self.prompt.confirm].into_iter().enumerate() {
            if field.is_some() {
                out.push(index);
                if self.can_show {
                    out.push(SHOW_FOCUS + index);
                }
            }
        }
        if self.prompt.choice.is_some() {
            out.push(2);
        }
        out.extend((0..self.prompt.checks.len()).map(|i| CHECK_FOCUS + i));
        out.extend([CANCEL_FOCUS, CONFIRM_FOCUS]);
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
        self.show_dialog(DialogState::new(DialogKind::Prompt(Box::new(view)), 0));
    }

    /// The flow is over: close the prompt.
    pub(super) fn prompt_done(&mut self) {
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::Prompt(_))) {
            self.dialog = None;
        }
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
        let answer = Answer::Continue { entry, password, confirm, choice: view.choice, checks: view.checks.clone(), selected: view.selected, picked };
        if let Some(reply) = view.reply.take() {
            let _ = reply.send(answer);
        }
        view.busy = true;
        self.draw_dialog();
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
            // Enter always answers with the default button (Space presses
            // the focused one).
            Keysym::Return | Keysym::KP_Enter => return self.prompt_answer(true),
            _ => {}
        }
        // On a button: Space presses it, Left/Right move between the two.
        let on_button = self.prompt_view().filter(|v| !v.busy).map(|v| v.focus).filter(|f| *f >= SHOW_FOCUS);
        if let Some(focus) = on_button {
            match event.keysym {
                Keysym::space if focus == CANCEL_FOCUS => return self.prompt_answer(false),
                Keysym::space if focus == CONFIRM_FOCUS => return self.prompt_answer(true),
                Keysym::space if focus < CANCEL_FOCUS => return self.prompt_show_password(focus - SHOW_FOCUS),
                Keysym::Left | Keysym::Right if focus >= CANCEL_FOCUS => {
                    if let Some(view) = self.prompt_view() {
                        view.focus = if event.keysym == Keysym::Left { CANCEL_FOCUS } else { CONFIRM_FOCUS };
                    }
                    return self.draw_dialog();
                }
                _ => {}
            }
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

    /// The prompt's text field with the keyboard focus (its accessible
    /// id), if one has it.
    pub(super) fn prompt_focused_field(&self) -> Option<String> {
        let Some(DialogKind::Prompt(view)) = self.dialog.as_ref().map(|d| &d.state.kind) else { return None };
        match view.focus {
            0 if view.prompt.password.is_some() => Some("password:0".into()),
            1 if view.prompt.confirm.is_some() => Some("password:1".into()),
            3 if view.prompt.entry.is_some() => Some("entry".into()),
            _ => None,
        }
    }

    /// Field `id`'s text, and whether it is a password (never copied).
    pub(super) fn prompt_field_text(&self, id: &str) -> Option<(String, bool)> {
        let Some(DialogKind::Prompt(view)) = self.dialog.as_ref().map(|d| &d.state.kind) else { return None };
        if id == "entry" && view.prompt.entry.is_some() {
            return Some((view.entry.clone(), false));
        }
        let i = id.strip_prefix("password:").and_then(|i| i.parse::<usize>().ok()).filter(|i| *i < 2)?;
        Some((view.fields[i].clone(), true))
    }

    /// An assistive technology set field `id`'s text (`ui/atspi.rs`).
    pub(super) fn prompt_set_text(&mut self, id: &str, value: &str, range: Option<(usize, usize)>) {
        let Some(DialogKind::Prompt(view)) = self.dialog.as_mut().map(|d| &mut d.state.kind) else { return };
        if view.busy {
            return;
        }
        if id == "entry" && view.prompt.entry.is_some() {
            view.entry = super::atspi::edited(&view.entry, false, value, range);
            view.focus = 3;
        } else if let Some(i) = id.strip_prefix("password:").and_then(|i| i.parse::<usize>().ok()).filter(|i| *i < 2) {
            let mut new = super::atspi::edited(&view.fields[i], !view.shown[i], value, range);
            std::mem::swap(&mut view.fields[i], &mut new);
            wipe(&mut new);
            view.focus = i;
        } else {
            return;
        }
        self.draw_dialog();
    }

    pub(super) fn paint_prompt(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::Prompt(view) = &overlay.state.kind else { return };
        let view = &**view;
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_dialog, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        let (hits, status, (cancel_label, confirm_label)) = paint_prompt_view(&mut canvas, (text, bold), (w, h), view, (overlay.state.hover, overlay.state.pressed));
        let (status, cancel_label, confirm_label) = (status.as_str(), cancel_label.as_str(), confirm_label.as_str());
        let node = a11y(view, &hits, status, (cancel_label, confirm_label), text);
        overlay.hits = hits;
        self.atspi_record(overlay.layer.wl_surface(), vec![node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }
}

/// Paint prompt `view` over a scrim on a `w`×`h` surface, the part under
/// the pointer `hover` and the one held `pressed`. Returns the clickable
/// parts, the status line and the buttons' labels (Cancel, Continue).
#[allow(clippy::type_complexity)]
pub(super) fn paint_prompt_view(canvas: &mut Canvas, (text, bold): (Option<&crate::text::Text>, Option<&crate::text::Text>), (w, h): (usize, usize), view: &PromptView, (hover, pressed): (Option<OverlayHit>, Option<OverlayHit>)) -> (Vec<(Rect, OverlayHit)>, String, (String, String)) {
    canvas.fill(SCRIM);
    let p = &view.prompt;
    // Lengths that hold text grow with the text scale.
    let grow = helix_draw::theme::grow;
    let bw = grow(440).min(w as i32 - 40);
    let inner = (bw - 48) as f32;
    let wrap = |s: &str, px: f32| -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            text.map_or_else(|| vec![s.to_owned()], |t| t.wrap(s, px, inner, 4))
        }
    };
    let message = wrap(&p.message, size::LABEL);
    let description = wrap(&p.description, size::CAPTION);
    let fields = usize::from(p.entry.is_some()) + usize::from(p.password.is_some()) + usize::from(p.confirm.is_some());
    // The picture: at most the inner width and 220 high.
    let picture = p.picture.as_deref().filter(|i| i.width > 0 && i.height > 0).map(|i| {
        let k = (f64::from(inner) / i.width as f64).min(220.0 / i.height as f64);
        (i, (i.width as f64 * k).round(), (i.height as f64 * k).round())
    });
    let picture_h = picture.map_or(0, |(_, _, ph)| ph as i32 + 12);
    // An icon above the title.
    let icon = p.icon.as_deref().filter(|i| i.width > 0 && i.height > 0);
    let top = if icon.is_some() { 60 } else { 0 };
    // Who authenticates: an avatar with the name under it.
    let identity_h = if p.identity.is_some() { grow(IDENTITY_H) } else { 0 };
    let list_h = |rows: usize| if rows > 0 { rows as i32 * grow(LIST_ROW) + 8 } else { 0 };
    let rest_h = identity_h + top + grow(70) + message.len() as i32 * grow(19) + description.len() as i32 * grow(17) + 8 + picture_h + fields as i32 * grow(62) + if p.choice.is_some() { grow(34) } else { 0 } + p.checks.len() as i32 * grow(34) + grow(26) + grow(64);
    // At most LIST_SHOWN rows, fewer when the dialog would not fit on
    // the screen (large text), scrolled to keep the selection in view.
    let room = h as i32 - 2 * PROMPT_MARGIN;
    let fit = usize::try_from((room - rest_h - 8) / grow(LIST_ROW)).unwrap_or(0).max(1);
    let shown = p.list.len().min(LIST_SHOWN).min(fit);
    let first = view.selected.saturating_sub(shown.saturating_sub(1)).min(p.list.len().saturating_sub(shown));
    let list_h = list_h(shown);
    let bh = (rest_h + list_h).min(room.max(0));
    let bx = (w as i32 - bw) / 2;
    // A third of the way down, then kept there; never off the screen.
    let by = match view.top.get() {
        Some((by, size)) if size == (w, h) && by + bh <= h as i32 - PROMPT_MARGIN => by,
        _ => {
            let by = ((h as i32 - bh) / 3).max(PROMPT_MARGIN.min((h as i32 - bh) / 2));
            view.top.set(Some((by, (w, h))));
            by
        }
    };
    theme::popover(canvas, bx, by, bw, bh);
    let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
    if let Some(icon) = icon {
        canvas.image(f64::from(bx + (bw - 48) / 2), f64::from(by + 20), 48.0, 48.0, icon);
    }
    // Below the icon, laid out as without it.
    let (by, bh) = (by + top, bh - top);
    let bold = bold.or(text);
    let tw = bold.map_or(0.0, |t| t.width(&p.title, size::TITLE).min(inner));
    label(bold, canvas, &p.title, bx as f32 + (bw as f32 - tw) / 2.0, (by + grow(44)) as f32, size::TITLE, FG, inner);
    let mut y = by + grow(76);
    for line in &message {
        label(text, canvas, line, center(line, size::LABEL), y as f32, size::LABEL, FG, inner);
        y += grow(19);
    }
    for line in &description {
        label(text, canvas, line, center(line, size::CAPTION), y as f32, size::CAPTION, DIM_FG, inner);
        y += grow(17);
    }
    y += 8;
    if let Some((image, pw, ph)) = picture {
        canvas.image(f64::from(bx) + (f64::from(bw) - pw) / 2.0, f64::from(y), pw, ph, image);
        y += picture_h;
    }
    let mut hits = Vec::new();
    if shown > 0 {
        theme::well(canvas, bx + 24, y, bw - 48, shown as i32 * grow(LIST_ROW));
        for (i, name) in p.list.iter().enumerate().skip(first).take(shown) {
            let r = Rect { x: bx + 24, y: y + (i - first) as i32 * grow(LIST_ROW), w: bw - 48, h: grow(LIST_ROW) };
            let lit = if p.multiple { view.picked[i] } else { i == view.selected };
            let hit = OverlayHit::Row(i);
            let look = theme::Look { on: lit, hovered: hover == Some(hit), pressed: pressed == Some(hit) };
            theme::row(canvas, r.x + 2, r.y + 1, r.w - 4, r.h - 2, look);
            if p.multiple && i == view.selected {
                // The current row: an outline.
                canvas.round_outline(f64::from(r.x + 3), f64::from(r.y + 2), f64::from(r.w - 6), f64::from(r.h - 4), theme::RADIUS_ROW - 1.0, 1.0, FG);
            }
            label(text, canvas, name, (r.x + 12) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, theme::control_text(look), (r.w - 24) as f32);
            hits.push((r, OverlayHit::Row(i)));
        }
    }
    y += list_h;
    if let Some((name, picture)) = &p.identity {
        let r = f64::from(AVATAR_R);
        let (cx, cy) = (f64::from(bx) + f64::from(bw) / 2.0, f64::from(y) + 4.0 + r);
        helix_draw::screen::avatar(canvas, text, bold, name, picture.as_deref(), cx, cy, r, 1.0);
        let nw = bold.map_or(0.0, |t| t.width(name, size::BODY).min(inner));
        label(bold, canvas, name, bx as f32 + (bw as f32 - nw) / 2.0, (cy + r) as f32 + 8.0 + grow(16) as f32, size::BODY, FG, inner);
        y += identity_h;
    }
    if let Some((name, _)) = &p.entry {
        label(text, canvas, name, (bx + 24) as f32, (y + grow(14)) as f32, size::CAPTION, DIM_FG, inner);
        let r = Rect { x: bx + 24, y: y + grow(22), w: bw - 48, h: grow(34) };
        let focused = view.focus == 3 && !view.busy;
        text_field(canvas, r, focused);
        // The end of the text when it does not fit, and a caret.
        let room = (r.w - 24) as f32;
        let mut shown = view.entry.as_str();
        while text.is_some_and(|t| t.width(shown, size::LABEL) > room) && !shown.is_empty() {
            let next = shown.char_indices().nth(1).map_or(shown.len(), |(i, _)| i);
            shown = &shown[next..];
        }
        label(text, canvas, shown, (r.x + 10) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, room);
        if focused {
            caret(canvas, r, r.x + 10 + text.map_or(0.0, |t| t.width(shown, size::LABEL)) as i32 + 1, size::LABEL);
        }
        hits.push((r, OverlayHit::Field(3)));
        y += grow(62);
    }
    for (index, field) in [&p.password, &p.confirm].into_iter().enumerate() {
        let Some(name) = field else { continue };
        label(text, canvas, name, (bx + 24) as f32, (y + grow(14)) as f32, size::CAPTION, DIM_FG, inner);
        let r = Rect { x: bx + 24, y: y + grow(22), w: bw - 48, h: grow(34) };
        let focused = view.focus == index && !view.busy;
        text_field(canvas, r, focused);
        let show_w = if view.can_show { SHOW_W } else { 0 };
        let end = if view.shown[index] {
            let room = (r.w - 24 - show_w) as f32;
            label(text, canvas, &view.fields[index], (r.x + 10) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, room);
            r.x + 10 + text.map_or(0.0, |t| t.width(&view.fields[index], size::LABEL).min(room)) as i32 + 1
        } else {
            // One dot per character (drawn: not every font has a bullet).
            let count = view.fields[index].chars().count().min(((r.w - 28 - show_w) / 12) as usize);
            for i in 0..count {
                canvas.circle(f64::from(r.x + 16 + i as i32 * 12), f64::from(r.y + r.h / 2), 4.0, FG);
            }
            r.x + 10 + count as i32 * 12
        };
        if focused {
            caret(canvas, r, end, size::LABEL);
        }
        if view.can_show {
            let b = Rect { x: r.x + r.w - SHOW_W, y: r.y, w: SHOW_W, h: r.h };
            let s = if view.shown[index] { tr("Hide") } else { tr("Show") };
            let hit = OverlayHit::ShowPassword(index);
            let look = theme::Look { hovered: hover == Some(hit), pressed: pressed == Some(hit), ..Default::default() };
            let inside = (f64::from(b.x + 4), f64::from(b.y + 5), f64::from(b.w - 8), f64::from(b.h - 10));
            theme::flat(canvas, inside, theme::RADIUS_ROW, look);
            let lw = text.map_or(0.0, |t| t.width(s, size::CAPTION));
            label(text, canvas, s, b.x as f32 + (b.w as f32 - lw) / 2.0, baseline(text, b.y, b.h, size::CAPTION), size::CAPTION, if look.hovered || look.pressed { FG } else { DIM_FG }, b.w as f32);
            if view.focus == SHOW_FOCUS + index && !view.busy {
                theme::focus_ring_round(canvas, inside.0, inside.1, inside.2, inside.3, theme::RADIUS_ROW);
            }
            hits.push((b, OverlayHit::ShowPassword(index)));
        }
        hits.push((r, OverlayHit::Field(index)));
        y += grow(62);
    }
    if let Some((name, _)) = &p.choice {
        let r = Rect { x: bx + 24, y, w: bw - 48, h: grow(26) };
        let look = theme::Look { hovered: hover == Some(OverlayHit::Toggle), pressed: pressed == Some(OverlayHit::Toggle), ..Default::default() };
        check(canvas, r, view.choice, look, view.focus == 2);
        label(text, canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, inner - 28.0);
        hits.push((r, OverlayHit::Toggle));
        y += grow(34);
    }
    for (i, (name, _)) in p.checks.iter().enumerate() {
        let r = Rect { x: bx + 24, y, w: bw - 48, h: grow(26) };
        let look = theme::Look { hovered: hover == Some(OverlayHit::Check(i)), pressed: pressed == Some(OverlayHit::Check(i)), ..Default::default() };
        check(canvas, r, view.checks[i], look, view.focus == CHECK_FOCUS + i);
        label(text, canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, inner - 28.0);
        hits.push((r, OverlayHit::Check(i)));
        y += grow(34);
    }
    // Busy: a password being checked says so; other answers (a command
    // to run, a choice) are quick and say nothing.
    let status = match (view.busy, p.password.is_some()) {
        (true, true) => tr("Authenticating…"),
        (true, false) => "",
        (false, _) => p.warning.as_str(),
    };
    if !status.is_empty() {
        let color = if view.busy { DIM_FG } else { ERROR_TEXT };
        label(text, canvas, status, center(status, size::CAPTION), (y + grow(14)) as f32, size::CAPTION, color, inner);
    }
    let by2 = by + bh - grow(56);
    let half = (bw - 24 * 2 - 12) / 2;
    let cancel = Rect { x: bx + 24, y: by2, w: half, h: grow(36) };
    let confirm = Rect { x: bx + 24 + half + 12, y: by2, w: half, h: grow(36) };
    let cancel_label = if p.cancel_label.is_empty() { tr("Cancel") } else { &p.cancel_label };
    let confirm_label = if p.continue_label.is_empty() { tr("Continue") } else { &p.continue_label };
    // Rounded buttons; the confirm button is the default one (the
    // suggested style; Enter presses it).
    let look = |hit| theme::Look { hovered: hover == Some(hit), pressed: pressed == Some(hit), ..Default::default() };
    let at = |r: Rect| (r.x, r.y, r.w, r.h);
    theme::button(canvas, text, at(cancel), cancel_label, theme::Kind::Normal, look(OverlayHit::Cancel), view.focus == CANCEL_FOCUS && !view.busy);
    if view.busy {
        theme::button_off(canvas, text, at(confirm), confirm_label);
    } else {
        theme::button(canvas, text, at(confirm), confirm_label, theme::Kind::Suggested, look(OverlayHit::Confirm), view.focus == CONFIRM_FOCUS);
    }
    hits.push((cancel, OverlayHit::Cancel));
    hits.push((confirm, OverlayHit::Confirm));
    (hits, status.to_owned(), (cancel_label.to_owned(), confirm_label.to_owned()))
}

/// The avatar of who authenticates (radius, logical px), and the room it
/// and the name take.
const AVATAR_R: i32 = 24;
const IDENTITY_H: i32 = 84;

/// A prompt's view for the gallery and tests.
#[cfg(test)]
pub(super) fn sample_view(prompt: Prompt) -> PromptView {
    let (reply, _) = oneshot::channel();
    PromptView::new(Request { prompt, reply })
}

/// A check box row's box (at the start of `r`) in `look`, with the focus
/// ring when `focused`.
fn check(canvas: &mut Canvas, r: Rect, on: bool, look: theme::Look, focused: bool) {
    let (x, y) = (f64::from(r.x), f64::from(r.y + (r.h - 18) / 2));
    theme::check_box(canvas, x, y, 18.0, on, look);
    if focused {
        theme::focus_ring_round(canvas, x, y, 18.0, 18.0, theme::RADIUS_CHECK);
    }
}

/// The prompt on the accessibility bus (`ui/atspi.rs`): the texts, fields
/// and buttons painted, focus on the field that takes typing.
fn a11y(view: &PromptView, hits: &[(Rect, OverlayHit)], status: &str, (cancel, confirm): (&str, &str), text: Option<&crate::text::Text>) -> crate::a11y::Node {
    use crate::a11y::{Node, Role};
    let p = &view.prompt;
    let at = |hit: OverlayHit| hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| crate::a11y::Rect::from(*r)).unwrap_or_default();
    let typing = |f: usize| view.focus == f && !view.busy;
    let mut out = Node::new(Role::Dialog, &p.title).id("prompt").modal();
    for text in [&p.message, &p.description] {
        if !text.is_empty() {
            out = out.child(Node::label(text));
        }
    }
    if let Some((name, _)) = &p.identity {
        out = out.child(Node::label(name).id("identity"));
    }
    if !p.list.is_empty() {
        let list_focused = view.focus == 2 && p.choice.is_none() && !view.busy;
        let rows = p.list.iter().enumerate().filter(|(i, _)| hits.iter().any(|(_, h)| *h == OverlayHit::Row(*i))).map(|(i, name)| {
            let on = if p.multiple { view.picked[i] } else { i == view.selected };
            Node::new(Role::ListBoxOption, name).id(format!("row:{i}")).at(at(OverlayHit::Row(i))).click().selected(on).focused(list_focused && i == view.selected)
        });
        out = out.child(Node::new(Role::ListBox, "").children(rows));
    }
    if let Some((name, _)) = &p.entry {
        // Painted from r.x + 10, its start scrolled out when it is long.
        let r = at(OverlayHit::Field(3));
        let room = (r.w - 24) as f32;
        let advances = text.map(|t| t.advances(&view.entry, size::LABEL)).unwrap_or_default();
        let total: f32 = advances.iter().sum();
        let mut hidden = 0.0;
        for a in &advances {
            if total - hidden <= room {
                break;
            }
            hidden += a;
        }
        let chars = crate::a11y::Chars { x: (r.x + 10) as f32 - hidden, y: r.y, h: r.h, advances };
        out = out.child(Node::text_field(Role::TextInput, name, &view.entry, r).id("entry").chars(chars).focused(typing(3)));
    }
    for (index, field) in [&p.password, &p.confirm].into_iter().enumerate() {
        let Some(name) = field else { continue };
        let r = at(OverlayHit::Field(index));
        let node = if view.shown[index] {
            let advances = text.map(|t| t.advances(&view.fields[index], size::LABEL)).unwrap_or_default();
            Node::text_field(Role::TextInput, name, &view.fields[index], r).chars(crate::a11y::Chars { x: (r.x + 10) as f32, y: r.y, h: r.h, advances })
        } else {
            // One dot per character, 12 apart from r.x + 12.
            let count = view.fields[index].chars().count();
            Node::text_field(Role::PasswordInput, name, Node::mask(&view.fields[index]), r).chars(crate::a11y::Chars::even((r.x + 12) as f32, r.y, r.h, 12.0, count))
        };
        out = out.child(node.id(format!("password:{index}")).focused(typing(index)));
        if view.can_show {
            out = out.child(Node::new(Role::Button, tr("Show Password")).id(format!("show:{index}")).at(at(OverlayHit::ShowPassword(index))).click().toggled(view.shown[index]).focusable().focused(view.focus == SHOW_FOCUS + index && !view.busy));
        }
    }
    if let Some((name, _)) = &p.choice {
        out = out.child(Node::new(Role::CheckBox, name).id("choice").at(at(OverlayHit::Toggle)).click().toggled(view.choice).focused(typing(2)));
    }
    for (i, (name, _)) in p.checks.iter().enumerate() {
        out = out.child(Node::new(Role::CheckBox, name).id(format!("check:{i}")).at(at(OverlayHit::Check(i))).click().toggled(view.checks[i]).focused(typing(CHECK_FOCUS + i)));
    }
    if !status.is_empty() {
        out = out.child(Node::label(status).id("status").live());
    }
    let nothing_focused = out.find(&|n| n.focused).is_none();
    let on = |f: usize| view.focus == f && !view.busy;
    out.child(Node::button(cancel, at(OverlayHit::Cancel)).id("cancel").focusable().focused(on(CANCEL_FOCUS)))
        .child(Node::button(confirm, at(OverlayHit::Confirm)).id("confirm").disabled(view.busy).focusable().focused(on(CONFIRM_FOCUS) || (nothing_focused && !view.busy)))
}
