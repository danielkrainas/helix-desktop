//! "Customize Panel…" (helix-desktop-spec `contracts/wayland/README.md`,
//! "Panel layout", "Editing"): a dialog to arrange the panel's items and
//! the quick settings switches. Each change is written to the keys at once.

use super::*;
use crate::panel_layout::{self as layout, Edit, COLUMNS, QUICK};

const COL_W_MAX: i32 = 250;
const COL_W_MIN: i32 = 190;
const PAD: i32 = 16;
const ROW_H: i32 = 34;
const BUTTON: i32 = 24;
const BOTTOM_W: i32 = 110;
const BOTTOM_H: i32 = 36;

/// A button in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditorHit {
    Up(usize, usize),
    Down(usize, usize),
    Left(usize, usize),
    Right(usize, usize),
    Remove(usize, usize),
    /// Hidden entry `n` back.
    Add(usize),
    Reset,
    Done,
}

#[derive(Debug, Default)]
pub(super) struct EditorState {
    /// Keyboard focus: index into the hits.
    focus: Option<usize>,
}

#[derive(Clone, Copy)]
enum Glyph {
    Up,
    Down,
    Left,
    Right,
    Cross,
    Plus,
}

fn draw_glyph(canvas: &mut Canvas, r: Rect, glyph: Glyph) {
    let (cx, cy) = (f64::from(r.x + r.w / 2), f64::from(r.y + r.h / 2));
    let lines: Vec<((f64, f64), (f64, f64))> = match glyph {
        Glyph::Up => vec![((cx - 5.0, cy + 2.5), (cx, cy - 2.5)), ((cx, cy - 2.5), (cx + 5.0, cy + 2.5))],
        Glyph::Down => vec![((cx - 5.0, cy - 2.5), (cx, cy + 2.5)), ((cx, cy + 2.5), (cx + 5.0, cy - 2.5))],
        Glyph::Left => vec![((cx + 2.5, cy - 5.0), (cx - 2.5, cy)), ((cx - 2.5, cy), (cx + 2.5, cy + 5.0))],
        Glyph::Right => vec![((cx - 2.5, cy - 5.0), (cx + 2.5, cy)), ((cx + 2.5, cy), (cx - 2.5, cy + 5.0))],
        Glyph::Cross => vec![((cx - 4.5, cy - 4.5), (cx + 4.5, cy + 4.5)), ((cx - 4.5, cy + 4.5), (cx + 4.5, cy - 4.5))],
        Glyph::Plus => vec![((cx - 5.0, cy), (cx + 5.0, cy)), ((cx, cy - 5.0), (cx, cy + 5.0))],
    };
    for (a, b) in lines {
        canvas.line(a, b, 1.7, FG);
    }
}

impl Ui {
    pub(super) fn open_panel_editor(&mut self) {
        if self.panel_editor.is_some() {
            return;
        }
        self.close_launcher(true);
        self.power_menu = None;
        self.leave_panel();
        self.panel_editor = Some(self.new_overlay("helix-panel-editor", KeyboardInteractivity::Exclusive, EditorState::default()));
    }

