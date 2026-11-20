//! An agent's question for the user (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Talk to the user methods";
//! helixdesktop/desktop#148): `Ask` (an answer typed or a choice picked)
//! and `Confirm` (yes or no).
//!
//! It looks unlike the system's prompts, so the user knows who asks: a
//! band in the agent indicator's red says "Question from an agent", the
//! title names the agent (its display name, which is only its claim) and
//! the program below it, the agent's words stand in a quoted block, and
//! the footer says Helix did not ask. Everything is left-aligned (the
//! system's prompts are centered). The text is plain: what the agent sent
//! is drawn as written, never as markup.
//!
//! Only the user at the machine answers it, as the consent prompt
//! (`ui/agent_prompt.rs`): the answer goes back on a channel only this
//! dialog holds; its surface has the protected namespace, so the
//! compositor drops remote, EIS and test input while it is shown;
//! accessibility actions on it are refused (setting the field's text
//! too); and keys and clicks in its first second are ignored.

use std::time::Instant;

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::WaylandSurface;

use super::agent_prompt::{armed, NAMESPACE};
use super::{baseline, caret, label, physical, size, text_field, theme, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, DIM_FG, FG, PRIVACY_PILL, SCRIM};
use crate::a11y::{Node, Role};
use crate::agent::talk::{clean, Dialog, QuestionRequest, Reply, ANSWER_MAX};
use helix_i18n::{tr, trf};

/// What the keyboard can be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// The answer's text field.
    Field,
    Choice(usize),
    Cancel,
    /// Send the typed answer.
    Answer,
    No,
    Yes,
}

/// An agent's question on screen (or waiting for its turn).
pub(super) struct AgentQuestionView {
    request: Option<QuestionRequest>,
    /// The question's id (the shell closes it by this).
    pub(super) id: u64,
    name: String,
    agent: String,
    dialog: Dialog,
    /// Index into [`AgentQuestionView::stops`].
    focus: usize,
    /// The typed answer.
    entry: String,
}

impl AgentQuestionView {
    pub(super) fn new(request: QuestionRequest) -> Self {
        let entry = match &request.dialog {
            Dialog::Ask { choices, default, .. } if choices.is_empty() => default.clone(),
            _ => String::new(),
        };
        // The text field, the default choice, or No.
        let focus = match &request.dialog {
            Dialog::Ask { choices, default, .. } => choices.iter().position(|c| c == default).unwrap_or(0),
            Dialog::Confirm { .. } => 0,
        };
        Self { id: request.id, name: request.name.clone(), agent: request.agent.clone(), dialog: request.dialog.clone(), request: Some(request), focus, entry }
    }

    /// The parts the keyboard moves between, in order.
    pub(super) fn stops(&self) -> Vec<Stop> {
        match &self.dialog {
            Dialog::Ask { choices, .. } if choices.is_empty() => vec![Stop::Field, Stop::Cancel, Stop::Answer],
            Dialog::Ask { choices, .. } => (0..choices.len()).map(Stop::Choice).chain([Stop::Cancel]).collect(),
            Dialog::Confirm { .. } => vec![Stop::No, Stop::Yes],
        }
    }

    fn focused(&self) -> Option<Stop> {
        self.stops().get(self.focus).copied()
    }

    /// Answer (once).
    fn answer(&mut self, reply: Reply) {
        if let Some(request) = self.request.take() {
            // Never the answer itself: it may be private.
            tracing::info!(agent = %request.agent, id = self.id, cancelled = (reply == Reply::Cancelled), "agent question answered");
            let _ = request.reply.send(reply);
        }
    }

    /// What pressing `stop` answers (`None`: it only takes the focus).
    fn reply_of(&self, stop: Stop) -> Option<Reply> {
        match stop {
            Stop::Field => None,
            Stop::Choice(i) => Some(Reply::Choice(i)),
            Stop::Cancel => Some(Reply::Cancelled),
            Stop::Answer => Some(Reply::Text(self.entry.clone())),
            Stop::No => Some(Reply::Yes(false)),
            Stop::Yes => Some(Reply::Yes(true)),
        }
    }

