//! "Extensions…" (helix-desktop-spec `contracts/extensions/README.md`,
//! "The Extensions dialog"): the extensions, turning them on and off,
//! and their settings.

use super::*;
use crate::ext_host::manifest::{permission_text, SettingType, SettingValue};
use crate::ext_host::{Info, State};

const BOX_W: i32 = 640;
const PAD: i32 = 18;
const ENTRY_H: i32 = 92;
const SETTING_H: i32 = 36;
const SWITCH_W: i32 = 44;
const SMALL: i32 = 28;

#[derive(Debug, Default)]
pub(super) struct DialogState {
    focus: Option<usize>,
    /// The extension whose settings are shown.
    expanded: Option<String>,
    /// Entries scrolled past.
    scroll: usize,
    /// (1.1) The command waiting for its new key, and a refusal to show.
    capturing: Option<String>,
    refused: Option<(String, String)>,
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
    /// Command `c` of extension `n`: wait for a new key; clear it.
    ChangeKey(usize, usize),
    ClearKey(usize, usize),
    Done,
}

/// An accelerator for a key press: at least one of Ctrl, Alt or Super, or
/// a function key; `None` for a modifier alone or a plain key.
pub(super) fn accelerator_for(keysym: Keysym, mods: &Modifiers) -> Option<String> {
    let name = xkbcommon::xkb::keysym_get_name(keysym);
    if name.is_empty() || ["Shift", "Control", "Alt", "Super", "Meta", "Hyper", "ISO_Level3", "Caps_Lock", "Num_Lock"].iter().any(|m| name.starts_with(m)) {
        return None;
    }
    let function = name.len() > 1 && name.starts_with('F') && name[1..].parse::<u32>().is_ok();
    if !(mods.ctrl || mods.alt || mods.logo || function) {
        return None;
    }
    let mut out = String::new();
    for (on, text) in [(mods.logo, "<Super>"), (mods.ctrl, "<Control>"), (mods.alt, "<Alt>"), (mods.shift, "<Shift>")] {
        if on {
            out.push_str(text);
        }
    }
    // Letters by their lower-case names, as GNOME writes them.
    if name.chars().count() == 1 {
        out.push_str(&name.to_lowercase());
    } else {
        out.push_str(&name);
    }
    Some(out)
}

