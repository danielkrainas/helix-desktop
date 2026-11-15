//! "Extensions…" (helix-desktop-spec `contracts/extensions/README.md`,
//! "The Extensions dialog"): the extensions, turning them on and off,
//! and their settings. The fallback for when the Settings app (whose
//! Extensions panel does more) is not installed.

use super::*;
use crate::ext_host::manifest::{permission_text, SettingType, SettingValue};
use crate::ext_host::{Info, State};
use helix_i18n::tr;
use helix_i18n::trf;

const BOX_W: i32 = 640;
const PAD: i32 = 18;
const ENTRY_H: i32 = 92;
const SETTING_H: i32 = 36;
const SWITCH_W: i32 = theme::SWITCH_W as i32;
const SMALL: i32 = 28;

#[derive(Debug, Default)]
pub(super) struct DialogState {
    focus: Option<usize>,
    /// The extension whose settings are shown.
    expanded: Option<String>,
    /// Entries scrolled past.
    scroll: usize,
    /// The control under the pointer, and the one pressed.
    pub hover: Option<DialogHit>,
    pub pressed: Option<DialogHit>,
}

impl DialogState {
    fn look(&self, hit: DialogHit) -> theme::Look {
        theme::Look { hovered: self.hover == Some(hit), pressed: self.pressed == Some(hit), ..Default::default() }
    }
}

/// A button in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DialogHit {
    /// Turn extension `n` on or off.
    Switch(usize),
    /// Show or hide its settings.
    Settings(usize),
    /// Setting `s` of extension `n`: flip, next choice, down, up.
    Flip(usize, usize),
    Next(usize, usize),
    Down(usize, usize),
    Up(usize, usize),
    Done,
}

/// The Settings app (helix-apps), when installed.
const SETTINGS_APP: &str = "helix-settings-app";

/// Open Settings' Extensions panel; false when Settings is not installed
/// or does not start.
pub(super) fn open_in_settings() -> bool {
    let installed = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(SETTINGS_APP).is_file()));
    if !installed {
        return false;
    }
    let app = gio::AppInfo::create_from_commandline(format!("{SETTINGS_APP} extensions"), Some(tr("Settings")), gio::AppInfoCreateFlags::NONE);
    match app.and_then(|a| gio::prelude::AppInfoExt::launch(&a, &[], None::<&gio::AppLaunchContext>)) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(%err, "cannot open Settings");
            false
        }
    }
}

fn state_text(info: &Info) -> String {
    match &info.state {
        State::Running => tr("Running").into(),
        State::Starting => tr("Starting").into(),
        State::Stopped => tr("Not running").into(),
        State::Disabled if info.error == "Off for this session" => tr("Off for this session (Shift was held at login)").into(),
        State::Disabled => tr("Off").into(),
        State::NeedsApproval => tr("Needs approval: turn it on to allow what it asks").into(),
        State::Invalid(reason) => trf!("Cannot run: {reason}", reason = reason),
        State::Crashed(last) if last.is_empty() => tr("Stopped working").into(),
        State::Crashed(last) => trf!("Stopped working: {last}", last = last),
    }
}

/// What an extension can do: sandboxed, with its permissions, or anything.
fn can_text(info: &Info) -> String {
    if info.sandboxed {
        let what: Vec<&str> = info.permissions.iter().map(|p| permission_text(p)).collect();
        if what.is_empty() { tr("Sandboxed").to_owned() } else { trf!("Sandboxed. Can: {}", what.join(", ")) }
    } else {
        tr("Full access: it can do anything you can").to_owned()
    }
}

