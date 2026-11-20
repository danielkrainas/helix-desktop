//! Agent Access's prompts (helixdesktop/spec
//! `contracts/agent-access/README.md`, "Consent" and "Sensitive actions";
//! helixdesktop/desktop#143): may this agent use the desktop, and may it
//! do this sensitive action.
//!
//! Only the user at the machine answers them:
//!
//! - the answer goes back on a channel only this dialog holds; no D-Bus
//!   method answers it;
//! - its surface's namespace is [`NAMESPACE`], and the compositor drops
//!   remote and test input (remote desktop and the headless input socket,
//!   the ways synthetic input reaches it) while such a surface is shown;
//! - accessibility actions on it are refused (screen readers still read
//!   it; the keyboard works it);
//! - keys and clicks in its first [`ARM_DELAY`] are ignored, so that a
//!   prompt an agent times to appear under a click or an Enter meant for
//!   something else is not answered by it;
//! - *Deny* has the focus at first.

use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::WaylandSurface;

use super::{label, physical, size, theme, Canvas, DialogKind, DialogState, Overlay, OverlayHit, Rect, Ui, DIM_FG, FG, SCRIM};
use crate::a11y::{Node, Role};
use crate::agent::grants::Answer;
use crate::agent::policy::Level;
use crate::agent::prompt::{Question, Request};
use helix_i18n::{tr, trf};

/// The layer-shell namespace of the prompt's surface; the compositor
/// keeps synthetic input away from surfaces of this namespace.
pub(super) const NAMESPACE: &str = "helix-agent-prompt";

/// Keys and clicks this soon after the prompt is drawn are ignored.
pub(super) const ARM_DELAY: Duration = Duration::from_millis(1000);

/// An agent prompt on screen.
pub(super) struct AgentPromptView {
    request: Option<Request>,
    /// The agent's display name, its id and the question (kept after the
    /// answer went).
    name: String,
    agent: String,
    question: Question,
    /// The button with the keyboard focus (0: Deny).
    focus: usize,
}

impl AgentPromptView {
    pub(super) fn new(request: Request) -> Self {
        Self { name: request.name.clone(), agent: request.agent.clone(), question: request.question.clone(), request: Some(request), focus: 0 }
    }

    /// The buttons, left to right: Deny first.
    fn buttons(&self) -> Vec<(&'static str, Answer)> {
        match self.question {
            Question::Consent { offer_always, .. } => {
                let mut out = vec![(tr("Deny"), Answer::Deny), (tr("Allow for This Session"), Answer::Session)];
                if offer_always {
                    out.push((tr("Always Allow"), Answer::Always));
                }
                out
            }
            Question::Confirm { .. } => vec![(tr("Deny"), Answer::Deny), (tr("Allow"), Answer::Session)],
        }
    }

    /// Answer (once).
    fn answer(&mut self, answer: Answer) {
        if let Some(request) = self.request.take() {
            tracing::info!(agent = %request.agent, ?answer, "agent prompt answered");
            let _ = request.reply.send(answer);
        }
    }
}

impl Drop for AgentPromptView {
    /// A prompt that goes away unanswered (locked, replaced) grants nothing.
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ = request.reply.send(Answer::Unanswered);
        }
    }
}

/// The level as Settings names it.
pub(super) fn level_name(level: Level) -> &'static str {
    match level {
        Level::ReadOnly => tr("Read only"),
        Level::Desktop => tr("Control the desktop"),
        Level::Apps => tr("Control apps"),
        Level::ScreenAndInput => tr("Screen and input"),
    }
}

/// What a level lets an agent do, in a sentence.
fn level_text(level: Level) -> &'static str {
    match level {
        Level::ReadOnly => tr("See your open windows, apps, notifications and recent files, and ask you questions."),
        Level::Desktop => tr("Also open files and apps, manage windows, and change quick settings, volume and brightness."),
        Level::Apps => tr("Also read and use the controls inside your apps."),
        Level::ScreenAndInput => tr("Also take screenshots and use your keyboard and pointer."),
    }
}

