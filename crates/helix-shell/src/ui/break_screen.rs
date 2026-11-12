//! The break screen (helix-desktop-spec `contracts/gsettings/wellbeing.md`,
//! "Break reminders"): a darkened layer over everything with the break's
//! title, the time left, and "Delay" / "Skip".

use super::*;
use crate::wellbeing::{Answer, BreakView};

const BUTTON_W: i32 = 120;
const BUTTON_H: i32 = 40;

impl Ui {
    pub(super) fn show_break(&mut self, view: Option<BreakView>) {
        match view {
            None => self.break_screen = None,
            Some(view) => match self.break_screen.as_mut() {
                Some(overlay) => {
                    overlay.state = view;
                    self.draw_break();
                }
                None => {
                    self.close_launcher(true);
                    self.power_menu = None;
                    self.break_screen = Some(self.new_overlay("helix-break", KeyboardInteractivity::OnDemand, view));
                }
            },
        }
    }

    pub(super) fn draw_break(&mut self) {
        let Some(mut overlay) = self.break_screen.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_break(&mut overlay);
        }
        self.break_screen = Some(overlay);
    }

    fn paint_break(&mut self, overlay: &mut Overlay<BreakView>) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        // Premultiplied black at 80 %.
        canvas.fill(0xCC00_0000);
        let (text, bold) = (self.text.as_ref(), self.bold.as_ref());
        let view = &overlay.state;
        let center = |t: Option<&Text>, s: &str, px: f32| (w as f32 - t.map_or(0.0, |t| t.width(s, px))) / 2.0;
        let mut y = h as i32 / 2 - 90;
        label(bold.or(text), &mut canvas, &view.title, center(bold.or(text), &view.title, 30.0), y as f32, 30.0, FG, w as f32);
        y += 44;
        label(text, &mut canvas, &view.message, center(text, &view.message, 17.0), y as f32, 17.0, DIM_FG, w as f32);
        if let Some(left) = view.left {
            y += 64;
            let clock = format!("{}:{:02}", left / 60, left % 60);
            label(bold.or(text), &mut canvas, &clock, center(bold.or(text), &clock, 40.0), y as f32, 40.0, FG, w as f32);
        }
        y += 48;
        overlay.hits.clear();
        let bx = w as i32 / 2 - BUTTON_W - 8;
        for (i, (name, hit)) in [("Delay", OverlayHit::Cancel), ("Skip", OverlayHit::Confirm)].into_iter().enumerate() {
            let r = Rect { x: bx + i as i32 * (BUTTON_W + 16), y, w: BUTTON_W, h: BUTTON_H };
            canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
            let tw = text.map_or(0.0, |t| t.width(name, 15.0));
            label(text, &mut canvas, name, r.x as f32 + (r.w as f32 - tw) / 2.0, baseline(text, r.y, r.h, 15.0), 15.0, FG, r.w as f32);
            overlay.hits.push((r, hit));
        }
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    fn answer_break(&mut self, answer: Answer) {
        self.break_screen = None;
        let _ = self.requests.send(UiRequest::BreakAnswer(answer));
    }

    /// A click on the break screen.
    pub(super) fn break_click(&mut self, x: f64, y: f64) {
        let hit = self.break_screen.as_ref().and_then(|o| o.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h));
        match hit {
            Some(OverlayHit::Cancel) => self.answer_break(Answer::Delay),
            Some(OverlayHit::Confirm) => self.answer_break(Answer::Skip),
            _ => {}
        }
    }

    /// Escape delays.
    pub(super) fn break_key(&mut self, event: &KeyEvent) {
        if event.keysym == Keysym::Escape {
            self.answer_break(Answer::Delay);
        }
    }
}
