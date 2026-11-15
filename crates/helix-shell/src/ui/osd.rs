//! On-screen displays (helix-desktop-spec `contracts/dbus/org.gnome.Shell.md`,
//! `ShowOSD`, and `contracts/wayland/README.md`, "On-screen displays"): a
//! floating pill near the bottom of each monitor (or the one `ShowOSD`
//! names), above full-screen windows, with an icon, a level bar and/or a
//! label.

use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_surface::WlSurface};
use helix_draw::Image;

use super::*;

/// The pill's size: a level, a label, or both (two rows).
const OSD_W: u32 = 300;
const OSD_H: i32 = 52;
const OSD_H_TWO: i32 = 72;
/// How far above the monitor's bottom edge.
const OSD_MARGIN: i32 = 72;
const OSD_ICON: i32 = 24;

/// One monitor's OSD surface.
pub(super) struct OsdView {
    layer: LayerSurface,
    output: Option<wl_output::WlOutput>,
    width: u32,
    height: u32,
    configured: bool,
}

/// The pill's height for `osd` (grown with the text scale).
fn osd_height(osd: &Osd) -> i32 {
    let two = osd.level.is_some() && osd.label.is_some();
    helix_draw::theme::grow(if two { OSD_H_TWO } else { OSD_H })
}

/// The symbolic icon names `ShowOSD`'s `icon` stands for: an icon name,
/// or a serialized themed GIcon (". GThemedIcon name1 name2 …").
pub(super) fn osd_icon_names(icon: &str) -> Vec<String> {
    let icon = icon.trim();
    match icon.strip_prefix(". GThemedIcon ") {
        Some(names) => names.split_whitespace().map(str::to_owned).collect(),
        None if !icon.is_empty() && !icon.contains('/') => vec![icon.to_owned()],
        None => Vec::new(),
    }
}

/// Paint the OSD pill for `osd` on a `w`×`h` canvas, with `icon`.
pub(super) fn paint_osd(canvas: &mut Canvas, text: Option<&Text>, icon: Option<&Image>, osd: &Osd, (w, h): (i32, i32)) {
    canvas.fill(0);
    let radius = f64::from(h.min(OSD_H)) / 2.0;
    canvas.round_rect(0.0, 0.0, f64::from(w), f64::from(h), radius, 0xF0262630);
    if helix_draw::high_contrast() {
        canvas.round_outline(1.0, 1.0, f64::from(w - 2), f64::from(h - 2), radius - 1.0, 2.0, 0xFFFFFFFF);
    } else {
        canvas.round_outline(0.5, 0.5, f64::from(w) - 1.0, f64::from(h) - 1.0, radius - 0.5, 1.0, 0x40FFFFFF);
    }
    let pad = 18;
    let mut x = pad;
    if let Some(image) = icon {
        canvas.image(f64::from(x), f64::from((h - OSD_ICON) / 2), f64::from(OSD_ICON), f64::from(OSD_ICON), image);
        x += OSD_ICON + 14;
    }
    let px = 14.0;
    let Some(level) = osd.level else {
        let caption = osd_caption(osd);
        // Words only (e.g. "Microphone off"), centered beside the icon.
        if let Some(caption) = caption {
            let room = (w - x - pad) as f32;
            let cw = text.map_or(0.0, |t| t.width(&caption, px)).min(room);
            label(text, canvas, &caption, x as f32 + (room - cw) / 2.0, baseline(text, 0, h, px), px, FG, room);
        }
        return;
    };
    // With a label: the words above the bar (the icon says what the level
    // is of, so there are no words without one).
    let bar_y = if let Some(caption) = &osd.label {
        let row = h / 2;
        label(text, canvas, caption, x as f32, baseline(text, 6, row - 4, px), px, FG, (w - x - pad) as f32);
        row + (h - row) / 2 - 3
    } else {
        h / 2 - 3
    };
    let max = osd.max_level.max(f64::EPSILON);
    let fraction = (level / max).clamp(0.0, 1.0);
    let percent = helix_i18n::format_percent((level * 100.0).round());
    let pw = text.map_or(36.0, |t| t.width(&helix_i18n::format_percent(100.0), px)) as i32 + 8;
    let bar_w = (w - x - pad - pw - 10).max(20);
    canvas.round_rect(f64::from(x), f64::from(bar_y), f64::from(bar_w), 6.0, 3.0, if helix_draw::high_contrast() { 0xFF000000 } else { theme::TRACK });
    if helix_draw::high_contrast() {
        canvas.round_outline(f64::from(x), f64::from(bar_y), f64::from(bar_w), 6.0, 3.0, 1.0, 0xFFFFFFFF);
    }
    let filled = (fraction * f64::from(bar_w)).max(6.0);
    canvas.round_rect(f64::from(x), f64::from(bar_y), filled, 6.0, 3.0, accent());
    // Above 100 % (overamplification): a mark where 100 % is.
    if max > 1.0 {
        let mark = f64::from(x) + f64::from(bar_w) / max;
        canvas.rect(mark.round() as i32 - 1, bar_y - 4, 2, 14, FG);
    }
    label(text, canvas, &percent, (x + bar_w + 10) as f32, baseline(text, bar_y - 10, 26, px), px, FG, pw as f32);
}