    /// Type `text` into the answer (plain, at most [`ANSWER_MAX`]).
    fn type_text(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            if self.entry.chars().count() >= ANSWER_MAX {
                break;
            }
            self.entry.push(c);
        }
    }
}

impl Drop for AgentQuestionView {
    /// A question that goes away unanswered (locked, closed) is cancelled.
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ = request.reply.send(Reply::Cancelled);
        }
    }
}

/// What a key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Key {
    Focus(usize),
    Press(usize),
    Cancel,
    Type(String),
    Erase,
}

/// What `keysym` (typing `utf8`) does with stop `focus` of `stops`.
pub(super) fn key_action(keysym: Keysym, utf8: Option<&str>, focus: usize, stops: &[Stop]) -> Option<Key> {
    let n = stops.len();
    let on = stops.get(focus).copied();
    let typing = on == Some(Stop::Field);
    let answer_at = stops.iter().position(|s| *s == Stop::Answer);
    match keysym {
        Keysym::Escape => Some(Key::Cancel),
        // Enter in the field sends the answer.
        Keysym::Return | Keysym::KP_Enter => Some(Key::Press(if typing { answer_at.unwrap_or(focus) } else { focus })),
        Keysym::Tab => Some(Key::Focus((focus + 1) % n)),
        Keysym::ISO_Left_Tab => Some(Key::Focus((focus + n - 1) % n)),
        Keysym::BackSpace if typing => Some(Key::Erase),
        Keysym::Down | Keysym::Right if !typing => Some(Key::Focus((focus + 1).min(n - 1))),
        Keysym::Up | Keysym::Left if !typing => Some(Key::Focus(focus.saturating_sub(1))),
        Keysym::space if !typing => Some(Key::Press(focus)),
        _ if typing => utf8.filter(|t| !t.is_empty() && !t.chars().all(char::is_control)).map(|t| Key::Type(t.to_owned())),
        _ => None,
    }
}

/// Who asks, for the title: the display name, else the program's file
/// name.
fn who(view: &AgentQuestionView) -> String {
    let name = clean(&view.name);
    if name.is_empty() {
        let exe = view.agent.split(' ').next().unwrap_or(&view.agent);
        exe.rsplit('/').next().unwrap_or(exe).to_owned()
    } else {
        name
    }
}

/// The texts painted: the band, the title, the program line, the
/// question and the footer.
fn texts(view: &AgentQuestionView) -> [String; 5] {
    let title = match view.dialog {
        Dialog::Ask { .. } => trf!("“{agent}” asks", agent = who(view)),
        Dialog::Confirm { .. } => trf!("“{agent}” asks you to confirm", agent = who(view)),
    };
    [
        tr("Question from an agent").to_owned(),
        title,
        trf!("Program: {program}", program = view.agent),
        view.dialog.question().to_owned(),
        tr("This question comes from the program above, not from Helix.").to_owned(),
    ]
}

/// A button's label for `stop`.
fn stop_label(view: &AgentQuestionView, stop: Stop) -> String {
    match (stop, &view.dialog) {
        (Stop::Choice(i), Dialog::Ask { choices, .. }) => choices.get(i).cloned().unwrap_or_default(),
        (Stop::Cancel, _) => tr("Cancel").to_owned(),
        (Stop::Answer, _) => tr("Answer").to_owned(),
        (Stop::No, Dialog::Confirm { no, .. }) if !no.is_empty() => no.clone(),
        (Stop::Yes, Dialog::Confirm { yes, .. }) if !yes.is_empty() => yes.clone(),
        (Stop::No, _) => tr("No").to_owned(),
        (Stop::Yes, _) => tr("Yes").to_owned(),
        _ => String::new(),
    }
}