fn state_text(info: &Info) -> String {
    match &info.state {
        State::Running => "Running".into(),
        State::Starting => "Starting".into(),
        State::Stopped => "Not running".into(),
        State::Disabled => "Off".into(),
        State::NeedsApproval => "Needs approval: turn it on to allow what it asks".into(),
        State::Invalid(reason) => format!("Cannot run: {reason}"),
        State::Crashed(last) if last.is_empty() => "Stopped working".into(),
        State::Crashed(last) => format!("Stopped working: {last}"),
    }
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
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x9900_0000);
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        let bw = BOX_W.min(w as i32 - 32);
        let bh = (h as i32 - PANEL_HEIGHT as i32 - 64).min(720);
        let (bx, by) = ((w as i32 - bw) / 2, PANEL_HEIGHT as i32 + 32);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        for (rx, ry, rw, rh) in [(bx, by, bw, 1), (bx, by + bh - 1, bw, 1), (bx, by, 1, bh), (bx + bw - 1, by, 1, bh)] {
            canvas.rect(rx, ry, rw, rh, BANNER_BORDER);
        }
        label(bold.or(text), &mut canvas, "Extensions", (bx + PAD) as f32, (by + 36) as f32, 20.0, FG, (bw - 2 * PAD) as f32);
        let mut hits: Vec<(Rect, DialogHit)> = Vec::new();
        let bottom = by + bh - PAD - 36;
        let mut y = by + 56;
        if list.is_empty() {
            label(text, &mut canvas, "No extensions are installed.", (bx + PAD) as f32, (y + 20) as f32, 14.0, DIM_FG, (bw - 2 * PAD) as f32);
        }
        let scroll = overlay.state.scroll.min(list.len().saturating_sub(1));
        overlay.state.scroll = scroll;
        let mut more_below = false;
        for (n, info) in list.iter().enumerate().skip(scroll) {
            let expanded = overlay.state.expanded.as_deref() == Some(info.id.as_str());
            let entry_h = ENTRY_H + if expanded { info.settings.len() as i32 * SETTING_H + 8 } else { 0 } + info.commands.len() as i32 * SETTING_H;
            if y + entry_h > bottom && n > scroll {
                more_below = true;
                break;
            }
            canvas.rect(bx + PAD, y, bw - 2 * PAD, entry_h - 8, HOVER_BG);
            let tx = bx + PAD + 12;
            let room = (bw - 2 * PAD - 24 - SWITCH_W - 12) as f32;
            let title = if info.version.is_empty() { info.name.clone() } else { format!("{}  {}", info.name, info.version) };
            label(bold.or(text), &mut canvas, &title, tx as f32, (y + 22) as f32, 15.0, FG, room);
            label(text, &mut canvas, &info.description, tx as f32, (y + 42) as f32, 13.0, DIM_FG, room);
            let state = state_text(info);
            let state_color = if matches!(info.state, State::Invalid(_) | State::Crashed(_) | State::NeedsApproval) { 0xFFE0_8040 } else { DIM_FG };
            label(text, &mut canvas, &state, tx as f32, (y + 62) as f32, 12.0, state_color, room);
            if !info.permissions.is_empty() {
                let can = format!("Can: {}", info.permissions.iter().map(|p| permission_text(p)).collect::<Vec<_>>().join(", "));
                label(text, &mut canvas, &can, tx as f32, (y + 78) as f32, 12.0, DIM_FG, room);
            }
            // The on/off switch (development extensions run regardless).
            if info.kind != crate::ext_host::store::Kind::Development && !matches!(info.state, State::Invalid(_)) {
                let on = info.enabled && info.approved;
                let r = Rect { x: bx + bw - PAD - 12 - SWITCH_W, y: y + 12, w: SWITCH_W, h: 22 };
                canvas.rect(r.x, r.y, r.w, r.h, if on { ACCENT } else { FIELD_BG });
                let knob = if on { r.x + r.w - 20 } else { r.x + 2 };
                canvas.rect(knob, r.y + 2, 18, 18, FG);
                hits.push((r, DialogHit::Switch(n)));
            }
            // (1.1) Commands and their keys.
            let mut cy = y + ENTRY_H - 8;
            for (c, (cmd, title, keys)) in info.commands.iter().enumerate() {
                let full = format!("{}/{cmd}", info.id);
                let capturing = overlay.state.capturing.as_deref() == Some(full.as_str());
                let refused = overlay.state.refused.as_ref().filter(|(f, _)| *f == full).map(|(_, why)| why.clone());
                label(text, &mut canvas, title, (tx + 8) as f32, baseline(text, cy, SETTING_H, 13.0), 13.0, FG, 200.0);
                let shown = if capturing {
                    "Press keys… (Escape: cancel, Backspace: none)".to_owned()
                } else if let Some(why) = refused {
                    why
                } else if keys.is_empty() {
                    "None".to_owned()
                } else {
                    keys.clone()
                };
                label(text, &mut canvas, &shown, (tx + 216) as f32, baseline(text, cy, SETTING_H, 12.0), 12.0, if capturing { ACCENT } else { DIM_FG }, 220.0);
                let right = bx + bw - PAD - 12;
                for (k, (name, hit)) in [("Change", DialogHit::ChangeKey(n, c)), ("Clear", DialogHit::ClearKey(n, c))].into_iter().enumerate() {
                    let r = Rect { x: right - 140 + k as i32 * 74, y: cy + 5, w: 66, h: 26 };
                    canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                    let tw = text.map_or(0.0, |t| t.width(name, 12.0));
                    label(text, &mut canvas, name, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 12.0), 12.0, FG, r.w as f32);
                    hits.push((r, hit));
                }
                cy += SETTING_H;
            }
            let commands_h = info.commands.len() as i32 * SETTING_H;
            if !info.settings.is_empty() {
                let name = if expanded { "Hide Settings" } else { "Settings" };
                let tw = text.map_or(0.0, |t| t.width(name, 12.0)) as i32;
                let r = Rect { x: bx + bw - PAD - 12 - tw - 16, y: y + 46, w: tw + 16, h: 24 };
                canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                label(text, &mut canvas, name, (r.x + 8) as f32, baseline(text, r.y, r.h, 12.0), 12.0, FG, tw as f32 + 1.0);
                hits.push((r, DialogHit::Settings(n)));
            }
            if expanded {
                let mut sy = y + ENTRY_H - 8 + commands_h;
                for (s, (decl, value)) in info.settings.iter().enumerate() {
                    label(text, &mut canvas, &decl.label, (tx + 8) as f32, baseline(text, sy, SETTING_H, 13.0), 13.0, FG, 220.0);
                    let right = bx + bw - PAD - 12;
                    match &decl.ty {
                        SettingType::Bool => {
                            let on = *value == SettingValue::Bool(true);
                            let r = Rect { x: right - SWITCH_W, y: sy + 7, w: SWITCH_W, h: 22 };
                            canvas.rect(r.x, r.y, r.w, r.h, if on { ACCENT } else { FIELD_BG });
                            canvas.rect(if on { r.x + r.w - 20 } else { r.x + 2 }, r.y + 2, 18, 18, FG);
                            hits.push((r, DialogHit::Flip(n, s)));
                        }
                        SettingType::Enum(_) => {
                            let shown = value.display();
                            let tw = text.map_or(0.0, |t| t.width(&shown, 13.0)) as i32;
                            let r = Rect { x: right - tw - 24, y: sy + 5, w: tw + 24, h: 26 };
                            canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                            label(text, &mut canvas, &shown, (r.x + 12) as f32, baseline(text, r.y, r.h, 13.0), 13.0, FG, tw as f32 + 1.0);
                            hits.push((r, DialogHit::Next(n, s)));
                        }
                        SettingType::Int | SettingType::Double => {
                            let plus = Rect { x: right - SMALL, y: sy + 4, w: SMALL, h: SMALL };
                            let minus = Rect { x: right - 3 * SMALL - 60, y: sy + 4, w: SMALL, h: SMALL };
                            for (r, glyph) in [(minus, "-"), (plus, "+")] {
                                canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                                let gw = text.map_or(0.0, |t| t.width(glyph, 15.0));
                                label(text, &mut canvas, glyph, r.x as f32 + (r.w as f32 - gw) / 2.0, baseline(text, r.y, r.h, 15.0), 15.0, FG, r.w as f32);
                            }
                            let shown = value.display();
                            let tw = text.map_or(0.0, |t| t.width(&shown, 13.0));
                            let mid = (minus.x + minus.w + plus.x) as f32 / 2.0;
                            label(text, &mut canvas, &shown, mid - tw / 2.0, baseline(text, sy, SETTING_H, 13.0), 13.0, FG, 80.0);
                            hits.push((minus, DialogHit::Down(n, s)));
                            hits.push((plus, DialogHit::Up(n, s)));
                        }
                        SettingType::String | SettingType::StringList => {
                            let shown = value.display();
                            let shown = if shown.is_empty() { "(empty)".to_owned() } else { shown };
                            let tw = text.map_or(0.0, |t| t.width(&shown, 13.0)).min(240.0);
                            label(text, &mut canvas, &shown, right as f32 - tw, baseline(text, sy, SETTING_H, 13.0), 13.0, DIM_FG, 240.0);
                        }
                    }
                    sy += SETTING_H;
                }
            }
            y += entry_h;
        }
        if scroll > 0 || more_below {
            let hint = "Scroll for more";
            label(text, &mut canvas, hint, (bx + PAD) as f32, baseline(text, bottom, 36, 12.0), 12.0, DIM_FG, 200.0);
        }
        let done = Rect { x: bx + bw - PAD - 110, y: bottom, w: 110, h: 36 };
        canvas.rect(done.x, done.y, done.w, done.h, ACCENT);
        let tw = text.map_or(0.0, |t| t.width("Done", 15.0));
        label(text, &mut canvas, "Done", done.x as f32 + (done.w as f32 - tw) / 2.0, baseline(text, done.y, done.h, 15.0), 15.0, FG, done.w as f32);
        hits.push((done, DialogHit::Done));
        if let Some(r) = overlay.state.focus.and_then(|i| hits.get(i)).map(|(r, _)| *r) {
            for (rx, ry, rw, rh) in [(r.x - 2, r.y - 2, r.w + 4, 2), (r.x - 2, r.y + r.h, r.w + 4, 2), (r.x - 2, r.y - 2, 2, r.h + 4), (r.x + r.w, r.y - 2, 2, r.h + 4)] {
                canvas.rect(rx, ry, rw, rh, ACCENT);
            }
        }
        overlay.hits = hits.into_iter().map(|(r, hit)| (r, OverlayHit::ExtDialog(hit))).collect();
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
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
            DialogHit::ChangeKey(n, c) | DialogHit::ClearKey(n, c) => {
                let Some(i) = info(n) else { return };
                let Some((cmd, _, _)) = i.commands.get(c) else { return };
                let full = format!("{}/{cmd}", i.id);
                if let Some(d) = self.extensions_dialog.as_mut() {
                    d.state.refused = None;
                    d.state.capturing = matches!(hit, DialogHit::ChangeKey(..)).then(|| full.clone());
                }
                if matches!(hit, DialogHit::ClearKey(..)) {
                    let _ = self.requests.send(UiRequest::ExtBind(full, String::new()));
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

    pub(super) fn extensions_dialog_click(&mut self, x: f64, y: f64) {
        let hit = self.extensions_dialog.as_ref().and_then(|o| o.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h));
        if let Some(OverlayHit::ExtDialog(hit)) = hit {
            if let Some(d) = self.extensions_dialog.as_mut() {
                d.state.focus = None;
            }
            self.extensions_dialog_do(hit);
        }
    }

    /// A command's new key was refused: say why on its row.
    pub(super) fn extensions_dialog_refused(&mut self, command: String, why: String) {
        if let Some(d) = self.extensions_dialog.as_mut() {
            d.state.refused = Some((command, why));
        }
        self.draw_extensions_dialog();
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
        // Waiting for a command's new key.
        if let Some(command) = dialog.state.capturing.clone() {
            match event.keysym {
                Keysym::Escape => dialog.state.capturing = None,
                Keysym::BackSpace => {
                    dialog.state.capturing = None;
                    let _ = self.requests.send(UiRequest::ExtBind(command, String::new()));
                }
                keysym => {
                    if let Some(accelerator) = accelerator_for(keysym, &self.mods) {
                        dialog.state.capturing = None;
                        let _ = self.requests.send(UiRequest::ExtBind(command, accelerator));
                    }
                }
            }
            self.draw_extensions_dialog();
            return;
        }
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
