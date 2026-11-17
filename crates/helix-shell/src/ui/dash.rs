//! The dash in the overview (helixdesktop/spec `contracts/wayland/README.md`,
//! "Favorites and app folders").

use helix_draw::Image;

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::{accent, theme, Canvas, Rect, Ui};
use crate::item_feedback::Look;
use crate::item_feedback::LAUNCH_LINGER;

/// Icon size and spacing (logical pixels).
pub(super) const DASH_ICON: i32 = 48;
pub(super) const DASH_GAP: i32 = 10;
/// The dash's height, padding included.
pub(super) const DASH_H: i32 = DASH_ICON + 24;

/// One dash icon.
pub(super) struct DashEntry {
    /// The app id (desktop file id without `.desktop`).
    pub app: String,
    pub icon: Option<Image>,
    /// Its windows, most recently used first.
    pub windows: Vec<u32>,
    /// After the gap (not a favorite).
    pub running_only: bool,
    /// Its windows' titles, for the tooltip.
    pub titles: Vec<String>,
}

/// What the dash shows at one moment.
pub(super) struct DashPaint<'a> {
    pub entries: &'a [DashEntry],
    /// Hover, press and launch feedback per entry ("Item feedback").
    pub looks: Vec<Look>,
    /// Launch copies growing out of entries: (entry, progress 0 to 1).
    pub ghosts: Vec<(usize, f64)>,
    /// The entry chosen with the keyboard (the focus ring).
    pub keyboard: Option<usize>,
    /// The focused window's app: its running marks are one accent pill.
    pub focused_app: &'a str,
    /// The dock's `running-indicator-style`.
    pub indicator: crate::dock::Indicator,
    /// The entry whose name (and windows) shows above it.
    pub tooltip: Option<usize>,
    /// An icon being dragged (app, pointer).
    pub drag: Option<(String, (f64, f64))>,
}

/// Paint the dash centered at the bottom of a `w`×`h` surface, its items
/// looking and answering as the dock's do: the highlight behind a lit or
/// pressed icon, the icon's feedback size, launch copies, running marks
/// in the dock's style and the tooltip above. Returns where each entry's
/// icon is, and the top of what was painted (the tooltip's, or the bar's).
pub(super) fn paint_dash(canvas: &mut Canvas, text: Option<&crate::text::Text>, (w, h): (i32, i32), m: &DashPaint) -> (Vec<Rect>, i32) {
    use super::dock::{draw_indicator, paint_item_highlight, paint_item_icon, paint_launch_copy, paint_tooltip, tooltip_size};
    let dash = m.entries;
    let after_gap = |i: usize| i > 0 && dash[i].running_only && !dash[i - 1].running_only;
    let gaps = (0..dash.len()).filter(|i| after_gap(*i)).count() as i32;
    let n = dash.len() as i32;
    let dw = n * DASH_ICON + (n - 1).max(0) * DASH_GAP + gaps * 16 + 2 * 12;
    let (dx, dy) = ((w - dw) / 2, h - DASH_H - 16);
    let mut top = dy;
    theme::popover_round(canvas, f64::from(dx), f64::from(dy), f64::from(dw), f64::from(DASH_H), theme::RADIUS_POPOVER + 6.0);
    let mut rects = Vec::with_capacity(dash.len());
    let mut x = dx + 12;
    for (i, entry) in dash.iter().enumerate() {
        if after_gap(i) {
            // A separator between favorites and the rest.
            theme::vseparator(canvas, x + 2, dy + 14, DASH_ICON - 8);
            x += 16;
        }
        let r = Rect { x, y: dy + 8, w: DASH_ICON, h: DASH_ICON };
        let look = m.looks.get(i).copied().unwrap_or(Look { highlight: 0.0, pressed: false, scale: 1.0 });
        paint_item_highlight(canvas, r.x, r.y, DASH_ICON, look);
        if m.keyboard == Some(i) {
            helix_draw::theme::focus_ring_round(canvas, f64::from(r.x - 4), f64::from(r.y - 4), f64::from(r.w + 8), f64::from(r.h + 8), 8.0);
        }
        let dragged = m.drag.as_ref().is_some_and(|(app, _)| *app == entry.app);
        if let (false, Some(icon)) = (dragged, &entry.icon) {
            paint_item_icon(canvas, center(r), DASH_ICON, look.scale, icon);
        }
        // Running marks under the icon, as on the dock.
        let cell = Rect { x: r.x, y: dy, w: DASH_ICON, h: DASH_H };
        let focused = !m.focused_app.is_empty() && entry.app.eq_ignore_ascii_case(m.focused_app);
        draw_indicator(canvas, m.indicator, crate::dock::Edge::Bottom, cell, entry.windows.len().min(4), focused);
        rects.push(r);
        x += DASH_ICON + DASH_GAP;
    }
    for &(i, p) in &m.ghosts {
        if let (Some(entry), Some(&r)) = (dash.get(i), rects.get(i)) {
            if let Some(icon) = entry.icon.as_ref() {
                paint_launch_copy(canvas, center(r), DASH_ICON, icon, p);
            }
        }
    }
    if let Some((entry, &r)) = m.tooltip.and_then(|i| Some((dash.get(i)?, rects.get(i)?))) {
        let name = crate::windows::app_names().get(&entry.app.to_lowercase()).cloned().unwrap_or_else(|| entry.app.clone());
        let lines = crate::dock::tooltip_lines(&name, &entry.titles);
        let (tw, th) = tooltip_size(text, &lines);
        let lw = tw.min(w);
        let lx = (r.x + r.w / 2 - lw / 2).clamp(0, (w - lw).max(0));
        top = dy - th - 6;
        paint_tooltip(canvas, text, Rect { x: lx, y: top, w: lw, h: th }, &lines);
    }
    // A dragged icon: where it would go, and the icon itself.
    if let Some((app, (px, py))) = m.drag.as_ref().map(|(a, at)| (a.as_str(), *at)) {
        let favorites: Vec<Rect> = dash.iter().zip(&rects).filter(|(e, _)| !e.running_only && e.app != app).map(|(_, r)| *r).collect();
        if py >= f64::from(dy) - super::overview::DASH_UNPIN {
            let i = super::overview::dash_insert_index(&favorites, px);
            let mx = favorites.get(i).map_or_else(|| favorites.last().map_or(dx + 12, |r| r.x + r.w + DASH_GAP / 2), |r| r.x - DASH_GAP / 2);
            canvas.rect(mx - 1, dy + 6, 3, DASH_H - 12, accent());
        }
        if let Some(icon) = dash.iter().find(|e| e.app == app).and_then(|e| e.icon.as_ref()) {
            canvas.image(px - f64::from(DASH_ICON) / 2.0, py - f64::from(DASH_ICON) / 2.0, f64::from(DASH_ICON), f64::from(DASH_ICON), icon);
        }
    }
    (rects, top)
}