/// Paint `view` over a scrim on a `w`×`h` surface, with the agent icon
/// for the band. The clickable parts (each a stop's index) and the
/// accessible tree.
pub(super) fn paint(
    canvas: &mut Canvas,
    (text, bold): (Option<&crate::text::Text>, Option<&crate::text::Text>),
    (w, h): (usize, usize),
    view: &AgentQuestionView,
    (hover, pressed): (Option<OverlayHit>, Option<OverlayHit>),
    icon: Option<&helix_draw::Image>,
) -> (Vec<(Rect, OverlayHit)>, Node) {
    canvas.fill(SCRIM);
    let grow = helix_draw::theme::grow;
    let bw = grow(520).min(w as i32 - 32);
    let pad = 24;
    let inner = (bw - 2 * pad) as f32;
    let [band, title, program, question, footer] = texts(view);
    let bold = bold.or(text);
    let wrap = |face: Option<&crate::text::Text>, s: &str, px: f32, width: f32, lines: usize| face.map_or_else(|| vec![s.to_owned()], |t| t.wrap(s, px, width, lines));
    let title_lines = wrap(bold, &title, size::TITLE, inner, 2);
    let program_lines = wrap(text, &program, size::CAPTION, inner, 2);
    // The question in its block: inside a bar and padding.
    let quote_inner = inner - 28.0;
    let question_lines = wrap(text, &question, size::LABEL, quote_inner, 12);
    let footer_lines = wrap(text, &footer, size::CAPTION, inner, 2);
    let stops = view.stops();
    let choices = stops.iter().filter(|s| matches!(s, Stop::Choice(_))).count() as i32;
    let typed = stops.contains(&Stop::Field);
    let band_h = grow(32);
    let quote_h = question_lines.len() as i32 * grow(20) + grow(20);
    let middle_h = if typed { grow(34) + grow(16) } else { choices * (grow(36) + 8) + grow(8) };
    let bh = 8 + band_h + grow(18) + title_lines.len() as i32 * grow(26) + program_lines.len() as i32 * grow(17) + grow(12) + quote_h + grow(16) + middle_h + grow(36) + grow(14) + footer_lines.len() as i32 * grow(17) + grow(18);
    let bh = bh.min(h as i32 - 16);
    let bx = (w as i32 - bw) / 2;
    let by = ((h as i32 - bh) / 3).max(8);
    theme::popover(canvas, bx, by, bw, bh);
    // The band, in the agent indicator's red.
    canvas.round_rect(f64::from(bx + 8), f64::from(by + 8), f64::from(bw - 16), f64::from(band_h), theme::RADIUS_ROW, PRIVACY_PILL);
    let mut x = bx + 8 + 12;
    if let Some(icon) = icon {
        canvas.image(f64::from(x), f64::from(by + 8 + (band_h - 16) / 2), 16.0, 16.0, icon);
        x += 16 + 8;
    }
    label(bold, canvas, &band, x as f32, baseline(bold, by + 8, band_h, size::CAPTION), size::CAPTION, 0xFFFFFFFF, (bx + bw - 20 - x) as f32);
    let left = (bx + pad) as f32;
    let mut y = by + 8 + band_h + grow(18);
    for line in &title_lines {
        label(bold, canvas, line, left, (y + grow(18)) as f32, size::TITLE, FG, inner);
        y += grow(26);
    }
    for line in &program_lines {
        label(text, canvas, line, left, (y + grow(12)) as f32, size::CAPTION, DIM_FG, inner);
        y += grow(17);
    }
    y += grow(12);
    // The agent's words: a well with a bar on its leading edge.
    theme::well(canvas, bx + pad, y, bw - 2 * pad, quote_h);
    canvas.rect(bx + pad, y, 4, quote_h, PRIVACY_PILL);
    let mut qy = y + grow(10);
    for line in &question_lines {
        label(text, canvas, line, left + 16.0, (qy + grow(15)) as f32, size::LABEL, FG, quote_inner);
        qy += grow(20);
    }
    y += quote_h + grow(16);
    let mut hits = Vec::new();
    let look = |hit: OverlayHit| theme::Look { hovered: hover == Some(hit), pressed: pressed == Some(hit), ..Default::default() };
    let at = |r: Rect| (r.x, r.y, r.w, r.h);
    let mut node = Node::new(Role::Dialog, &title).id("agent-question").modal();
    node = node.child(Node::label(band.as_str())).child(Node::label(program.as_str())).child(Node::label(question.as_str()).id("question"));
    let focused = view.focused();
    let index_of = |stop: Stop| stops.iter().position(|s| *s == stop).unwrap_or(0);
    if typed {
        let r = Rect { x: bx + pad, y, w: bw - 2 * pad, h: grow(34) };
        let on = focused == Some(Stop::Field);
        text_field(canvas, r, on);
        // The end of the text when it does not fit, and a caret.
        let room = (r.w - 24) as f32;
        let mut shown = view.entry.as_str();
        while text.is_some_and(|t| t.width(shown, size::LABEL) > room) && !shown.is_empty() {
            let next = shown.char_indices().nth(1).map_or(shown.len(), |(i, _)| i);
            shown = &shown[next..];
        }
        label(text, canvas, shown, (r.x + 10) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, room);
        if on {
            caret(canvas, r, r.x + 10 + text.map_or(0.0, |t| t.width(shown, size::LABEL)) as i32 + 1, size::LABEL);
        }
        let hit = OverlayHit::AgentButton(index_of(Stop::Field));
        hits.push((r, hit));
        node = node.child(Node::text_field(Role::TextInput, tr("Your answer"), &view.entry, r.into()).id("answer").focused(on));
        y += grow(34) + grow(16);
    } else if choices > 0 {
        for (i, stop) in stops.iter().enumerate().filter(|(_, s)| matches!(s, Stop::Choice(_))) {
            let r = Rect { x: bx + pad, y, w: bw - 2 * pad, h: grow(36) };
            let hit = OverlayHit::AgentButton(i);
            let name = stop_label(view, *stop);
            theme::button(canvas, text, at(r), &name, theme::Kind::Normal, look(hit), view.focus == i);
            hits.push((r, hit));
            node = node.child(Node::button(name.as_str(), r.into()).id(format!("agent-choice:{i}")).focused(view.focus == i));
            y += grow(36) + 8;
        }
        y += grow(8);
    }
    // The buttons along the bottom: two halves, or Cancel alone on the
    // trailing side under the choices.
    let row: Vec<Stop> = stops.iter().copied().filter(|s| matches!(s, Stop::Cancel | Stop::Answer | Stop::No | Stop::Yes)).collect();
    let gap = 12;
    let half = (bw - 2 * pad - gap) / 2;
    for (k, stop) in row.iter().enumerate() {
        let slot = if row.len() == 1 { 1 } else { k as i32 };
        let r = Rect { x: bx + pad + slot * (half + gap), y, w: half, h: grow(36) };
        let i = index_of(*stop);
        let hit = OverlayHit::AgentButton(i);
        let kind = if *stop == Stop::Answer { theme::Kind::Suggested } else { theme::Kind::Normal };
        let name = stop_label(view, *stop);
        theme::button(canvas, text, at(r), &name, kind, look(hit), view.focus == i);
        hits.push((r, hit));
        node = node.child(Node::button(name.as_str(), r.into()).id(format!("agent-button:{i}")).focused(view.focus == i));
    }
    y += grow(36) + grow(14);
    for line in &footer_lines {
        label(text, canvas, line, left, (y + grow(12)) as f32, size::CAPTION, DIM_FG, inner);
        y += grow(17);
    }
    node = node.child(Node::label(footer.as_str()));
    (hits, node)
}