    pub(super) fn draw_panel_editor(&mut self) {
        let Some(mut overlay) = self.panel_editor.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_panel_editor(&mut overlay);
        }
        self.panel_editor = Some(overlay);
    }

    fn paint_panel_editor(&mut self, overlay: &mut Overlay<EditorState>) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x9900_0000);
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        let keys = self.panel_keys.normalized();
        let columns: Vec<Vec<layout::Row>> = (0..COLUMNS.len()).map(|c| layout::rows(&keys, c)).collect();
        let hidden = layout::hidden(&keys);
        let hidden_in = |col: usize| hidden.iter().enumerate().filter(move |(_, (c, _, _))| *c == col).collect::<Vec<_>>();
        let saved = helix_settings::has_schema(layout::SCHEMA);

        let col_w = ((w as i32 - 5 * PAD - 48) / 4).clamp(COL_W_MIN, COL_W_MAX);
        let bw = 4 * col_w + 5 * PAD;
        let rows_max = columns.iter().map(Vec::len).max().unwrap_or(0) as i32;
        let hidden_max = hidden_in(2).len().max(hidden_in(QUICK).len()) as i32;
        let hidden_h = if hidden_max > 0 { 32 + hidden_max * ROW_H } else { 0 };
        let note_h = if saved { 0 } else { 28 };
        let bh = 56 + note_h + 30 + rows_max * ROW_H + 16 + hidden_h + 16 + BOTTOM_H + PAD;
        // The top stays put while rows come and go (it grows downward), so
        // buttons do not move under the pointer.
        let typical = 56 + 30 + layout::Item::ALL.len() as i32 * ROW_H + 16 + BOTTOM_H + PAD;
        let (bx, by) = ((w as i32 - bw) / 2, ((h as i32 - typical) / 2).max(PANEL_HEIGHT as i32 + 8).min((h as i32 - bh).max(PANEL_HEIGHT as i32 + 8)));
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        for (rx, ry, rw, rh) in [(bx, by, bw, 1), (bx, by + bh - 1, bw, 1), (bx, by, 1, bh), (bx + bw - 1, by, 1, bh)] {
            canvas.rect(rx, ry, rw, rh, BANNER_BORDER);
        }
        label(bold.or(text), &mut canvas, "Customize Panel", (bx + PAD) as f32, (by + 36) as f32, 20.0, FG, (bw - 2 * PAD) as f32);
        let mut y = by + 56;
        if !saved {
            label(text, &mut canvas, "Changes cannot be saved: the org.helix.shell settings are not installed.", (bx + PAD) as f32, (y + 16) as f32, 13.0, DIM_FG, (bw - 2 * PAD) as f32);
            y += note_h;
        }
        let mut hits: Vec<(Rect, EditorHit)> = Vec::new();
        let col_x = |c: usize| bx + PAD + c as i32 * (col_w + PAD);
        for (c, title) in COLUMNS.iter().enumerate() {
            label(bold.or(text), &mut canvas, title, col_x(c) as f32, (y + 18) as f32, 14.0, DIM_FG, col_w as f32);
        }
        let rows_y = y + 30;
        for (c, rows) in columns.iter().enumerate() {
            for (r, row) in rows.iter().enumerate() {
                let ry = rows_y + r as i32 * ROW_H;
                canvas.rect(col_x(c), ry + 2, col_w, ROW_H - 4, HOVER_BG);
                // Buttons from the right end: remove, right, left, down, up.
                let mut buttons: Vec<(EditorHit, Glyph)> = vec![(EditorHit::Up(c, r), Glyph::Up), (EditorHit::Down(c, r), Glyph::Down)];
                if c < QUICK {
                    if c > 0 {
                        buttons.push((EditorHit::Left(c, r), Glyph::Left));
                    }
                    if c < QUICK - 1 {
                        buttons.push((EditorHit::Right(c, r), Glyph::Right));
                    }
                }
                if row.id != layout::Item::Power.id() {
                    buttons.push((EditorHit::Remove(c, r), Glyph::Cross));
                }
                // Right-aligned, but in reading order for Tab.
                let mut x = col_x(c) + col_w - 4 - buttons.len() as i32 * BUTTON;
                let label_end = x;
                for (hit, glyph) in buttons {
                    let br = Rect { x, y: ry + (ROW_H - BUTTON) / 2, w: BUTTON, h: BUTTON };
                    draw_glyph(&mut canvas, br, glyph);
                    hits.push((br, hit));
                    x += BUTTON;
                }
                let x = label_end;
                label(text, &mut canvas, row.name, (col_x(c) + 10) as f32, baseline(text, ry, ROW_H, 14.0), 14.0, FG, (x - col_x(c) - 14) as f32);
            }
        }
        y = rows_y + rows_max * ROW_H + 16;
        if hidden_h > 0 {
            for c in [2, QUICK] {
                if hidden_in(c).is_empty() {
                    continue;
                }
                label(bold.or(text), &mut canvas, "Hidden", col_x(c) as f32, (y + 18) as f32, 14.0, DIM_FG, col_w as f32);
                for (r, (n, (_, _, name))) in hidden_in(c).into_iter().enumerate() {
                    let ry = y + 32 + r as i32 * ROW_H;
                    let br = Rect { x: col_x(c) + col_w - 4 - BUTTON, y: ry + (ROW_H - BUTTON) / 2, w: BUTTON, h: BUTTON };
                    draw_glyph(&mut canvas, br, Glyph::Plus);
                    hits.push((br, EditorHit::Add(n)));
                    label(text, &mut canvas, name, (col_x(c) + 10) as f32, baseline(text, ry, ROW_H, 14.0), 14.0, DIM_FG, (col_w - BUTTON - 20) as f32);
                }
            }
            y += hidden_h;
        }
        y += 16;
        for (hit, name, x) in [(EditorHit::Reset, "Reset", bx + PAD), (EditorHit::Done, "Done", bx + bw - PAD - BOTTOM_W)] {
            let r = Rect { x, y, w: BOTTOM_W, h: BOTTOM_H };
            canvas.rect(r.x, r.y, r.w, r.h, if hit == EditorHit::Done { ACCENT } else { BANNER_BORDER });
            let tw = text.map_or(0.0, |t| t.width(name, 15.0));
            label(text, &mut canvas, name, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 15.0), 15.0, FG, r.w as f32);
            hits.push((r, hit));
        }
        // Keyboard focus ring.
        if let Some(r) = overlay.state.focus.and_then(|i| hits.get(i)).map(|(r, _)| *r) {
            for (rx, ry, rw, rh) in [(r.x - 2, r.y - 2, r.w + 4, 2), (r.x - 2, r.y + r.h, r.w + 4, 2), (r.x - 2, r.y - 2, 2, r.h + 4), (r.x + r.w, r.y - 2, 2, r.h + 4)] {
                canvas.rect(rx, ry, rw, rh, ACCENT);
            }
        }
        overlay.hits = hits.into_iter().map(|(r, hit)| (r, OverlayHit::PanelEditor(hit))).collect();
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    fn panel_editor_do(&mut self, hit: EditorHit) {
        crate::click_feedback();
        let keys = &self.panel_keys;
        let change = match hit {
            EditorHit::Up(c, r) => Edit::Up(c, r),
            EditorHit::Down(c, r) => Edit::Down(c, r),
            EditorHit::Left(c, r) => Edit::Move(c, r, c - 1),
            EditorHit::Right(c, r) => Edit::Move(c, r, c + 1),
            EditorHit::Remove(c, r) => Edit::Remove(c, r),
            EditorHit::Add(n) => match layout::hidden(keys).get(n) {
                Some((_, id, _)) => Edit::Add(id.clone()),
                None => return,
            },
            EditorHit::Reset => Edit::Reset,
            EditorHit::Done => {
                self.panel_editor = None;
                return;
            }
        };
        let changed = layout::edit(keys, change);
        if changed != *keys {
            tracing::info!(?changed, "panel layout edited");
            layout::write(&changed);
            // Shown at once; the settings' echo changes nothing more.
            self.panel_layout_changed(changed);
        }
    }

    /// A click on the editor: outside its buttons does nothing.
    pub(super) fn panel_editor_click(&mut self, x: f64, y: f64) {
        let hit = self.panel_editor.as_ref().and_then(|o| o.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h));
        if let Some(OverlayHit::PanelEditor(hit)) = hit {
            if let Some(editor) = self.panel_editor.as_mut() {
                editor.state.focus = None;
            }
            self.panel_editor_do(hit);
        }
    }

    /// Tab and Shift+Tab move between the buttons, Enter or Space presses
    /// one, Escape closes.
    pub(super) fn panel_editor_key(&mut self, event: &KeyEvent) {
        let Some(editor) = self.panel_editor.as_mut() else { return };
        let count = editor.hits.len().max(1);
        match event.keysym {
            Keysym::Escape => self.panel_editor = None,
            Keysym::Tab | Keysym::Right | Keysym::Down => {
                editor.state.focus = Some(editor.state.focus.map_or(0, |i| (i + 1) % count));
                self.draw_panel_editor();
            }
            Keysym::ISO_Left_Tab | Keysym::Left | Keysym::Up => {
                editor.state.focus = Some(editor.state.focus.map_or(count - 1, |i| (i + count - 1) % count));
                self.draw_panel_editor();
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                let hit = editor.state.focus.and_then(|i| editor.hits.get(i)).map(|(_, h)| *h);
                if let Some(OverlayHit::PanelEditor(hit)) = hit {
                    self.panel_editor_do(hit);
                    // Keep the focus on a button that still exists.
                    if let Some(editor) = self.panel_editor.as_mut() {
                        let count = editor.hits.len().max(1);
                        editor.state.focus = editor.state.focus.map(|i| i.min(count - 1));
                    }
                    self.draw_panel_editor();
                }
            }
            _ => {}
        }
    }

    /// A right click on the panel outside the status icons: the panel's
    /// own menu. True if it was one.
    pub(super) fn panel_context(&mut self, event: &PointerEvent) -> bool {
        if &event.surface != self.panel.wl_surface() || self.lock.session.is_some() {
            return false;
        }
        let PointerEventKind::Press { button: 0x111, .. } = event.kind else { return false };
        let item = super::window_menu::Item {
            label: "Customize Panel…".into(),
            choice: super::window_menu::Choice::CustomizePanel,
            checked: None,
            active: false,
            sub: Vec::new(),
        };
        self.open_menu(0, event.position.0 as i32, PANEL_HEIGHT as i32, vec![Some(item)], Vec::new());
        true
    }
}