impl Ui {
    /// Show `osd` (for [`OSD_DURATION`]) on the monitor its connector
    /// names, else on every monitor.
    pub(super) fn show_osd(&mut self, osd: Osd) {
        let outputs: Vec<wl_output::WlOutput> = {
            let all: Vec<wl_output::WlOutput> = self.output_state.outputs().collect();
            let named: Vec<wl_output::WlOutput> = osd
                .connector
                .as_ref()
                .map(|c| all.iter().filter(|o| self.output_state.info(o).and_then(|i| i.name).as_deref() == Some(c.as_str())).cloned().collect())
                .unwrap_or_default();
            if named.is_empty() { all } else { named }
        };
        let height = osd_height(&osd);
        self.osd = Some((osd, Instant::now() + OSD_DURATION));
        let same = self.osd_views.len() == outputs.len().max(1) && self.osd_views.iter().all(|v| v.output.as_ref().is_none_or(|o| outputs.contains(o)));
        if same {
            for view in &mut self.osd_views {
                if view.height != height as u32 {
                    view.layer.set_size(osd_w(), height as u32);
                    view.layer.commit();
                }
            }
            self.draw_osd();
            return;
        }
        self.osd_views.clear();
        let targets: Vec<Option<wl_output::WlOutput>> = if outputs.is_empty() { vec![None] } else { outputs.into_iter().map(Some).collect() };
        for output in targets {
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-osd"), output.as_ref());
            layer.set_anchor(Anchor::BOTTOM);
            layer.set_margin(0, 0, OSD_MARGIN, 0);
            layer.set_size(osd_w(), height as u32);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            // The pointer goes through it.
            if let Ok(region) = Region::new(&self.compositor) {
                layer.wl_surface().set_input_region(Some(region.wl_region()));
            }
            layer.commit();
            self.osd_views.push(OsdView { layer, output, width: 0, height: 0, configured: false });
        }
    }

    /// The OSD's time is up: it goes.
    pub(super) fn osd_tick(&mut self) {
        if self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            self.osd = None;
            self.osd_views.clear();
        }
    }

    pub(super) fn osd_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(view) = self.osd_views.iter_mut().find(|v| &v.layer == layer) else { return false };
        view.width = if size.0 > 0 { size.0 } else { osd_w() };
        view.height = size.1;
        view.configured = true;
        self.draw_osd();
        true
    }

    pub(super) fn osd_closed(&mut self, layer: &LayerSurface) -> bool {
        let before = self.osd_views.len();
        self.osd_views.retain(|v| &v.layer != layer);
        before != self.osd_views.len()
    }

    pub(super) fn draw_osd(&mut self) {
        let Some((osd, _)) = self.osd.clone() else { return };
        for i in 0..self.osd_views.len() {
            let view = &self.osd_views[i];
            if !view.configured || view.width == 0 || view.height == 0 {
                continue;
            }
            let (w, h) = (view.width as usize, view.height as usize);
            let layer = view.layer.clone();
            let scale = self.scales.of(layer.wl_surface());
            let icon = osd.icon.as_deref().map(osd_icon_names).and_then(|names| names.iter().find_map(|n| self.symbolic_icon_px(n, f64::from(OSD_ICON), scale)));
            let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
                continue;
            };
            let mut canvas = Canvas::scaled(data, w, h, scale);
            paint_osd(&mut canvas, self.text.as_ref(), icon.as_ref(), &osd, (w as i32, h as i32));
            self.scales.prepare(&self.qh, layer.wl_surface(), w as i32, h as i32);
            if buffer.attach_to(layer.wl_surface()).is_ok() {
                layer.commit();
            }
        }
    }

    /// The OSD on the accessibility bus: its surface, if shown.
    pub(super) fn osd_a11y(&self) -> Option<(&WlSurface, (u32, u32))> {
        self.osd.as_ref()?;
        let view = self.osd_views.iter().find(|v| v.configured)?;
        Some((view.layer.wl_surface(), (view.width, view.height)))
    }

    /// What the OSD says, for screen readers ("Volume 45 %").
    pub(super) fn osd_words(&self) -> Option<String> {
        let (osd, _) = self.osd.as_ref()?;
        let caption = osd_caption(osd);
        let level = osd.level.map(|l| helix_i18n::format_percent((l * 100.0).round()));
        match (caption, level) {
            (Some(c), Some(l)) => Some(format!("{c} {l}")),
            (Some(c), None) => Some(c),
            (None, Some(l)) => Some(l),
            (None, None) => None,
        }
    }
}

/// The pill's width, grown with the text scale.
fn osd_w() -> u32 {
    helix_draw::theme::grow(OSD_W as i32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_names() {
        assert_eq!(osd_icon_names("audio-volume-high-symbolic"), ["audio-volume-high-symbolic"]);
        assert_eq!(osd_icon_names(". GThemedIcon display-brightness-symbolic display-brightness"), ["display-brightness-symbolic", "display-brightness"]);
        assert!(osd_icon_names("/usr/share/icons/x.png").is_empty());
        assert!(osd_icon_names("").is_empty());
    }
}