/// What a sensitive call does, as the question's title.
fn action_title(who: &str, method: &str, action: Option<&str>) -> String {
    match action {
        Some("wifi-on") => return trf!("Allow {agent} to turn Wi-Fi on?", agent = who),
        Some("wifi-off") => return trf!("Allow {agent} to turn Wi-Fi off?", agent = who),
        Some("airplane-mode-on") => return trf!("Allow {agent} to turn airplane mode on?", agent = who),
        Some("airplane-mode-off") => return trf!("Allow {agent} to turn airplane mode off?", agent = who),
        Some("close") => return trf!("Allow {agent} to close a window?", agent = who),
        _ => {}
    }
    match method {
        "Open" => trf!("Allow {agent} to open a file or link?", agent = who),
        "Launch" => trf!("Allow {agent} to start an app?", agent = who),
        "WindowAction" => trf!("Allow {agent} to change a window?", agent = who),
        "SwitchWorkspace" => trf!("Allow {agent} to switch workspaces?", agent = who),
        "SetToggle" => trf!("Allow {agent} to change a quick setting?", agent = who),
        "SetValue" => trf!("Allow {agent} to change the volume or brightness?", agent = who),
        "Notify" => trf!("Allow {agent} to show a notification?", agent = who),
        "SetClipboard" => trf!("Allow {agent} to change the clipboard?", agent = who),
        "A11yAction" => trf!("Allow {agent} to use a control in an app?", agent = who),
        "A11ySetValue" => trf!("Allow {agent} to change a value in an app?", agent = who),
        "Input" => trf!("Allow {agent} to use your keyboard and pointer?", agent = who),
        _ => trf!("Allow {agent} to do this?", agent = who),
    }
}

/// What is painted: the title and the lines below it (text, dim).
fn content(view: &AgentPromptView) -> (String, Vec<(String, bool)>) {
    // The name is the agent's own claim: the program is shown with it.
    let who = if view.name.is_empty() {
        let exe = view.agent.split(' ').next().unwrap_or(&view.agent);
        format!("“{}”", exe.rsplit('/').next().unwrap_or(exe))
    } else {
        format!("“{}”", view.name)
    };
    let program = trf!("Program: {program}", program = view.agent);
    match &view.question {
        Question::Consent { level, .. } => (
            trf!("Allow {agent} to use your desktop?", agent = who),
            vec![
                (program, true),
                (trf!("Access: {level}", level = level_name(*level)), false),
                (level_text(*level).to_owned(), true),
                (tr("While it is active, the top bar shows it, and you can stop it there.").to_owned(), true),
            ],
        ),
        Question::Confirm { method, action, control } => {
            let mut lines = vec![(program, true)];
            // The control in the app, by the app's own words: it says
            // what is pressed or filled in.
            if let Some(c) = control {
                let name = if c.name.is_empty() { c.role.to_owned() } else { format!("“{}” ({})", c.name, c.role) };
                lines.push((trf!("Control: {control} in {app}", control = name, app = c.app), false));
            }
            lines.push((tr("You asked to confirm actions agents mark as sensitive, such as deleting, sending or changing settings.").to_owned(), true));
            (action_title(&who, method, *action), lines)
        }
    }
}