/// The Extensions dialog on the accessibility bus (helix-desktop-spec
/// `contracts/dbus/org.a11y.atspi.md`): a modal `dialog` with a list of
/// the extensions shown (`shown`, indexes into `list`), each a `list item`
/// named by the extension and described by its description, state and
/// what it can do, holding its switch (a toggle button, checked when on),
/// its `Settings` button (expanded while open), its commands and keys
/// (labels), and when open its settings: switches, choices (buttons
/// described by the value), numbers (`Decrease` / `Increase` described by
/// the setting and value) and text (labels). Then "Scroll for more" when
/// some are out of view, and `Done`. The control with the keyboard focus
/// (Tab) has it.
pub(super) fn extension_nodes(list: &[Info], shown: &[usize], expanded: Option<&str>, hits: &[(Rect, DialogHit)], focus: Option<usize>, more: bool) -> Vec<crate::a11y::Node> {
    use crate::a11y::{Node, Role};
    let control = |i: usize, node: Node| node.at(hits[i].0.into()).click().focusable().focused(focus == Some(i)).id(format!("{:?}", hits[i].1));
    let hit = |want: DialogHit| hits.iter().position(|(_, h)| *h == want);
    let mut out = Node::new(Role::Dialog, tr("Extensions")).id("extensions").modal();
    if list.is_empty() {
        out = out.child(Node::label(tr("No extensions are installed.")));
    }
    let entries = shown.iter().filter_map(|&n| {
        let info = list.get(n)?;
        let parts = [info.description.clone(), state_text(info), can_text(info)];
        let about = parts.iter().map(|s| s.trim().trim_end_matches('.')).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(". ");
        let title = if info.version.is_empty() { info.name.clone() } else { format!("{} {}", info.name, info.version) };
        let mut entry = Node::new(Role::ListItem, title).id(info.id.clone()).describe(about);
        if let Some(i) = hit(DialogHit::Switch(n)) {
            entry = entry.child(control(i, Node::new(Role::Switch, &info.name).toggled(info.enabled && info.approved)));
        }
        if let Some(i) = hit(DialogHit::Settings(n)) {
            entry = entry.child(control(i, Node::new(Role::Button, tr("Settings")).describe(info.name.as_str()).expanded(expanded == Some(info.id.as_str()))));
        }
        for (_, title, keys) in &info.commands {
            let keys = if keys.is_empty() { tr("None") } else { keys.as_str() };
            entry = entry.child(Node::label(format!("{title}: {keys}")));
        }
        if expanded == Some(info.id.as_str()) {
            for (s, (decl, value)) in info.settings.iter().enumerate() {
                let shown = value.display();
                match &decl.ty {
                    SettingType::Bool => {
                        if let Some(i) = hit(DialogHit::Flip(n, s)) {
                            entry = entry.child(control(i, Node::new(Role::Switch, &decl.label).toggled(*value == SettingValue::Bool(true))));
                        }
                    }
                    SettingType::Enum(_) => {
                        if let Some(i) = hit(DialogHit::Next(n, s)) {
                            entry = entry.child(control(i, Node::new(Role::Button, &decl.label).describe(shown)));
                        }
                    }
                    SettingType::Int | SettingType::Double => {
                        let about = format!("{}: {shown}", decl.label);
                        for (want, name) in [(DialogHit::Down(n, s), tr("Decrease")), (DialogHit::Up(n, s), tr("Increase"))] {
                            if let Some(i) = hit(want) {
                                entry = entry.child(control(i, Node::new(Role::Button, name).describe(about.as_str())));
                            }
                        }
                    }
                    SettingType::String | SettingType::StringList => {
                        let shown = if shown.is_empty() { tr("(empty)").to_owned() } else { shown };
                        entry = entry.child(Node::label(format!("{}: {shown}", decl.label)));
                    }
                }
            }
        }
        Some(entry)
    });
    out = out.child(Node::new(Role::List, "").id("list").children(entries));
    if more {
        out = out.child(Node::label(tr("Scroll for more")).id("more"));
    }
    if let Some(i) = hit(DialogHit::Done) {
        out = out.child(control(i, Node::new(Role::Button, tr("Done"))));
    }
    vec![out]
}

/// A number setting's step.
fn step(ty: &SettingType, min: Option<f64>, max: Option<f64>) -> f64 {
    match (ty, min, max) {
        (SettingType::Int, _, _) => 1.0,
        (_, Some(lo), Some(hi)) if hi > lo => (hi - lo) / 20.0,
        _ => 0.1,
    }
}

