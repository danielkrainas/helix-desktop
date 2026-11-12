//! The prompt dialog (helix-desktop-spec
//! `org.freedesktop.PolicyKit1.AuthenticationAgent.md`, "Prompt dialog").

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::shell::WaylandSurface;
use tokio::sync::oneshot;

use super::{baseline, label, physical, wipe, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, ACCENT, BANNER_BG, BANNER_BORDER, DIM_FG, FG, FIELD_BG, SCRIM, SHARING};
use crate::auth::Secret;
use crate::prompt::{Answer, Prompt, Request};

/// A prompt on screen.
pub(super) struct PromptView {
    prompt: Prompt,
    reply: Option<oneshot::Sender<Answer>>,
    /// The password and confirmation fields' text.
    fields: [String; 2],
    /// 0, 1: a field; 2: the check box.
    focus: usize,
    choice: bool,
    /// Answered; waiting for the next prompt or the end.
    busy: bool,
}

impl PromptView {
    fn new(request: Request) -> Self {
        let choice = request.prompt.choice.as_ref().is_some_and(|c| c.1);
        let focus = if request.prompt.password.is_some() { 0 } else { 2 };
        Self { prompt: request.prompt, reply: Some(request.reply), fields: Default::default(), focus, choice, busy: false }
    }

    /// Answer "cancel" if nobody answered yet.
    pub(super) fn cancel(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Answer::Cancel);
        }
    }

    fn focusable(&self) -> Vec<usize> {
        let mut out = Vec::new();
        if self.prompt.password.is_some() {
            out.push(0);
        }
        if self.prompt.confirm.is_some() {
            out.push(1);
        }
        if self.prompt.choice.is_some() {
            out.push(2);
        }
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
        let answer = Answer::Continue { password, confirm, choice: view.choice };
        if let Some(reply) = view.reply.take() {
            let _ = reply.send(answer);
        }
        view.busy = true;
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
        let Some(view) = self.prompt_view() else { return };
        if view.busy {
            return;
        }
        match event.keysym {
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                let order = view.focusable();
                if let Some(i) = order.iter().position(|f| *f == view.focus) {
                    let back = event.keysym == Keysym::ISO_Left_Tab;
                    let next = if back { (i + order.len() - 1) % order.len() } else { (i + 1) % order.len() };
                    view.focus = order[next];
                }
            }
            Keysym::space if view.focus == 2 => {
                view.choice = !view.choice;
            }
            Keysym::BackSpace if view.focus < 2 => {
                view.fields[view.focus].pop();
            }
            _ if view.focus < 2 => {
                if let Some(text) = &event.utf8 {
                    view.fields[view.focus].extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            _ => {}
        }
        self.draw_dialog();
    }

    pub(super) fn paint_prompt(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::Prompt(view) = &overlay.state.kind else { return };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let p = &view.prompt;
        let bw = 440.min(w as i32 - 40);
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
        let fields = usize::from(p.password.is_some()) + usize::from(p.confirm.is_some());
        let bh = 70 + message.len() as i32 * 19 + description.len() as i32 * 17 + 8 + fields as i32 * 62 + if p.choice.is_some() { 34 } else { 0 } + 26 + 64;
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 3;
        canvas.rect(bx - 1, by - 1, bw + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
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
        let mut hits = Vec::new();
        for (index, field) in [&p.password, &p.confirm].into_iter().enumerate() {
            let Some(name) = field else { continue };
            label(text, &mut canvas, name, (bx + 24) as f32, (y + 14) as f32, 13.0, DIM_FG, inner);
            let r = Rect { x: bx + 24, y: y + 22, w: bw - 48, h: 34 };
            canvas.rect(r.x, r.y, r.w, r.h, FIELD_BG);
            if view.focus == index && !view.busy {
                canvas.rect(r.x, r.y + r.h - 2, r.w, 2, ACCENT);
            }
            // One dot per character (drawn: not every font has a bullet).
            let count = view.fields[index].chars().count().min(((r.w - 20) / 12) as usize);
            for i in 0..count {
                canvas.circle(f64::from(r.x + 16 + i as i32 * 12), f64::from(r.y + r.h / 2), 4.0, FG);
            }
            hits.push((r, OverlayHit::Inside));
            y += 62;
        }
        if let Some((name, _)) = &p.choice {
            let r = Rect { x: bx + 24, y, w: bw - 48, h: 26 };
            canvas.rect(r.x, r.y + 4, 18, 18, if view.choice { ACCENT } else { FIELD_BG });
            if view.focus == 2 {
                canvas.rect(r.x, r.y + 23, 18, 2, ACCENT);
            }
            label(text, &mut canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, 14.0), 14.0, FG, inner - 28.0);
            hits.push((r, OverlayHit::Toggle));
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