/// Paint the prompt over a scrim on a `w`×`h` surface. The clickable
/// buttons and the accessible tree.
pub(super) fn paint(canvas: &mut Canvas, (text, bold): (Option<&crate::text::Text>, Option<&crate::text::Text>), (w, h): (usize, usize), view: &AgentPromptView, (hover, pressed): (Option<OverlayHit>, Option<OverlayHit>)) -> (Vec<(Rect, OverlayHit)>, Node) {
    canvas.fill(SCRIM);
    let grow = helix_draw::theme::grow;
    let buttons = view.buttons();
    let bw = grow(if buttons.len() > 2 { 600 } else { 480 }).min(w as i32 - 32);
    let inner = (bw - 48) as f32;
    let (title, lines) = content(view);
    let title_face = bold.or(text);
    let title_lines = title_face.map_or_else(|| vec![title.clone()], |t| t.wrap(&title, size::TITLE, inner, 3));
    let wrapped: Vec<(Vec<String>, bool)> = lines.iter().map(|(s, dim)| (text.map_or_else(|| vec![s.clone()], |t| t.wrap(s, size::LABEL, inner, 4)), *dim)).collect();
    let body_lines: i32 = wrapped.iter().map(|(l, _)| l.len() as i32).sum();
    let gaps = wrapped.len() as i32 - 1;
    let bh = grow(36) + title_lines.len() as i32 * grow(26) + grow(14) + body_lines * grow(19) + gaps * grow(8) + grow(24) + grow(36) + grow(24);
    let bx = (w as i32 - bw) / 2;
    let by = ((h as i32 - bh) / 3).max(8);
    theme::popover(canvas, bx, by, bw, bh);
    let mut y = by + grow(36);
    for line in &title_lines {
        let tw = title_face.map_or(0.0, |t| t.width(line, size::TITLE).min(inner));
        label(title_face, canvas, line, bx as f32 + (bw as f32 - tw) / 2.0, y as f32 + grow(18) as f32, size::TITLE, FG, inner);
        y += grow(26);
    }
    y += grow(14);
    for (lines, dim) in &wrapped {
        for line in lines {
            label(text, canvas, line, (bx + 24) as f32, (y + grow(14)) as f32, size::LABEL, if *dim { DIM_FG } else { FG }, inner);
            y += grow(19);
        }
        y += grow(8);
    }
    // The buttons, equal widths, Deny on the left.
    let n = buttons.len() as i32;
    let gap = 12;
    let button_w = (bw - 48 - gap * (n - 1)) / n;
    let button_y = by + bh - grow(24) - grow(36);
    let mut hits = Vec::new();
    let mut node = Node::new(Role::Dialog, &title).id("agent-prompt").modal();
    for (line, _) in &lines {
        node = node.child(Node::label(line.as_str()));
    }
    for (i, (name, _)) in buttons.iter().enumerate() {
        let r = Rect { x: bx + 24 + i as i32 * (button_w + gap), y: button_y, w: button_w, h: grow(36) };
        let hit = OverlayHit::AgentButton(i);
        let look = theme::Look { hovered: hover == Some(hit), pressed: pressed == Some(hit), ..Default::default() };
        theme::button(canvas, text, (r.x, r.y, r.w, r.h), name, theme::Kind::Normal, look, view.focus == i);
        hits.push((r, hit));
        node = node.child(Node::button(*name, r.into()).id(format!("agent-button:{i}")).focused(view.focus == i));
    }
    (hits, node)
}

/// What a key does in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    Focus(usize),
    Press(usize),
    /// Escape: Deny.
    Deny,
}

/// What `keysym` does with button `focus` of `count` focused.
pub(super) fn key_action(keysym: Keysym, focus: usize, count: usize) -> Option<Key> {
    match keysym {
        Keysym::Escape => Some(Key::Deny),
        Keysym::Return | Keysym::KP_Enter | Keysym::space => Some(Key::Press(focus)),
        Keysym::Tab | Keysym::Right => Some(Key::Focus((focus + 1) % count)),
        Keysym::ISO_Left_Tab | Keysym::Left => Some(Key::Focus((focus + count - 1) % count)),
        _ => None,
    }
}

/// Whether input at `now` reaches a prompt first drawn at `shown`.
pub(super) fn armed(shown: Option<Instant>, now: Instant) -> bool {
    shown.is_some_and(|shown| now.saturating_duration_since(shown) >= ARM_DELAY)
}