impl Ui {
    /// An agent's question: now, or once the dialog shown (a prompt,
    /// another question) is gone; never while locked (it is cancelled).
    pub(super) fn show_agent_question(&mut self, request: QuestionRequest) {
        if self.lock.session.is_some() {
            return;
        }
        let view = AgentQuestionView::new(request);
        if self.dialog.is_some() || self.agent_waiting.is_some() || !self.agent_questions.is_empty() {
            tracing::info!(agent = %view.agent, id = view.id, "agent question waits for its turn");
            self.agent_questions.push_back(view);
            self.agent_prompt_next();
            return;
        }
        self.open_agent_question(view);
    }

    fn open_agent_question(&mut self, view: AgentQuestionView) {
        tracing::info!(agent = %view.agent, id = view.id, "agent question shown");
        let state = DialogState::new(DialogKind::AgentQuestion(Box::new(view)), 0);
        self.power_menu = None;
        self.close_launcher(true);
        self.close_window_menu();
        self.dialog = Some(self.new_overlay(NAMESPACE, KeyboardInteractivity::Exclusive, state));
    }

    /// The next waiting question, when nothing else is shown.
    pub(super) fn agent_question_next(&mut self) {
        if self.dialog.is_some() || self.agent_waiting.is_some() || self.lock.session.is_some() {
            return;
        }
        if let Some(view) = self.agent_questions.pop_front() {
            self.open_agent_question(view);
        }
    }