impl Ui {
    pub(super) fn open_extensions_dialog(&mut self) {
        if self.extensions_dialog.is_some() {
            return;
        }
        self.close_launcher(true);
        self.power_menu = None;
        self.leave_panel();
        self.extensions_dialog = Some(self.new_overlay("helix-extensions", KeyboardInteractivity::Exclusive, DialogState::default()));
    }

    pub(super) fn draw_extensions_dialog(&mut self) {
        let Some(mut overlay) = self.extensions_dialog.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_extensions_dialog(&mut overlay);
        }
        self.extensions_dialog = Some(overlay);
    }

    fn paint_extensions_dialog(&mut self, overlay: &mut Overlay<DialogState>) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let list = self.ext_list.clone();
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(overlay.layer.wl_surface()));
        canvas.fill(0x9900_0000);
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        let bw = BOX_W.min(w as i32 - 32);
        let bh = (h as i32 - panel_height() as i32 - 64).min(720);
        let (bx, by) = ((w as i32 - bw) / 2, panel_height() as i32 + 32);
        theme::popover(&mut canvas, bx, by, bw, bh);
        label(bold.or(text), &mut canvas, tr("Extensions"), (bx + PAD) as f32, (by + 36) as f32, size::TITLE, FG, (bw - 2 * PAD) as f32);
        let mut hits: Vec<(Rect, DialogHit)> = Vec::new();
        let bottom = by + bh - PAD - 36;
        let mut y = by + 56;
        if list.is_empty() {
            label(text, &mut canvas, tr("No extensions are installed."), (bx + PAD) as f32, (y + 20) as f32, size::LABEL, DIM_FG, (bw - 2 * PAD) as f32);
        }
        let scroll = overlay.state.scroll.min(list.len().saturating_sub(1));
        overlay.state.scroll = scroll;
        let mut more_below = false;
        let mut shown = Vec::new();
        for (n, info) in list.iter().enumerate().skip(scroll) {
            let expanded = overlay.state.expanded.as_deref() == Some(info.id.as_str());
            let entry_h = ENTRY_H + if expanded { info.settings.len() as i32 * SETTING_H + 8 } else { 0 } + info.commands.len() as i32 * SETTING_H;
            if y + entry_h > bottom && n > scroll {
                more_below = true;
                break;
            }
            shown.push(n);
            theme::well(&mut canvas, bx + PAD, y, bw - 2 * PAD, entry_h - 8);
            let tx = bx + PAD + 12;
            let room = (bw - 2 * PAD - 24 - SWITCH_W - 12) as f32;
            let title = if info.version.is_empty() { info.name.clone() } else { format!("{}  {}", info.name, info.version) };
            label(bold.or(text), &mut canvas, &title, tx as f32, (y + 22) as f32, size::BODY, FG, room);
            label(text, &mut canvas, &info.description, tx as f32, (y + 42) as f32, size::CAPTION, DIM_FG, room);
            let state = state_text(info);
            let state_color = if matches!(info.state, State::Invalid(_) | State::Crashed(_) | State::NeedsApproval) { 0xFFE0_8040 } else { DIM_FG };
            label(text, &mut canvas, &state, tx as f32, (y + 62) as f32, size::CAPTION, state_color, room);
            let can = can_text(info);
            label(text, &mut canvas, &can, tx as f32, (y + 78) as f32, size::CAPTION, if info.sandboxed { DIM_FG } else { 0xFFE0_8040 }, room);
            // The on/off switch (development extensions run regardless).
            if info.kind != crate::ext_host::store::Kind::Development && !matches!(info.state, State::Invalid(_)) {
                let on = info.enabled && info.approved;
                let r = Rect { x: bx + bw - PAD - 12 - SWITCH_W, y: y + 12, w: SWITCH_W, h: 22 };
                theme::switch(&mut canvas, f64::from(r.x), f64::from(r.y), on, overlay.state.look(DialogHit::Switch(n)));
                hits.push((r, DialogHit::Switch(n)));
            }
            // (1.1) Commands and their keys (changed in Settings, or with
            // `helix-extensions bind`).
            let mut cy = y + ENTRY_H - 8;
            for (_, title, keys) in &info.commands {
                label(text, &mut canvas, title, (tx + 8) as f32, baseline(text, cy, SETTING_H, size::CAPTION), size::CAPTION, FG, 200.0);
                let shown = if keys.is_empty() { tr("None") } else { keys.as_str() };
                label(text, &mut canvas, shown, (tx + 216) as f32, baseline(text, cy, SETTING_H, size::CAPTION), size::CAPTION, DIM_FG, 220.0);
                cy += SETTING_H;
            }
            let commands_h = info.commands.len() as i32 * SETTING_H;
            if !info.settings.is_empty() {
                let name = if expanded { tr("Hide Settings") } else { tr("Settings") };
                let tw = text.map_or(0.0, |t| t.width(name, size::CAPTION)) as i32;
                let r = Rect { x: bx + bw - PAD - 12 - tw - 16, y: y + 46, w: tw + 16, h: 24 };
                theme::control(&mut canvas, r.x, r.y, r.w, r.h, overlay.state.look(DialogHit::Settings(n)));
                label(text, &mut canvas, name, (r.x + 8) as f32, baseline(text, r.y, r.h, size::CAPTION), size::CAPTION, FG, tw as f32 + 1.0);
                hits.push((r, DialogHit::Settings(n)));
            }
            if expanded {
                let mut sy = y + ENTRY_H - 8 + commands_h;
                for (s, (decl, value)) in info.settings.iter().enumerate() {
                    label(text, &mut canvas, &decl.label, (tx + 8) as f32, baseline(text, sy, SETTING_H, size::CAPTION), size::CAPTION, FG, 220.0);
                    let right = bx + bw - PAD - 12;
                    match &decl.ty {
                        SettingType::Bool => {
                            let on = *value == SettingValue::Bool(true);
                            let r = Rect { x: right - SWITCH_W, y: sy + 7, w: SWITCH_W, h: 22 };
                            theme::switch(&mut canvas, f64::from(r.x), f64::from(r.y), on, overlay.state.look(DialogHit::Flip(n, s)));
                            hits.push((r, DialogHit::Flip(n, s)));
                        }
                        SettingType::Enum(_) => {
                            let shown = value.display();
                            let tw = text.map_or(0.0, |t| t.width(&shown, size::CAPTION)) as i32;
                            let r = Rect { x: right - tw - 24, y: sy + 5, w: tw + 24, h: 26 };
                            theme::control(&mut canvas, r.x, r.y, r.w, r.h, overlay.state.look(DialogHit::Next(n, s)));
                            label(text, &mut canvas, &shown, (r.x + 12) as f32, baseline(text, r.y, r.h, size::CAPTION), size::CAPTION, FG, tw as f32 + 1.0);
                            hits.push((r, DialogHit::Next(n, s)));
                        }
                        SettingType::Int | SettingType::Double => {
                            let plus = Rect { x: right - SMALL, y: sy + 4, w: SMALL, h: SMALL };
                            let minus = Rect { x: right - 3 * SMALL - 60, y: sy + 4, w: SMALL, h: SMALL };
                            for (r, glyph, hit) in [(minus, "-", DialogHit::Down(n, s)), (plus, "+", DialogHit::Up(n, s))] {
                                theme::control(&mut canvas, r.x, r.y, r.w, r.h, overlay.state.look(hit));
                                let gw = text.map_or(0.0, |t| t.width(glyph, size::BODY));
                                label(text, &mut canvas, glyph, r.x as f32 + (r.w as f32 - gw) / 2.0, baseline(text, r.y, r.h, size::BODY), size::BODY, FG, r.w as f32);
                            }
                            let shown = value.display();
                            let tw = text.map_or(0.0, |t| t.width(&shown, size::CAPTION));
                            let mid = (minus.x + minus.w + plus.x) as f32 / 2.0;
                            label(text, &mut canvas, &shown, mid - tw / 2.0, baseline(text, sy, SETTING_H, size::CAPTION), size::CAPTION, FG, 80.0);
                            hits.push((minus, DialogHit::Down(n, s)));
                            hits.push((plus, DialogHit::Up(n, s)));
                        }
                        SettingType::String | SettingType::StringList => {
                            let shown = value.display();
                            let shown = if shown.is_empty() { tr("(empty)").to_owned() } else { shown };
                            let tw = text.map_or(0.0, |t| t.width(&shown, size::CAPTION)).min(240.0);
                            label(text, &mut canvas, &shown, right as f32 - tw, baseline(text, sy, SETTING_H, size::CAPTION), size::CAPTION, DIM_FG, 240.0);
                        }
                    }
                    sy += SETTING_H;
                }
            }
            y += entry_h;
        }
        if scroll > 0 || more_below {
            let hint = tr("Scroll for more");
            label(text, &mut canvas, hint, (bx + PAD) as f32, baseline(text, bottom, 36, size::CAPTION), size::CAPTION, DIM_FG, 200.0);
        }
        let done = Rect { x: bx + bw - PAD - 110, y: bottom, w: 110, h: 36 };
        theme::button(&mut canvas, text, (done.x, done.y, done.w, done.h), tr("Done"), theme::Kind::Suggested, overlay.state.look(DialogHit::Done), false);
        hits.push((done, DialogHit::Done));
        if let Some((r, hit)) = overlay.state.focus.and_then(|i| hits.get(i)).copied() {
            let radius = if matches!(hit, DialogHit::Switch(_) | DialogHit::Flip(..)) { 11.0 } else { theme::RADIUS_BUTTON };
            theme::focus_ring_round(&mut canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), radius);
        }
        let nodes = extension_nodes(&list, &shown, overlay.state.expanded.as_deref(), &hits, overlay.state.focus, scroll > 0 || more_below);
        self.atspi_record(overlay.layer.wl_surface(), nodes);
        overlay.hits = hits.into_iter().map(|(r, hit)| (r, OverlayHit::ExtDialog(hit))).collect();
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// The dialog for the accessibility bus.
    pub(super) fn extensions_dialog_a11y(&self) -> Option<(&LayerSurface, (u32, u32))> {
        let o = self.extensions_dialog.as_ref().filter(|o| o.configured && o.width > 0)?;
        Some((&o.layer, (o.width, o.height)))
    }

    fn extensions_dialog_do(&mut self, hit: DialogHit) {
        crate::click_feedback();
        let info = |n: usize| self.ext_list.get(n).cloned();
        match hit {
            DialogHit::Done => {
                self.extensions_dialog = None;
                return;
            }
            DialogHit::Switch(n) => {
                if let Some(i) = info(n) {
                    let on = !(i.enabled && i.approved);
                    let _ = self.requests.send(UiRequest::ExtEnable(i.id, on));
                }
            }
            DialogHit::Settings(n) => {
                if let (Some(i), Some(d)) = (info(n), self.extensions_dialog.as_mut()) {
                    d.state.expanded = if d.state.expanded.as_deref() == Some(i.id.as_str()) { None } else { Some(i.id) };
                }
            }
            DialogHit::Flip(n, s) | DialogHit::Next(n, s) | DialogHit::Down(n, s) | DialogHit::Up(n, s) => {
                let Some(i) = info(n) else { return };
                let Some((decl, value)) = i.settings.get(s).cloned() else { return };
                let new = match (&decl.ty, &value, hit) {
                    (SettingType::Bool, SettingValue::Bool(b), _) => Some(SettingValue::Bool(!b)),
                    (SettingType::Enum(choices), SettingValue::String(v), _) => {
                        let at = choices.iter().position(|c| c == v).map_or(0, |p| (p + 1) % choices.len());
                        Some(SettingValue::String(choices[at].clone()))
                    }
                    (SettingType::Int, SettingValue::Int(v), DialogHit::Down(..)) => Some(SettingValue::Int(v - 1)),
                    (SettingType::Int, SettingValue::Int(v), DialogHit::Up(..)) => Some(SettingValue::Int(v + 1)),
                    (SettingType::Double, SettingValue::Double(v), DialogHit::Down(..) | DialogHit::Up(..)) => {
                        let d = step(&decl.ty, decl.min, decl.max);
                        let next = if matches!(hit, DialogHit::Up(..)) { v + d } else { v - d };
                        Some(SettingValue::Double(next))
                    }
                    _ => None,
                };
                // Values beyond the limits stop at them.
                let new = new.map(|v| match (v, decl.min, decl.max) {
                    (SettingValue::Int(i), lo, hi) => SettingValue::Int((i as f64).clamp(lo.unwrap_or(f64::MIN), hi.unwrap_or(f64::MAX)) as i64),
                    (SettingValue::Double(d), lo, hi) => SettingValue::Double(d.clamp(lo.unwrap_or(f64::MIN), hi.unwrap_or(f64::MAX))),
                    (v, _, _) => v,
                });
                if let Some(new) = new.filter(|v| *v != value) {
                    let _ = self.requests.send(UiRequest::ExtSetting(i.id, decl.key, new));
                }
            }
        }
        self.draw_extensions_dialog();
    }

    /// A click on one of the dialog's controls.
    pub(super) fn extensions_dialog_press(&mut self, hit: DialogHit) {
        if let Some(d) = self.extensions_dialog.as_mut() {
            d.state.focus = None;
        }
        self.extensions_dialog_do(hit);
    }

    /// Scrolling the list.
    pub(super) fn extensions_dialog_scroll(&mut self, down: bool) {
        let len = self.ext_list.len();
        if let Some(d) = self.extensions_dialog.as_mut() {
            d.state.scroll = if down { (d.state.scroll + 1).min(len.saturating_sub(1)) } else { d.state.scroll.saturating_sub(1) };
            d.state.focus = None;
        }
        self.draw_extensions_dialog();
    }

    /// Tab and Shift+Tab move between the buttons, Enter or Space presses
    /// one, Escape closes.
    pub(super) fn extensions_dialog_key(&mut self, event: &KeyEvent) {
        let Some(dialog) = self.extensions_dialog.as_mut() else { return };
        let count = dialog.hits.len().max(1);
        match event.keysym {
            Keysym::Escape => self.extensions_dialog = None,
            Keysym::Tab | Keysym::Right | Keysym::Down => {
                dialog.state.focus = Some(dialog.state.focus.map_or(0, |i| (i + 1) % count));
                self.draw_extensions_dialog();
            }
            Keysym::ISO_Left_Tab | Keysym::Left | Keysym::Up => {
                dialog.state.focus = Some(dialog.state.focus.map_or(count - 1, |i| (i + count - 1) % count));
                self.draw_extensions_dialog();
            }
            Keysym::Page_Down => self.extensions_dialog_scroll(true),
            Keysym::Page_Up => self.extensions_dialog_scroll(false),
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                let hit = dialog.state.focus.and_then(|i| dialog.hits.get(i)).map(|(_, h)| *h);
                if let Some(OverlayHit::ExtDialog(hit)) = hit {
                    self.extensions_dialog_do(hit);
                }
            }
            _ => {}
        }
    }
}

impl Ui {
    /// The wheel over the dialog scrolls its list; true if it was for it.
    pub(super) fn extensions_dialog_axis(&mut self, event: &PointerEvent) -> bool {
        if !self.extensions_dialog.as_ref().is_some_and(|d| d.layer.wl_surface() == &event.surface) {
            return false;
        }
        if let PointerEventKind::Axis { vertical, .. } = event.kind {
            let delta = if vertical.discrete != 0 { f64::from(vertical.discrete) } else { vertical.absolute };
            if delta != 0.0 {
                self.extensions_dialog_scroll(delta > 0.0);
            }
            return true;
        }
        false
    }
}