impl Ui {
    /// Ask the user about an agent: now, or once the dialog shown (a
    /// password prompt, another question) is gone.
    pub(super) fn show_agent_prompt(&mut self, request: Request) {
        if self.lock.session.is_some() {
            // Locked: nobody is asked (the request answers "unanswered").
            return;
        }
        if self.dialog.is_some() {
            tracing::info!(agent = %request.agent, "agent prompt waits for the dialog shown");
            // An earlier waiting one goes unanswered.
            self.agent_waiting = Some(AgentPromptView::new(request));
            return;
        }
        tracing::info!(agent = %request.agent, "agent prompt shown");
        let state = DialogState::new(DialogKind::Agent(Box::new(AgentPromptView::new(request))), 0);
        self.power_menu = None;
        self.close_launcher(true);
        self.close_window_menu();
        self.dialog = Some(self.new_overlay(NAMESPACE, KeyboardInteractivity::Exclusive, state));
    }

    /// A prompt that waited for another dialog, once that is gone; then
    /// agents' questions waiting for their turn.
    pub(super) fn agent_prompt_next(&mut self) {
        if self.dialog.is_some() || self.lock.session.is_some() {
            return;
        }
        if let Some(mut view) = self.agent_waiting.take() {
            if let Some(request) = view.request.take() {
                self.show_agent_prompt(request);
            }
        }
        self.agent_question_next();
    }

    /// Whether `surface` is an agent prompt's or an agent's question's.
    pub(super) fn is_agent_prompt(&self, surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface) -> bool {
        self.dialog.as_ref().is_some_and(|d| d.layer.wl_surface() == surface && matches!(d.state.kind, DialogKind::Agent(_) | DialogKind::AgentQuestion(_)))
    }