/// The middle of `r`.
fn center(r: Rect) -> (f64, f64) {
    (f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(r.y) + f64::from(r.h) / 2.0)
}

/// An app id of a desktop file id.
fn app_of(desktop_id: &str) -> &str {
    desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id)
}

/// Favorites, then other apps with windows: (app id, running only).
pub(super) fn dash_apps(favorites: &[String], installed: &[String], window_apps: &[String]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = favorites.iter().filter(|f| installed.contains(f)).map(|f| (app_of(f).to_owned(), false)).collect();
    for app in window_apps {
        if !app.is_empty() && !out.iter().any(|(a, _)| a.eq_ignore_ascii_case(app)) {
            out.push((app.clone(), true));
        }
    }
    out
}

impl Ui {
    /// The dash's entries, with their icons.
    pub(super) fn dash_entries(&mut self, scale: f64) -> Vec<DashEntry> {
        // As the launcher counts them: apps shown in menus.
        let installed = crate::icons::apps().shown.clone();
        let window_apps: Vec<String> = self.windows.mru().iter().map(|e| e.app_id.clone()).collect();
        let px = (f64::from(DASH_ICON) * scale).round() as u32;
        dash_apps(&crate::favorites::favorites(), &installed, &window_apps)
            .into_iter()
            .map(|(app, running_only)| {
                let mine: Vec<&crate::windows::WindowEntry> = self.windows.mru().iter().filter(|e| e.app_id.eq_ignore_ascii_case(&app)).collect();
                let windows = mine.iter().map(|e| e.id).collect();
                let titles = mine.iter().map(|e| e.title.clone()).collect();
                let key = format!("app:{app}@{px}");
                let id = app.clone();
                let icon = self.icon_cache.get(key, move || crate::icons::app_icon(&id, px));
                DashEntry { app, icon, windows, running_only, titles }
            })
            .collect()
    }

    /// A dash icon was clicked: its newest window, else launch it. A
    /// launch with animations keeps the overview for the launch copy
    /// ([`LAUNCH_LINGER`]), as the app grid does; going to a window closes
    /// it at once.
    pub(super) fn dash_activate(&mut self, app: &str, window: Option<u32>) {
        if let Some(id) = window {
            self.close_overview(true);
            self.activate_window(id);
            return;
        }
        let id = format!("{app}.desktop");
        tracing::info!(app = id, "launching from the dash");
        if let Err(err) = crate::launcher::launch(&id) {
            tracing::warn!(%err, app = id, "launch failed");
        }
        let lingering = self.overview.as_ref().is_some_and(|o| o.state.linger.is_some() || !o.state.dash_feedback.animate);
        let token = match (lingering, self.loop_handle.as_ref()) {
            (false, Some(handle)) => handle
                .insert_source(Timer::from_duration(LAUNCH_LINGER), |_, _, ui: &mut Ui| {
                    // Only the overview this was for (not one opened since).
                    if ui.overview.as_ref().is_some_and(|o| o.state.linger.is_some()) {
                        ui.close_overview(true);
                    }
                    TimeoutAction::Drop
                })
                .ok(),
            _ => None,
        };
        let Some(token) = token else {
            self.close_overview(true);
            return;
        };
        if let Some(o) = self.overview.as_mut() {
            o.state.dash_feedback.launch(app.to_owned(), std::time::Instant::now());
            o.state.linger = Some(token);
        }
        self.overview_dash_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorites_then_running() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let apps = dash_apps(
            &s(&["org.gnome.Calculator.desktop", "gone.desktop", "org.gnome.Nautilus.desktop"]),
            &s(&["org.gnome.Calculator.desktop", "org.gnome.Nautilus.desktop", "firefox.desktop"]),
            &s(&["firefox", "org.gnome.calculator", "firefox", ""]),
        );
        assert_eq!(apps, [("org.gnome.Calculator".to_owned(), false), ("org.gnome.Nautilus".to_owned(), false), ("firefox".to_owned(), true)]);
    }
}
