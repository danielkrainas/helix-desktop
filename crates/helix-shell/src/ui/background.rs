//! The desktop background on every monitor, and the lock screen's
//! (helix-desktop-spec `contracts/gsettings/background.md`).

use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;

use super::{physical, Ui};
use crate::wallpaper::{place, Background, Image, Shading, Span};

/// One monitor's background surface.
pub(super) struct BackgroundView {
    pub(super) output: WlOutput,
    pub(super) layer: LayerSurface,
    pub(super) width: u32,
    pub(super) height: u32,
}

fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> [f32; 3] {
    let f = |i: usize| f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t;
    [f(0), f(1), f(2)]
}

/// A source pixel, bilinear, at (u, v) in image pixels: RGBA as floats.
fn sample(image: &Image, u: f32, v: f32) -> [f32; 4] {
    let (w, h) = (image.width as i32, image.height as i32);
    let (x, y) = ((u - 0.5).max(0.0), (v - 0.5).max(0.0));
    let (x0, y0) = ((x as i32).min(w - 1), (y as i32).min(h - 1));
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let px = |xx: i32, yy: i32| {
        let i = (yy as usize * image.width as usize + xx as usize) * 4;
        [f32::from(image.rgba[i]), f32::from(image.rgba[i + 1]), f32::from(image.rgba[i + 2]), f32::from(image.rgba[i + 3])]
    };
    let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
    let mut out = [0.0; 4];
    for k in 0..4 {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        out[k] = top + (bottom - top) * fy;
    }
    out
}