    /// Dismiss the agent prompt unanswered and cancel agents' questions
    /// (the screen locks).
    pub(super) fn dismiss_agent_prompt(&mut self) {
        self.agent_waiting = None;
        self.agent_questions.clear();
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::Agent(_) | DialogKind::AgentQuestion(_))) {
            self.dialog = None;
        }
    }

    /// Press button `index`, if the prompt is armed.
    pub(super) fn agent_prompt_press(&mut self, index: usize) {
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::AgentQuestion(_))) {
            return self.agent_question_press(index);
        }
        let Some(dialog) = self.dialog.as_mut() else { return };
        if !armed(dialog.state.shown, Instant::now()) {
            tracing::debug!("agent prompt not armed yet; input ignored");
            return;
        }
        let DialogKind::Agent(view) = &mut dialog.state.kind else { return };
        let Some(&(_, answer)) = view.buttons().get(index) else { return };
        crate::click_feedback();
        view.answer(answer);
        self.dialog = None;
        self.agent_prompt_next();
    }

    pub(super) fn agent_prompt_key(&mut self, event: &KeyEvent) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let DialogKind::Agent(view) = &mut dialog.state.kind else { return };
        let count = view.buttons().len();
        match key_action(event.keysym, view.focus, count) {
            Some(Key::Focus(i)) => {
                view.focus = i;
                self.draw_dialog();
            }
            Some(Key::Press(i)) => self.agent_prompt_press(i),
            Some(Key::Deny) => self.agent_prompt_press(0),
            None => {}
        }
    }

    pub(super) fn paint_agent_prompt(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::Agent(view) = &overlay.state.kind else { return };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.buffers.create_buffer(overlay.layer.wl_surface(), Ui::draw_dialog, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        let (hits, node) = paint(&mut canvas, (self.text.as_ref(), self.bold.as_ref()), (w, h), view, (overlay.state.hover, overlay.state.pressed));
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

    fn view(question: Question) -> (AgentPromptView, oneshot::Receiver<Answer>) {
        let (reply, rx) = oneshot::channel();
        (AgentPromptView::new(Request { name: "Build bot".into(), agent: "/usr/bin/python3 /home/me/bot.py".into(), question, reply }), rx)
    }

    #[test]
    fn buttons_and_answers() {
        let (v, _) = view(Question::Consent { level: Level::Desktop, offer_always: true });
        assert_eq!(v.buttons().iter().map(|b| b.1).collect::<Vec<_>>(), [Answer::Deny, Answer::Session, Answer::Always]);
        let (v, _) = view(Question::Consent { level: Level::Desktop, offer_always: false });
        assert_eq!(v.buttons().len(), 2, "no Always Allow for unidentified agents or under remember-mode ask");
        let (v, _) = view(Question::Confirm { method: "Open", action: None, control: None });
        assert_eq!(v.buttons().iter().map(|b| b.1).collect::<Vec<_>>(), [Answer::Deny, Answer::Session]);
        // Deny has the focus at first.
        assert_eq!(v.focus, 0);
    }

    #[test]
    fn dropped_unanswered() {
        let (mut v, mut rx) = view(Question::Consent { level: Level::ReadOnly, offer_always: false });
        drop(v);
        assert_eq!(rx.try_recv(), Ok(Answer::Unanswered));
        let (v2, mut rx) = view(Question::Consent { level: Level::ReadOnly, offer_always: false });
        v = v2;
        v.answer(Answer::Session);
        v.answer(Answer::Deny);
        drop(v);
        assert_eq!(rx.try_recv(), Ok(Answer::Session), "the first answer counts");
    }

    #[test]
    fn keys() {
        assert_eq!(key_action(Keysym::Escape, 2, 3), Some(Key::Deny));
        assert_eq!(key_action(Keysym::Return, 1, 3), Some(Key::Press(1)));
        assert_eq!(key_action(Keysym::Tab, 2, 3), Some(Key::Focus(0)));
        assert_eq!(key_action(Keysym::ISO_Left_Tab, 0, 3), Some(Key::Focus(2)));
        assert_eq!(key_action(Keysym::Left, 0, 2), Some(Key::Focus(1)));
        assert_eq!(key_action(Keysym::a, 0, 2), None);
    }

    #[test]
    fn arming() {
        let now = Instant::now();
        assert!(!armed(None, now), "not drawn yet");
        assert!(!armed(Some(now), now + Duration::from_millis(300)));
        assert!(armed(Some(now), now + ARM_DELAY));
    }

    #[test]
    fn names_the_program() {
        let (v, _) = view(Question::Consent { level: Level::Apps, offer_always: true });
        let (title, lines) = content(&v);
        assert_eq!(title, "Allow “Build bot” to use your desktop?");
        assert!(lines[0].0.contains("/usr/bin/python3 /home/me/bot.py"));
        assert!(lines[1].0.contains("Control apps"));
        let (mut v, _) = view(Question::Confirm { method: "Input", action: None, control: None });
        v.name.clear();
        assert_eq!(content(&v).0, "Allow “python3” to use your keyboard and pointer?");
        // What the action does, where the prompt can say.
        let (v, _) = view(Question::Confirm { method: "SetToggle", action: Some("wifi-off"), control: None });
        assert_eq!(content(&v).0, "Allow “Build bot” to turn Wi-Fi off?");
        let (v, _) = view(Question::Confirm { method: "WindowAction", action: Some("close"), control: None });
        assert_eq!(content(&v).0, "Allow “Build bot” to close a window?");
        let (v, _) = view(Question::Confirm { method: "WindowAction", action: None, control: None });
        assert_eq!(content(&v).0, "Allow “Build bot” to change a window?");
        // An app's control: named with the app.
        let control = crate::agent::Control { name: "Delete All".into(), role: "button", app: "org.example.Files".into() };
        let (v, _) = view(Question::Confirm { method: "A11yAction", action: None, control: Some(control) });
        let (title, lines) = content(&v);
        assert_eq!(title, "Allow “Build bot” to use a control in an app?");
        assert_eq!(lines[1].0, "Control: “Delete All” (button) in org.example.Files");
    }

    #[test]
    fn paints_buttons() {
        let (v, _) = view(Question::Consent { level: Level::Desktop, offer_always: true });
        let (w, h) = (1024usize, 768usize);
        let mut data = vec![0u8; w * h * 4];
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
        let (hits, node) = paint(&mut canvas, (None, None), (w, h), &v, (None, None));
        assert_eq!(hits.len(), 3);
        assert!(hits.windows(2).all(|p| p[0].0.x + p[0].0.w < p[1].0.x), "left to right, apart");
        assert_eq!(node.role, Role::Dialog);
    }
}