    /// Close question `id` unanswered (the shell gave up on it).
    pub(super) fn close_agent_question(&mut self, id: u64) {
        self.agent_questions.retain(|v| v.id != id);
        if self.dialog.as_ref().is_some_and(|d| matches!(&d.state.kind, DialogKind::AgentQuestion(v) if v.id == id)) {
            tracing::info!(id, "agent question closed");
            self.dialog = None;
            self.agent_prompt_next();
        }
    }

    /// Press stop `index`, if the question is armed.
    pub(super) fn agent_question_press(&mut self, index: usize) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        if !armed(dialog.state.shown, Instant::now()) {
            tracing::debug!("agent question not armed yet; input ignored");
            return;
        }
        let DialogKind::AgentQuestion(view) = &mut dialog.state.kind else { return };
        let Some(&stop) = view.stops().get(index) else { return };
        match view.reply_of(stop) {
            None => {
                view.focus = index;
                self.draw_dialog();
            }
            Some(reply) => {
                crate::click_feedback();
                view.answer(reply);
                self.dialog = None;
                self.agent_prompt_next();
            }
        }
    }

    pub(super) fn agent_question_key(&mut self, event: &KeyEvent) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let DialogKind::AgentQuestion(view) = &mut dialog.state.kind else { return };
        match key_action(event.keysym, event.utf8.as_deref(), view.focus, &view.stops()) {
            Some(Key::Focus(i)) => {
                view.focus = i;
                self.draw_dialog();
            }
            Some(Key::Press(i)) => self.agent_question_press(i),
            Some(Key::Cancel) => {
                view.answer(Reply::Cancelled);
                self.dialog = None;
                self.agent_prompt_next();
            }
            Some(Key::Type(t)) => {
                view.type_text(&t);
                self.draw_dialog();
            }
            Some(Key::Erase) => {
                view.entry.pop();
                self.draw_dialog();
            }
            None => {}
        }
    }

    pub(super) fn paint_agent_question(&mut self, overlay: &mut Overlay<DialogState>) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let icon = self.symbolic_icon_color("system-run-symbolic", 16.0, scale, 0xFFFFFFFF);
        let DialogKind::AgentQuestion(view) = &overlay.state.kind else { return };
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_dialog, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        let (hits, node) = paint(&mut canvas, (self.text.as_ref(), self.bold.as_ref()), (w, h), view, (overlay.state.hover, overlay.state.pressed), icon.as_ref());
        overlay.hits = hits;
        self.atspi_record(overlay.layer.wl_surface(), vec![node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn view(dialog: Dialog) -> (AgentQuestionView, oneshot::Receiver<Reply>) {
        let (reply, rx) = oneshot::channel();
        (AgentQuestionView::new(QuestionRequest { id: 7, name: "Build bot".into(), agent: "/usr/bin/python3 /home/me/bot.py".into(), dialog, reply }), rx)
    }

    fn ask(choices: &[&str], default: &str) -> Dialog {
        Dialog::Ask { question: "Which branch should I deploy?".into(), choices: choices.iter().map(|c| (*c).to_owned()).collect(), default: default.into() }
    }

    fn confirm() -> Dialog {
        Dialog::Confirm { question: "Deploy now?".into(), yes: "Deploy".into(), no: String::new() }
    }

    #[test]
    fn stops_and_focus() {
        let (v, _) = view(ask(&[], "main"));
        assert_eq!(v.stops(), [Stop::Field, Stop::Cancel, Stop::Answer]);
        assert_eq!((v.focus, v.entry.as_str()), (0, "main"), "the field, with the default filled in");
        let (v, _) = view(ask(&["main", "dev", "test"], "dev"));
        assert_eq!(v.stops(), [Stop::Choice(0), Stop::Choice(1), Stop::Choice(2), Stop::Cancel]);
        assert_eq!(v.focus, 1, "the default choice");
        let (v, _) = view(confirm());
        assert_eq!(v.stops(), [Stop::No, Stop::Yes]);
        assert_eq!(v.focused(), Some(Stop::No), "No first, as Deny in the consent prompt");
        assert_eq!((stop_label(&v, Stop::Yes), stop_label(&v, Stop::No)), ("Deploy".to_owned(), "No".to_owned()));
    }

    #[test]
    fn answers() {
        let (mut v, mut rx) = view(ask(&[], ""));
        v.type_text("feature/x\u{7}");
        assert_eq!(v.entry, "feature/x", "control characters are not typed");
        v.answer(v.reply_of(Stop::Answer).unwrap());
        v.answer(Reply::Cancelled);
        assert_eq!(rx.try_recv(), Ok(Reply::Text("feature/x".into())), "the first answer counts");
        let (v, mut rx) = view(confirm());
        drop(v);
        assert_eq!(rx.try_recv(), Ok(Reply::Cancelled), "gone unanswered: cancelled");
        let (mut v, _) = view(ask(&[], ""));
        v.type_text(&"x".repeat(ANSWER_MAX + 10));
        assert_eq!(v.entry.chars().count(), ANSWER_MAX);
    }

    #[test]
    fn keys() {
        let typed = [Stop::Field, Stop::Cancel, Stop::Answer];
        assert_eq!(key_action(Keysym::Return, None, 0, &typed), Some(Key::Press(2)), "Enter in the field answers");
        assert_eq!(key_action(Keysym::Return, None, 1, &typed), Some(Key::Press(1)));
        assert_eq!(key_action(Keysym::space, Some(" "), 0, &typed), Some(Key::Type(" ".into())));
        assert_eq!(key_action(Keysym::a, Some("a"), 0, &typed), Some(Key::Type("a".into())));
        assert_eq!(key_action(Keysym::BackSpace, None, 0, &typed), Some(Key::Erase));
        assert_eq!(key_action(Keysym::Escape, None, 0, &typed), Some(Key::Cancel));
        assert_eq!(key_action(Keysym::Tab, None, 2, &typed), Some(Key::Focus(0)));
        let choices = [Stop::Choice(0), Stop::Choice(1), Stop::Cancel];
        assert_eq!(key_action(Keysym::Down, None, 0, &choices), Some(Key::Focus(1)));
        assert_eq!(key_action(Keysym::Up, None, 0, &choices), Some(Key::Focus(0)));
        assert_eq!(key_action(Keysym::space, Some(" "), 1, &choices), Some(Key::Press(1)));
        assert_eq!(key_action(Keysym::a, Some("a"), 1, &choices), None);
    }

    #[test]
    fn names_the_agent_and_program() {
        let (mut v, _) = view(confirm());
        let [band, title, program, question, footer] = texts(&v);
        assert_eq!(band, "Question from an agent");
        assert_eq!(title, "“Build bot” asks you to confirm");
        assert!(program.contains("/usr/bin/python3 /home/me/bot.py"));
        assert_eq!(question, "Deploy now?");
        assert!(footer.contains("not from Helix"));
        v.name = "Sys\u{202E}tem".into();
        assert_eq!(texts(&v)[1], "“System” asks you to confirm", "reordering characters in the name go");
        v.name.clear();
        assert_eq!(texts(&v)[1], "“python3” asks you to confirm");
    }

    #[test]
    fn paints_every_stop() {
        let (w, h) = (1024usize, 768usize);
        for dialog in [ask(&[], ""), ask(&["a", "b", "c"], ""), confirm()] {
            let (v, _) = view(dialog);
            let mut data = vec![0u8; w * h * 4];
            let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
            let (hits, node) = paint(&mut canvas, (None, None), (w, h), &v, (None, None), None);
            assert_eq!(hits.len(), v.stops().len());
            assert_eq!(node.role, Role::Dialog);
            assert!(hits.iter().all(|(r, _)| r.y + r.h < h as i32));
        }
    }
}