/// Paint `background` into an ARGB8888 buffer of `pw`×`ph` pixels showing a
/// monitor of logical size `monitor` at `scale`; `span`: the layout's box and
/// this monitor's offset in it.
pub fn paint(buf: &mut [u8], pw: usize, ph: usize, scale: f64, background: &Background, monitor: (f64, f64), span: Option<Span>) {
    // The colors.
    for y in 0..ph {
        for x in 0..pw {
            let t = match background.shading {
                Shading::Solid => 0.0,
                Shading::Vertical => y as f32 / ph.max(2).saturating_sub(1) as f32,
                Shading::Horizontal => x as f32 / pw.max(2).saturating_sub(1) as f32,
            };
            let c = lerp(background.primary, background.secondary, t);
            let i = (y * pw + x) * 4;
            buf[i..i + 4].copy_from_slice(&[c[2] as u8, c[1] as u8, c[0] as u8, 255]);
        }
    }
    let Some(image) = background.image.as_deref() else { return };
    let Some(((rx, ry, rw, rh), tiled)) = place(background.fit, (f64::from(image.width), f64::from(image.height)), monitor, span) else { return };
    // In pixels.
    let (rx, ry, rw, rh) = (rx * scale, ry * scale, rw * scale, rh * scale);
    if rw < 1.0 || rh < 1.0 {
        return;
    }
    let (sx, sy) = (f64::from(image.width) / rw, f64::from(image.height) / rh);
    let (x0, y0, x1, y1) = if tiled { (0, 0, pw, ph) } else { (rx.max(0.0) as usize, ry.max(0.0) as usize, ((rx + rw).ceil().max(0.0) as usize).min(pw), ((ry + rh).ceil().max(0.0) as usize).min(ph)) };
    for y in y0..y1 {
        let mut ly = y as f64 + 0.5 - ry;
        if tiled {
            ly = ly.rem_euclid(rh);
        }
        if !(0.0..rh).contains(&ly) {
            continue;
        }
        for x in x0..x1 {
            let mut lx = x as f64 + 0.5 - rx;
            if tiled {
                lx = lx.rem_euclid(rw);
            }
            if !(0.0..rw).contains(&lx) {
                continue;
            }
            let s = sample(image, (lx * sx) as f32, (ly * sy) as f32);
            let a = s[3] / 255.0 * background.opacity;
            let i = (y * pw + x) * 4;
            for (k, ch) in [(0usize, 2usize), (1, 1), (2, 0)] {
                let under = f32::from(buf[i + k]);
                buf[i + k] = (s[ch] * a + under * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

impl Ui {
    /// The layout's bounding box and `output`'s offset in it (for spanned).
    fn span_of(&self, output: &WlOutput) -> Option<Span> {
        let rects: Vec<(i32, i32, i32, i32)> = self
            .output_state
            .outputs()
            .filter_map(|o| {
                let i = self.output_state.info(&o)?;
                let ((x, y), (w, h)) = (i.logical_position?, i.logical_size?);
                Some((x, y, w, h))
            })
            .collect();
        let info = self.output_state.info(output)?;
        let (ox, oy) = info.logical_position?;
        let (x0, y0) = rects.iter().fold((i32::MAX, i32::MAX), |(a, b), r| (a.min(r.0), b.min(r.1)));
        let (x1, y1) = rects.iter().fold((i32::MIN, i32::MIN), |(a, b), r| (a.max(r.0 + r.2), b.max(r.1 + r.3)));
        Some(((f64::from(x0), f64::from(y0), f64::from(x1 - x0), f64::from(y1 - y0)), (f64::from(ox - x0), f64::from(oy - y0))))
    }

    pub(super) fn add_background(&mut self, output: WlOutput) {
        if self.backgrounds.iter().any(|b| b.output == output) {
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Background, Some("helix-background"), Some(&output));
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.backgrounds.push(BackgroundView { output, layer, width: 0, height: 0 });
    }

    pub(super) fn remove_background(&mut self, output: &WlOutput) {
        self.backgrounds.retain(|b| &b.output != output);
    }

    /// A background surface got its size; true if it was one.
    pub(super) fn background_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(index) = self.backgrounds.iter().position(|b| &b.layer == layer) else { return false };
        let view = &mut self.backgrounds[index];
        if (view.width, view.height) != size || size.0 == 0 {
            view.width = size.0;
            view.height = size.1;
        }
        self.draw_background(index);
        true
    }

    pub(super) fn draw_backgrounds(&mut self) {
        for index in 0..self.backgrounds.len() {
            self.draw_background(index);
        }
    }

    fn draw_background(&mut self, index: usize) {
        let view = &self.backgrounds[index];
        let (w, h) = (view.width as usize, view.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        let scale = self.scales.of(view.layer.wl_surface());
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        let span = self.span_of(&view.output);
        let pool = match self.background_pool.as_mut() {
            Some(pool) => pool,
            None => match SlotPool::new(pw * ph * 4, &self.shm) {
                Ok(pool) => self.background_pool.insert(pool),
                Err(err) => {
                    tracing::warn!(%err, "no memory for the background");
                    return;
                }
            },
        };
        let Ok((buffer, data)) = pool.create_buffer(pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Xrgb8888) else { return };
        paint(data, pw, ph, scale, &self.desktop_background, (w as f64, h as f64), span);
        let view = &self.backgrounds[index];
        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, pw as i32, ph as i32);
            view.layer.commit();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::wallpaper::Fit;

    #[test]
    fn paints_colors_and_picture() {
        // A 2x1 picture, red and blue, stretched over 4x2 pixels at 50 %.
        let image = Image { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let bg = Background { fit: Fit::Stretched, opacity: 0.5, primary: [0, 0, 0], secondary: [0, 0, 0], shading: Shading::Solid, image: Some(Arc::new(image)) };
        let mut buf = vec![0u8; 4 * 2 * 4];
        paint(&mut buf, 4, 2, 1.0, &bg, (4.0, 2.0), None);
        // Leftmost pixel: half red over black (BGRA bytes).
        assert_eq!(&buf[0..4], &[0, 0, 128, 255]);
        // Rightmost: half blue.
        assert_eq!(&buf[12..16], &[128, 0, 0, 255]);
        // Vertical gradient without a picture.
        let bg = Background { fit: Fit::None, opacity: 1.0, primary: [0, 0, 0], secondary: [200, 100, 0], shading: Shading::Vertical, image: None };
        paint(&mut buf, 4, 2, 1.0, &bg, (4.0, 2.0), None);
        assert_eq!(&buf[0..4], &[0, 0, 0, 255]);
        assert_eq!(&buf[16..20], &[0, 100, 200, 255]);
    }
}
