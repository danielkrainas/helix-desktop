//! Notification banners and the notification list (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.Notifications.md`, "Helix"): icons,
//! body markup with links, and the list opened from the panel's clock.

use std::collections::HashSet;

use helix_draw::{draw_lines, wrap_spans, Fonts, Image, Span};

use super::*;
use crate::icons::IconSource;
use crate::notifications::{Banner, List};
use helix_i18n::tr;
use helix_i18n::trf;

/// Icon size in the banner and the list (logical pixels).
pub(super) const ICON: i32 = 32;
/// Icons are decoded at this many pixels, sharp up to scale 2.
const ICON_PX: u32 = 64;
const LIST_PAD: i32 = 10;
const BODY_PX: f32 = 14.0;

/// The list column's width, a body line's height and a group header's:
/// they grow with the text scale.
fn list_w() -> i32 {
    helix_draw::theme::grow(420)
}

fn body_line() -> i32 {
    helix_draw::theme::grow(19)
}

fn group_header() -> i32 {
    helix_draw::theme::grow(30)
}

/// A notification's height above its body: app name and summary.
fn heading_h() -> i32 {
    helix_draw::theme::grow(16) + helix_draw::theme::grow(26)
}

/// What a click on the banner hit.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum BannerHit {
    Body,
    Close,
    Button(String),
    Link(String),
}

/// What a click on the notification list hit.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ListHit {
    Entry(u32),
    /// The calendar: another month (by), a day, a media button.
    Month(i32),
    Day(i32, i32, i32),
    Media(crate::shell_keys::Playback),
    Dismiss(u32),
    Link(String),
    Clear,
    /// A group's header: show or hide its other notifications.
    GroupToggle(String),
    /// Dismiss a group's notifications.
    GroupClear(String),
    DoNotDisturb,
    Inside,
    Outside,
}

/// The open notification list: a full-output overlay with a box under the
/// clock.
pub(super) struct ListView {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    hits: Vec<(Rect, ListHit)>,
    /// Rows scrolled past.
    scroll: usize,
    /// Groups opened to show all their notifications.
    expanded: HashSet<String>,
    /// How it was opened (from the keyboard it takes the keyboard).
    opened: super::quick::Opened,
    /// What has the keyboard focus.
    focus: Option<ListHit>,
    /// The controls the keyboard reaches, in Tab order, where the last
    /// paint put them.
    stops: Vec<(Rect, ListHit)>,
    /// Logical px of the calendar column scrolled past (when it is taller
    /// than the screen), and where it was shown.
    cal_scroll: i32,
    cal_area: Rect,
}


/// A row of the notification list.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Row {
    /// A group of two or more: its key, app name, count, whether open.
    Header { key: String, app: String, count: usize, open: bool },
    /// A notification, by index into the entries.
    Entry(usize),
}

/// What groups notifications: the app id, else the app name.
pub(super) fn group_key(banner: &Banner) -> String {
    if banner.app_id.is_empty() {
        format!("name:{}", banner.app_name)
    } else {
        format!("app:{}", banner.app_id)
    }
}

/// The list's rows for `entries` (newest first): groups in the order of
/// their newest notification; a group of one is just its notification; a
/// larger group a header and its newest notification, or all of them when
/// open.
pub(super) fn list_rows(entries: &[Banner], expanded: &HashSet<String>) -> Vec<Row> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let key = group_key(e);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    let mut rows = Vec::new();
    for (key, members) in groups {
        if members.len() == 1 {
            rows.push(Row::Entry(members[0]));
            continue;
        }
        let open = expanded.contains(&key);
        rows.push(Row::Header { key, app: entries[members[0]].app_name.clone(), count: members.len(), open });
        let shown = if open { members.len() } else { 1 };
        rows.extend(members.into_iter().take(shown).map(Row::Entry));
    }
    rows
}

/// Notification state the UI keeps.
#[derive(Default)]
pub(super) struct NotifyState {
    pub list: List,
    pub do_not_disturb: bool,
    pub view: Option<ListView>,
}

fn spans(banner: &Banner) -> Vec<Span> {
    if banner.markup {
        crate::markup::parse(&banner.body)
    } else {
        crate::markup::text(&banner.body)
    }
}

/// Draw one notification's icon, app, summary and body with its top-left
/// at (x, y) in `width`; returns link areas and the bottom.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_notification(
    canvas: &mut Canvas,
    (text, bold): (Option<&Text>, Option<&Text>),
    icon: Option<&Image>,
    banner: &Banner,
    (x, y): (i32, i32),
    width: i32,
    max_lines: usize,
) -> (Vec<(Rect, String)>, i32) {
    let text_x = x + ICON + 12;
    let text_w = (width - ICON - 12) as f32;
    match icon {
        Some(image) => {
            // Keep the aspect ratio inside the icon square.
            let (iw, ih) = (image.width as f64, image.height as f64);
            let s = f64::from(ICON) / iw.max(ih);
            let (w, h) = (iw * s, ih * s);
            let (ix, iy) = (f64::from(x) + (f64::from(ICON) - w) / 2.0, f64::from(y + 4) + (f64::from(ICON) - h) / 2.0);
            canvas.image(ix, iy, w, h, image);
        }
        None => canvas.circle(f64::from(x + ICON / 2), f64::from(y + 4 + ICON / 2), f64::from(ICON) / 2.0 - 2.0, theme::BORDER),
    }
    let grow = helix_draw::theme::grow;
    // The app, and when it arrived ("Chat · 5 min").
    let when = gio::glib::DateTime::now_local().ok().map(|now| crate::notifications::arrival(banner.arrived, &now, false)).unwrap_or_default();
    let head = match (banner.app_name.is_empty(), when.is_empty()) {
        (_, true) => banner.app_name.clone(),
        (true, false) => when,
        (false, false) => format!("{} · {when}", banner.app_name),
    };
    label(text, canvas, &head, text_x as f32, (y + grow(12)) as f32, 12.0, DIM_FG, text_w - 24.0);
    let mut bottom = y + grow(16);
    match bold {
        Some(bold) => bold.draw(canvas, &banner.summary, text_x as f32, (bottom + grow(20)) as f32, Style { px: 16.0, color: FG, max_width: text_w }),
        None => label(text, canvas, &banner.summary, text_x as f32, (bottom + grow(20)) as f32, 16.0, FG, text_w),
    }
    bottom += grow(26);
    let mut links = Vec::new();
    if let (Some(regular), false) = (text, banner.body.is_empty()) {
        let fonts = Fonts { regular, bold };
        let spans = spans(banner);
        let lines = wrap_spans(&fonts, &spans, BODY_PX, text_w, max_lines);
        let areas = draw_lines(canvas, &fonts, &spans, &lines, (text_x as f32, (bottom + grow(14)) as f32), BODY_PX, body_line() as f32, (DIM_FG, accent_text()));
        for ((lx, ly, lw, lh), uri) in areas {
            links.push((Rect { x: lx as i32, y: ly as i32, w: lw.ceil() as i32, h: lh as i32 }, uri));
        }
        bottom += lines.len() as i32 * body_line();
    }
    (links, bottom.max(y + ICON + 4))
}

impl Ui {
    fn fonts(&self) -> Option<Fonts<'_>> {
        Some(Fonts { regular: self.text.as_ref()?, bold: self.bold.as_ref() })
    }

    /// The icon of a notification, decoded once (on a worker).
    fn notification_icon(&mut self, source: &Option<IconSource>) -> Option<Image> {
        super::source_icon(&mut self.icon_cache, source.clone()?, ICON_PX)
    }

    /// Body lines of a notification at `width`.
    fn body_lines(&self, banner: &Banner, width: f32, max_lines: usize) -> usize {
        match self.fonts() {
            Some(fonts) if !banner.body.is_empty() => wrap_spans(&fonts, &spans(banner), BODY_PX, width, max_lines).len(),
            Some(_) => 0,
            None => usize::from(!banner.body.is_empty()),
        }
    }

    /// Banner height for its content.
    pub(super) fn banner_height(&self, banner: &Banner) -> u32 {
        let text_w = (banner_w() as i32 - 2 * BANNER_PAD - ICON - 12) as f32;
        let body = self.body_lines(banner, text_w, banner_lines(banner)) as i32;
        let buttons = if banner.buttons.is_empty() { 0 } else { 44 };
        let content = (heading_h() + body * body_line()).max(ICON + 4);
        (BANNER_PAD + content + buttons + BANNER_PAD) as u32
    }

    pub(super) fn paint_banner(&mut self, view: &mut BannerView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let icon = self.notification_icon(&view.data.icon);
        let scale = self.scales.of(view.layer.wl_surface());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate banner buffer");
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(view.layer.wl_surface()));
        // A popover, rounded, its edge inside the surface.
        canvas.fill(0);
        theme::popover_round(&mut canvas, 1.0, 1.0, w as f64 - 2.0, h as f64 - 2.0, theme::RADIUS_POPOVER);
        if view.focused {
            // The focus ring, inside the banner's edge.
            theme::focus_ring_round(&mut canvas, 5.0, 5.0, w as f64 - 10.0, h as f64 - 10.0, theme::RADIUS_POPOVER - 4.0);
        }
        view.hits.clear();
        let inner = w as i32 - 2 * BANNER_PAD;
        // Close button, top right.
        let close = Rect { x: w as i32 - BANNER_PAD - 18, y: BANNER_PAD - 4, w: 22, h: 22 };
        let text = self.text.as_ref();
        let (links, mut y) =
            paint_notification(&mut canvas, (text, self.bold.as_ref()), icon.as_ref(), &view.data, (BANNER_PAD, BANNER_PAD), inner - 24, banner_lines(&view.data));
        label(text, &mut canvas, "×", (close.x + 5) as f32, (close.y + 17) as f32, 18.0, DIM_FG, 20.0);
        view.hits.push((close, BannerHit::Close));
        for (rect, uri) in links {
            view.hits.push((rect, BannerHit::Link(uri)));
        }
        if !view.data.buttons.is_empty() {
            y += 8;
            let n = view.data.buttons.len() as i32;
            let gap = 8;
            let bw = (w as i32 - 2 * BANNER_PAD - gap * (n - 1)) / n;
            for (i, (key, button_label)) in view.data.buttons.iter().enumerate() {
                let r = Rect { x: BANNER_PAD + i as i32 * (bw + gap), y, w: bw, h: 32 };
                let hit = BannerHit::Button(key.clone());
                let look = theme::Look { hovered: view.hover.as_ref() == Some(&hit), pressed: view.pressed.as_ref() == Some(&hit), ..Default::default() };
                theme::button(&mut canvas, text, (r.x, r.y, r.w, r.h), button_label, theme::Kind::Normal, look, false);
                view.hits.push((r, BannerHit::Button(key.clone())));
            }
        }
        // Anything else on the banner is its body.
        view.hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, BannerHit::Body));

        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    /// The pointer entering or leaving the banner holds its timer.
    pub(super) fn banner_hover(&mut self, event: &PointerEvent) {
        let Some(view) = self.banner.as_ref().filter(|b| b.layer.wl_surface() == &event.surface) else { return };
        let held = match event.kind {
            PointerEventKind::Enter { .. } => true,
            PointerEventKind::Leave { .. } => false,
            _ => return,
        };
        let _ = self.requests.send(UiRequest::BannerHeld { id: view.data.id, held: held || view.focused });
    }

    /// Give the banner keyboard focus (or take it away).
    pub(super) fn focus_banner(&mut self, focus: bool) {
        let Some(view) = self.banner.as_mut() else { return };
        if view.focused == focus {
            return;
        }
        view.focused = focus;
        let _ = self.requests.send(UiRequest::BannerHeld { id: view.data.id, held: focus });
        view.layer.set_keyboard_interactivity(if focus { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None });
        view.layer.commit();
        self.draw_banner();
    }

    /// A key while the banner has focus.
    pub(super) fn banner_key(&mut self, event: &KeyEvent) {
        let Some(view) = &self.banner else { return };
        let id = view.data.id;
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => {
                self.focus_banner(false);
                let _ = self.requests.send(UiRequest::NotificationActivated { id, action: None });
            }
            Keysym::Delete | Keysym::KP_Delete => {
                self.focus_banner(false);
                let _ = self.requests.send(UiRequest::NotificationDismissed { id });
            }
            Keysym::Escape => self.focus_banner(false),
            _ => {}
        }
    }

    pub(super) fn banner_click(&mut self, x: f64, y: f64) {
        let Some(view) = &self.banner else { return };
        let Some(hit) = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone()) else { return };
        let id = view.data.id;
        let request = match hit {
            BannerHit::Close => UiRequest::NotificationDismissed { id },
            BannerHit::Body => UiRequest::NotificationActivated { id, action: None },
            BannerHit::Button(key) => UiRequest::NotificationActivated { id, action: Some(key) },
            BannerHit::Link(uri) => UiRequest::OpenUri(uri),
        };
        let _ = self.requests.send(request);
    }

    /// The list changed (or what is unseen).
    pub(super) fn notifications_changed(&mut self, list: List) {
        let had_dot = self.notify.list.unseen && !self.notify.list.entries.is_empty();
        self.notify.list = list;
        if self.notify.view.is_some() {
            if self.notify.list.unseen {
                let _ = self.requests.send(UiRequest::NotificationListOpened);
            }
            self.draw_notification_list();
        }
        if had_dot != self.unseen_dot() {
            self.draw_panel();
        }
        if self.lock.session.is_some() {
            self.draw_lock();
        }
    }

    /// Whether the panel shows the unseen-notifications dot.
    pub(super) fn unseen_dot(&self) -> bool {
        self.notify.list.unseen && !self.notify.list.entries.is_empty() && self.notify.view.is_none()
    }

    /// The open list for the accessibility bus (`ui/atspi.rs`): its
    /// surface, size and what its places do.
    pub(super) fn notification_list_a11y(&self) -> Option<super::atspi::ListShown<'_>> {
        let view = self.notify.view.as_ref().filter(|v| v.configured)?;
        Some((view.layer.wl_surface(), (view.width, view.height), &view.hits, view.focus.as_ref()))
    }

    /// Open the list (from the keyboard: taking the keyboard, the first
    /// notification focused), or close it.
    pub(super) fn toggle_notification_list(&mut self, opened: super::quick::Opened) {
        if self.notify.view.is_some() {
            self.close_notification_list();
            return;
        }
        self.close_launcher(true);
        self.close_quick_settings();
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-notification-list"), None);
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(opened.interactivity());
        layer.set_size(0, 0);
        layer.commit();
        // (`Inside`: the first notification, once painted.)
        let focus = opened.keyboard().then_some(ListHit::Inside);
        self.notify.view = Some(ListView { layer, width: 0, height: 0, configured: false, hits: Vec::new(), scroll: 0, expanded: HashSet::new(), opened, focus, stops: Vec::new(), cal_scroll: 0, cal_area: Rect::default() });
        let _ = self.requests.send(UiRequest::NotificationListOpened);
        self.calendar_open();
        self.draw_panel();
    }

    pub(super) fn notify_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(view) = self.notify.view.as_mut().filter(|v| &v.layer == layer) else { return false };
        view.width = size.0;
        view.height = size.1;
        view.configured = true;
        self.draw_notification_list();
        true
    }

    pub(super) fn notify_closed(&mut self, layer: &LayerSurface) -> bool {
        if self.notify.view.as_ref().is_some_and(|v| &v.layer == layer) {
            self.notify.view = None;
            self.draw_panel();
            return true;
        }
        false
    }

    pub(super) fn draw_notification_list(&mut self) {
        let _busy = crate::watchdog::scope("draw notification list");
        let Some(mut view) = self.notify.view.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_notification_list(&mut view);
        }
        self.notify.view = Some(view);
    }

    fn paint_notification_list(&mut self, view: &mut ListView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let entries = self.notify.list.entries.clone();
        view.expanded.retain(|key| entries.iter().any(|e| group_key(e) == *key));
        let rows = list_rows(&entries, &view.expanded);
        view.scroll = view.scroll.min(rows.len().saturating_sub(1));
        let indents = row_indents(&entries, &rows);
        let heights: Vec<i32> = rows
            .iter()
            .zip(&indents)
            .map(|(row, indent)| match row {
                Row::Header { .. } => group_header(),
                Row::Entry(i) => entry_height(self.fonts().as_ref(), &entries[*i], *indent),
            })
            .collect();
        let icons: Vec<Option<Image>> = rows
            .iter()
            .skip(view.scroll)
            .map(|row| match row {
                Row::Entry(i) => self.notification_icon(&entries[*i].icon),
                Row::Header { .. } => None,
            })
            .collect();
        let scale = self.scales.of(view.layer.wl_surface());
        let media_icons = self.calendar_media_icons(scale);
        let cal_h = self.calendar_height();
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(super::mirror::on(view.layer.wl_surface()));
        // A focus that is not there (yet, or any more) moves to the first
        // notification, else Do Not Disturb.
        let targets = row_targets(&entries, &rows);
        let model = ListModel { entries: &entries, rows: &rows, heights: &heights, icons: &icons, scroll: view.scroll, do_not_disturb: self.notify.do_not_disturb, calendar: &self.calendar, media_icons: &media_icons, cal_h, cal_scroll: view.cal_scroll, hover: None, pressed: None };
        let fonts = (self.text.as_ref(), self.bold.as_ref());
        let mut painted = paint_list(&mut canvas, fonts, (w, h), &model, view.focus.as_ref());
        let lost = view.focus.as_ref().is_some_and(|f| !painted.stops.iter().any(|(_, s)| s == f) && !targets.contains(f));
        if lost || view.focus == Some(ListHit::Inside) {
            view.focus = Some(targets.first().cloned().unwrap_or(ListHit::DoNotDisturb));
            painted = paint_list(&mut canvas, fonts, (w, h), &model, view.focus.as_ref());
        }
        view.hits = painted.hits;
        view.stops = painted.stops;
        view.cal_scroll = painted.cal_scroll;
        view.cal_area = painted.cal_area;
        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    /// Do what a click on `hit` does.
    fn list_activate(&mut self, hit: ListHit) {
        let Some(view) = self.notify.view.as_mut() else { return };
        match hit {
            ListHit::Entry(id) => {
                let _ = self.requests.send(UiRequest::NotificationActivated { id, action: None });
                self.close_notification_list();
            }
            ListHit::Dismiss(id) => {
                let _ = self.requests.send(UiRequest::NotificationDismissed { id });
            }
            ListHit::Link(uri) => {
                let _ = self.requests.send(UiRequest::OpenUri(uri));
                self.close_notification_list();
            }
            ListHit::Clear => {
                let _ = self.requests.send(UiRequest::NotificationsCleared);
            }
            ListHit::GroupToggle(key) => {
                if !view.expanded.remove(&key) {
                    view.expanded.insert(key);
                }
                self.draw_notification_list();
            }
            ListHit::GroupClear(key) => {
                for e in self.notify.list.entries.iter().filter(|e| group_key(e) == key) {
                    let _ = self.requests.send(UiRequest::NotificationDismissed { id: e.id });
                }
            }
            ListHit::DoNotDisturb => {
                let on = !self.notify.do_not_disturb;
                self.notify.do_not_disturb = on;
                let _ = self.requests.send(UiRequest::SetDoNotDisturb(on));
                self.draw_notification_list();
            }
            hit @ (ListHit::Month(_) | ListHit::Day(..) | ListHit::Media(_)) => self.calendar_click(&hit),
            ListHit::Inside => {}
            ListHit::Outside => self.close_notification_list(),
        }
    }

    /// Pointer input on the list; true if it was for it.
    pub(super) fn notification_list_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.notify.view.as_mut().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        match event.kind {
            PointerEventKind::Axis { vertical, .. } if vertical.absolute != 0.0 || vertical.discrete != 0 => {
                let down = vertical.absolute > 0.0 || vertical.discrete > 0;
                if view.cal_area.contains(event.position.0, event.position.1) {
                    // The calendar column (when it does not fit).
                    view.cal_scroll = (view.cal_scroll + if down { 48 } else { -48 }).max(0);
                } else {
                    let count = list_rows(&self.notify.list.entries, &view.expanded).len();
                    view.scroll = if down { (view.scroll + 1).min(count.saturating_sub(1)) } else { view.scroll.saturating_sub(1) };
                }
                self.draw_notification_list();
            }
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                let (x, y) = event.position;
                let hit = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone());
                self.list_activate(hit.unwrap_or(ListHit::Outside));
            }
            _ => {}
        }
        true
    }

    /// Close the list; the panel's clock gets the focus back when it was
    /// opened from there.
    pub(super) fn close_notification_list(&mut self) {
        let Some(view) = self.notify.view.take() else { return };
        if let super::quick::Opened::Panel(i) = view.opened {
            self.refocus_panel(i);
        }
        drop(view);
        self.draw_panel();
    }

    /// Focus `hit`, scrolling the list so a notification row shows.
    fn list_focus(&mut self, hit: ListHit) {
        let Some(view) = self.notify.view.as_mut() else { return };
        view.focus = Some(hit.clone());
        let rows = list_rows(&self.notify.list.entries, &view.expanded);
        let targets = row_targets(&self.notify.list.entries, &rows);
        if let Some(index) = targets.iter().position(|t| *t == hit) {
            if index < view.scroll {
                view.scroll = index;
            }
        }
        self.draw_notification_list();
        // Further down than what fits: scroll until it shows.
        for _ in 0..targets.len() {
            let Some(view) = self.notify.view.as_mut() else { return };
            if view.hits.iter().any(|(_, h)| *h == hit) || !targets.contains(&hit) {
                break;
            }
            view.scroll += 1;
            self.draw_notification_list();
        }
    }

    /// A key while the list is open.
    pub(super) fn notification_list_key(&mut self, event: &KeyEvent) {
        let Some(view) = self.notify.view.as_ref() else { return };
        let rows = list_rows(&self.notify.list.entries, &view.expanded);
        let targets = row_targets(&self.notify.list.entries, &rows);
        let action = list_key_action(event.keysym, self.ctrl_held, &view.stops, view.focus.as_ref(), &targets, &self.notify.list.entries);
        match action {
            ListKey::None => {}
            ListKey::Close => self.close_notification_list(),
            ListKey::Focus(hit) => self.list_focus(hit),
            ListKey::Activate(hit) => self.list_activate(hit),
            ListKey::Expand(key, open) => {
                if let Some(view) = self.notify.view.as_mut() {
                    if open {
                        view.expanded.insert(key.clone());
                    } else {
                        view.expanded.remove(&key);
                    }
                }
                self.list_focus(ListHit::GroupToggle(key));
            }
            ListKey::Day(days) => self.calendar_move(super::calendar::CalendarMove::Days(days)),
            ListKey::Month(months) => self.calendar_move(super::calendar::CalendarMove::Months(months)),
            ListKey::Today => self.calendar_move(super::calendar::CalendarMove::Today),
        }
    }
}

/// The keyboard moved the calendar's selected day: it keeps the focus.
pub(super) fn view_focus_day(view: &mut ListView, (y, m, d): (i32, i32, i32)) {
    if matches!(view.focus, Some(ListHit::Day(..))) {
        view.focus = Some(ListHit::Day(y, m, d));
    }
}

/// The notification column's rows as the keyboard reaches them: a
/// notification, or a group's header.
pub(super) fn row_targets(entries: &[Banner], rows: &[Row]) -> Vec<ListHit> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Header { key, .. } => Some(ListHit::GroupToggle(key.clone())),
            Row::Entry(i) => entries.get(*i).map(|e| ListHit::Entry(e.id)),
        })
        .collect()
}

/// Whether `hit` is in the calendar column.
fn in_calendar(hit: &ListHit) -> bool {
    matches!(hit, ListHit::Month(_) | ListHit::Day(..) | ListHit::Media(_))
}

/// What a key does in the open list.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ListKey {
    None,
    Close,
    Focus(ListHit),
    Activate(ListHit),
    /// Open or close a group.
    Expand(String, bool),
    /// Move the selected day, change the month, go to today.
    Day(i32),
    Month(i32),
    Today,
}

/// What `keysym` does with the keyboard on `focus`; `stops` are the
/// controls painted (in Tab order), `targets` every notification row.
pub(super) fn list_key_action(keysym: Keysym, ctrl: bool, stops: &[(Rect, ListHit)], focus: Option<&ListHit>, targets: &[ListHit], entries: &[Banner]) -> ListKey {
    if keysym == Keysym::Escape {
        return ListKey::Close;
    }
    let order: Vec<&ListHit> = stops.iter().map(|(_, h)| h).collect();
    let Some(focus) = focus.filter(|f| order.contains(f)) else {
        // Nothing focused yet (opened by a click).
        return match keysym {
            Keysym::Tab | Keysym::Down | Keysym::Up | Keysym::ISO_Left_Tab => ListKey::Focus(targets.first().cloned().unwrap_or(ListHit::DoNotDisturb)),
            _ => ListKey::None,
        };
    };
    let at = order.iter().position(|h| *h == focus).unwrap_or(0);
    let n = order.len();
    let calendar = in_calendar(focus);
    match keysym {
        // Ctrl+Tab: the other column.
        Keysym::Tab | Keysym::ISO_Left_Tab if ctrl => {
            let other = order.iter().find(|h| in_calendar(h) != calendar);
            return other.map_or(ListKey::None, |h| ListKey::Focus((*h).clone()));
        }
        Keysym::Tab => return ListKey::Focus(order[(at + 1) % n].clone()),
        Keysym::ISO_Left_Tab => return ListKey::Focus(order[(at + n - 1) % n].clone()),
        _ => {}
    }
    // The notification a focused part (close button, link) belongs to:
    // the row before it in Tab order.
    let owner = || order[..=at].iter().rev().find(|h| targets.contains(h)).map(|h| (*h).clone());
    let row = targets.iter().position(|t| t == focus).or_else(|| owner().and_then(|o| targets.iter().position(|t| *t == o)));
    let activate = || ListKey::Activate(focus.clone());
    if let ListHit::Day(..) = focus {
        return match keysym {
            Keysym::Left | Keysym::KP_Left => ListKey::Day(-1),
            Keysym::Right | Keysym::KP_Right => ListKey::Day(1),
            Keysym::Up | Keysym::KP_Up => ListKey::Day(-7),
            Keysym::Down | Keysym::KP_Down => ListKey::Day(7),
            Keysym::Page_Up => ListKey::Month(-1),
            Keysym::Page_Down => ListKey::Month(1),
            Keysym::Home => ListKey::Today,
            _ => ListKey::None,
        };
    }
    if calendar {
        // The month and media buttons: Left/Right along them, Up/Down
        // to the days.
        let day = order.iter().find(|h| matches!(h, ListHit::Day(..))).map(|h| (*h).clone());
        let same = |h: &&&ListHit| std::mem::discriminant(**h) == std::mem::discriminant(focus);
        return match keysym {
            Keysym::Left | Keysym::KP_Left => order[..at].iter().rev().find(same).map_or(ListKey::None, |h| ListKey::Focus((*h).clone())),
            Keysym::Right | Keysym::KP_Right => order[at + 1..].iter().find(same).map_or(ListKey::None, |h| ListKey::Focus((*h).clone())),
            Keysym::Down | Keysym::KP_Down if matches!(focus, ListHit::Month(_)) => day.map_or(ListKey::None, ListKey::Focus),
            Keysym::Up | Keysym::KP_Up if matches!(focus, ListHit::Media(_)) => day.map_or(ListKey::None, ListKey::Focus),
            Keysym::Page_Up => ListKey::Month(-1),
            Keysym::Page_Down => ListKey::Month(1),
            Keysym::Return | Keysym::KP_Enter | Keysym::space => activate(),
            _ => ListKey::None,
        };
    }
    match keysym {
        Keysym::Down | Keysym::KP_Down => match row {
            Some(r) => targets.get(r + 1).map_or(ListKey::None, |t| ListKey::Focus(t.clone())),
            None => targets.first().map_or(ListKey::None, |t| ListKey::Focus(t.clone())),
        },
        Keysym::Up | Keysym::KP_Up => match row {
            Some(0) => ListKey::Focus(ListHit::DoNotDisturb),
            Some(r) => ListKey::Focus(targets[r - 1].clone()),
            None => ListKey::None,
        },
        Keysym::Right | Keysym::KP_Right => match focus {
            ListHit::GroupToggle(key) => ListKey::Expand(key.clone(), true),
            ListHit::DoNotDisturb if order.contains(&&ListHit::Clear) => ListKey::Focus(ListHit::Clear),
            _ => ListKey::None,
        },
        Keysym::Left | Keysym::KP_Left => match focus {
            ListHit::GroupToggle(key) => ListKey::Expand(key.clone(), false),
            ListHit::Clear => ListKey::Focus(ListHit::DoNotDisturb),
            // A grouped notification: back to (and closing) its group.
            _ => {
                let key = owner().and_then(|o| match o {
                    ListHit::Entry(id) => entries.iter().find(|e| e.id == id).map(group_key),
                    _ => None,
                });
                match key.filter(|k| targets.contains(&ListHit::GroupToggle(k.clone()))) {
                    Some(k) => ListKey::Expand(k, false),
                    None => ListKey::None,
                }
            }
        },
        Keysym::Delete | Keysym::KP_Delete => match owner() {
            Some(ListHit::Entry(id)) => ListKey::Activate(ListHit::Dismiss(id)),
            Some(ListHit::GroupToggle(key)) => ListKey::Activate(ListHit::GroupClear(key)),
            _ => ListKey::None,
        },
        Keysym::Return | Keysym::KP_Enter | Keysym::space => activate(),
        Keysym::Home => targets.first().map_or(ListKey::None, |t| ListKey::Focus(t.clone())),
        Keysym::End => targets.last().map_or(ListKey::None, |t| ListKey::Focus(t.clone())),
        _ => ListKey::None,
    }
}

/// How far a group's notifications are indented under its header.
const GROUP_INDENT: i32 = 12;

/// How far each row is indented: a notification under its group's
/// header by [`GROUP_INDENT`].
pub(super) fn row_indents(entries: &[Banner], rows: &[Row]) -> Vec<i32> {
    let mut group: Option<String> = None;
    rows.iter()
        .map(|row| match row {
            Row::Header { key, .. } => {
                group = Some(key.clone());
                0
            }
            Row::Entry(i) => {
                let key = entries.get(*i).map(group_key);
                if key.is_some() && key == group {
                    GROUP_INDENT
                } else {
                    group = None;
                    0
                }
            }
        })
        .collect()
}

/// The width a list entry indented by `indent` is painted at (its icon,
/// then its text), left of its ×.
fn entry_w(indent: i32) -> i32 {
    list_w() - 2 * LIST_PAD - 24 - indent
}

/// Height of one list entry indented by `indent`: its body wraps at the
/// width it is painted at.
pub(super) fn entry_height(fonts: Option<&Fonts>, banner: &Banner, indent: i32) -> i32 {
    let text_w = (entry_w(indent) - ICON - 12) as f32;
    let body = match fonts {
        Some(fonts) if !banner.body.is_empty() => wrap_spans(fonts, &spans(banner), BODY_PX, text_w, 2).len(),
        Some(_) => 0,
        None => usize::from(!banner.body.is_empty()),
    } as i32;
    (heading_h() + body * body_line()).max(ICON + 4) + 2 * LIST_PAD
}

/// What the list shows.
pub(super) struct ListModel<'a> {
    pub entries: &'a [Banner],
    pub rows: &'a [Row],
    /// Each row's height.
    pub heights: &'a [i32],
    /// Icons of the rows from `scroll` on.
    pub icons: &'a [Option<Image>],
    /// Rows scrolled past.
    pub scroll: usize,
    pub do_not_disturb: bool,
    pub calendar: &'a super::calendar::CalendarState,
    pub media_icons: &'a super::calendar::MediaIcons,
    pub cal_h: i32,
    /// Logical px of the calendar column scrolled past.
    pub cal_scroll: i32,
    /// The part under the pointer, and the one pressed.
    pub hover: Option<&'a ListHit>,
    pub pressed: Option<&'a ListHit>,
}

impl ListModel<'_> {
    /// How the part `hit` looks: hovered, pressed.
    pub(super) fn look(&self, hit: &ListHit) -> theme::Look {
        theme::Look { hovered: self.hover == Some(hit), pressed: self.pressed == Some(hit), ..Default::default() }
    }
}

/// What painting the list gives back: what clicks hit, and the controls
/// the keyboard reaches in Tab order.
pub(super) struct ListPainted {
    pub hits: Vec<(Rect, ListHit)>,
    pub stops: Vec<(Rect, ListHit)>,
    /// The calendar's scroll (kept in range, moved to show the focus) and
    /// where the column shows.
    pub cal_scroll: i32,
    pub cal_area: Rect,
}

/// Paint the list and calendar (`m`) on a `w`×`h` overlay, the focus ring
/// on `focus`.
pub(super) fn paint_list(canvas: &mut Canvas, fonts: (Option<&Text>, Option<&Text>), (w, h): (usize, usize), m: &ListModel, focus: Option<&ListHit>) -> ListPainted {
    let painted = paint_list_at(canvas, fonts, (w, h), m, focus, m.cal_scroll);
    // A focused calendar control out of sight: scroll to it.
    let area = painted.cal_area;
    let Some((r, _)) = focus.filter(|f| in_calendar(f)).and_then(|f| painted.stops.iter().find(|(_, h)| h == f)) else { return painted };
    let scroll = if r.y < area.y {
        painted.cal_scroll - (area.y - r.y) - 8
    } else if r.y + r.h > area.y + area.h {
        painted.cal_scroll + (r.y + r.h - area.y - area.h) + 8
    } else {
        return painted;
    };
    paint_list_at(canvas, fonts, (w, h), m, focus, scroll)
}

fn paint_list_at(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (w, h): (usize, usize), m: &ListModel, focus: Option<&ListHit>, cal_scroll: i32) -> ListPainted {
    canvas.fill(0x00000000);
    let mut hits: Vec<(Rect, ListHit)> = Vec::new();
    let total_w = list_w() + super::calendar::cal_w();
    let bx = (w as i32 - total_w) / 2;
    let by = panel_height() as i32 + 4;
    let max_h = h as i32 - by - 8;
    // Entries that fit, from the scroll position.
    let header = helix_draw::theme::grow(44);
    let mut used = header;
    let mut shown = Vec::new();
    for ((row, eh), icon) in m.rows.iter().zip(m.heights).skip(m.scroll).zip(m.icons) {
        if used + eh > max_h && !shown.is_empty() {
            break;
        }
        used += eh;
        shown.push((row, *eh, icon.as_ref()));
    }
    let empty_h = if m.entries.is_empty() { helix_draw::theme::grow(60) } else { 0 };
    // No taller than the screen: the calendar column scrolls instead.
    let bh = (used + empty_h).max(m.cal_h).min(max_h);
    let cal_scroll = cal_scroll.clamp(0, (m.cal_h - bh).max(0));
    let cal_area = Rect { x: bx + list_w(), y: by, w: super::calendar::cal_w(), h: bh };
    theme::popover(canvas, bx, by, total_w, bh);
    theme::vseparator(canvas, bx + list_w(), by + LIST_PAD, bh - 2 * LIST_PAD);
    let mut calendar_hits = Vec::new();
    canvas.clip(cal_area.x, cal_area.y, cal_area.w, cal_area.h);
    super::calendar::paint_calendar(m.calendar, (text, bold), canvas, (bx + list_w(), by - cal_scroll), m.media_icons, &mut calendar_hits, &|hit| m.look(hit));
    canvas.unclip();
    if m.cal_h > bh {
        // Where the column is, when it scrolls.
        let thumb = (bh * bh / m.cal_h).max(16);
        let at = (bh - thumb) * cal_scroll / (m.cal_h - bh).max(1);
        canvas.rect(cal_area.x + cal_area.w - 4, by + at, 3, thumb, theme::TEXT_DIM);
    }
    // Header: Do Not Disturb switch and Clear.
    let switch = Rect { x: bx + LIST_PAD, y: by + 10, w: theme::SWITCH_W as i32, h: theme::SWITCH_H as i32 };
    theme::switch(canvas, f64::from(switch.x), f64::from(switch.y), m.do_not_disturb, m.look(&ListHit::DoNotDisturb));
    label(text, canvas, tr("Do Not Disturb"), (switch.x + switch.w + 10) as f32, baseline(text, switch.y, switch.h, 14.0), 14.0, FG, helix_draw::theme::grow(160) as f32);
    hits.push((Rect { x: switch.x, y: switch.y, w: switch.w + 10 + text.map_or(130, |t| t.width(tr("Do Not Disturb"), 14.0) as i32), h: switch.h }, ListHit::DoNotDisturb));
    if !m.entries.is_empty() {
        let clear_w = helix_draw::theme::grow(64);
        let clear = Rect { x: bx + list_w() - LIST_PAD - clear_w, y: by + 8, w: clear_w, h: helix_draw::theme::grow(26) };
        theme::button(canvas, text, (clear.x, clear.y, clear.w, clear.h), tr("Clear"), theme::Kind::Normal, m.look(&ListHit::Clear), false);
        hits.push((clear, ListHit::Clear));
    }
    // Tab order: the header's controls, then each row followed by its
    // buttons (which come before it in `hits`, being on top).
    let mut stops: Vec<(Rect, ListHit)> = hits.clone();
    let mut y = by + header;
    if m.entries.is_empty() {
        let message = tr("No Notifications");
        let mw = text.map_or(0.0, |t| t.width(message, 15.0));
        label(text, canvas, message, bx as f32 + (list_w() as f32 - mw) / 2.0, (y + 30) as f32, 15.0, DIM_FG, list_w() as f32);
    }
    // Entries of a group are indented under its header.
    let indents = row_indents(m.entries, m.rows);
    for (n, (row, eh, icon)) in shown.into_iter().enumerate() {
        let indent = indents.get(m.scroll + n).copied().unwrap_or(0);
        let entry = match row {
            Row::Header { key, app, count, open } => {
                let row_r = Rect { x: bx, y, w: list_w(), h: eh };
                theme::separator(canvas, bx + LIST_PAD, y, list_w() - 2 * LIST_PAD);
                card(canvas, row_r, m.look(&ListHit::GroupToggle(key.clone())));
                let head = app.clone();
                let tail = trf!("{count} notifications", count = count);
                // A chevron: down when open, right when closed.
                let towards = if *open { theme::Towards::Down } else { theme::Towards::Right };
                theme::chevron(canvas, f64::from(bx + LIST_PAD + 8), f64::from(y + 15), 8.0, towards, DIM_FG);
                let hb = bold.or(text);
                label(hb, canvas, &head, (bx + LIST_PAD + 20) as f32, (y + 20) as f32, 14.0, FG, 200.0);
                let hw = hb.map_or(0.0, |t| t.width(&head, 14.0).min(200.0));
                label(text, canvas, &tail, bx as f32 + (LIST_PAD + 32) as f32 + hw, (y + 20) as f32, 13.0, DIM_FG, 140.0);
                let close = Rect { x: bx + list_w() - LIST_PAD - 22, y: y + 4, w: 22, h: 22 };
                label(text, canvas, "×", (close.x + 5) as f32, (close.y + 17) as f32, 18.0, DIM_FG, 20.0);
                hits.push((close, ListHit::GroupClear(key.clone())));
                hits.push((row_r, ListHit::GroupToggle(key.clone())));
                stops.push((row_r, ListHit::GroupToggle(key.clone())));
                stops.push((close, ListHit::GroupClear(key.clone())));
                y += eh;
                continue;
            }
            Row::Entry(i) => &m.entries[*i],
        };
        let grouped = indent > 0;
        if !grouped {
            theme::separator(canvas, bx + LIST_PAD, y, list_w() - 2 * LIST_PAD);
        }
        card(canvas, Rect { x: bx, y, w: list_w(), h: eh }, m.look(&ListHit::Entry(entry.id)));
        if grouped {
            canvas.round_rect(f64::from(bx + LIST_PAD + 4), f64::from(y + 4), 2.0, f64::from(eh - 8), 1.0, if helix_draw::high_contrast() { 0xFFFF_FFFF } else { theme::BORDER });
        }
        let (links, _) = paint_notification(canvas, (text, bold), icon, entry, (bx + LIST_PAD + indent, y + LIST_PAD), entry_w(indent), 2);
        let close = Rect { x: bx + list_w() - LIST_PAD - 22, y: y + LIST_PAD - 2, w: 22, h: 22 };
        label(text, canvas, "×", (close.x + 5) as f32, (close.y + 17) as f32, 18.0, DIM_FG, 20.0);
        let row_r = Rect { x: bx, y, w: list_w(), h: eh };
        hits.push((close, ListHit::Dismiss(entry.id)));
        stops.push((row_r, ListHit::Entry(entry.id)));
        stops.push((close, ListHit::Dismiss(entry.id)));
        for (rect, uri) in links {
            hits.push((rect, ListHit::Link(uri.clone())));
            stops.push((rect, ListHit::Link(uri)));
        }
        hits.push((row_r, ListHit::Entry(entry.id)));
        y += eh;
    }
    // The calendar column: month buttons, the selected day, media.
    let selected = m.calendar.selected;
    for (r, hit) in &calendar_hits {
        if !matches!(hit, ListHit::Day(..)) || *hit == ListHit::Day(selected.0, selected.1, selected.2) {
            stops.push((*r, hit.clone()));
        }
    }
    // Clicks only on what shows.
    hits.extend(calendar_hits.into_iter().filter(|(r, _)| r.y >= cal_area.y && r.y + r.h <= cal_area.y + cal_area.h));
    // The focus ring: a circle around a day, a box around anything else.
    if let Some((r, hit)) = focus.and_then(|f| stops.iter().find(|(_, h)| h == f)) {
        if matches!(hit, ListHit::Day(..)) {
            canvas.ring(f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(r.y) + f64::from(r.h) / 2.0, 16.5, 2.0, helix_draw::theme::focus());
        } else if matches!(hit, ListHit::Entry(_) | ListHit::GroupToggle(_)) {
            let c = card_rect(*r);
            theme::focus_ring_round(canvas, f64::from(c.x), f64::from(c.y), f64::from(c.w), f64::from(c.h), theme::RADIUS_ROW);
        } else if matches!(hit, ListHit::DoNotDisturb) {
            theme::focus_ring_round(canvas, f64::from(r.x), f64::from(r.y), theme::SWITCH_W, theme::SWITCH_H, theme::SWITCH_H / 2.0);
        } else if matches!(hit, ListHit::Media(_) | ListHit::Month(_) | ListHit::Dismiss(_) | ListHit::GroupClear(_)) {
            theme::focus_ring_round(canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), f64::from(r.w.min(r.h)) / 2.0);
        } else {
            theme::focus_ring_round(canvas, f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h), theme::RADIUS_BUTTON);
        }
    }
    hits.push((Rect { x: bx, y: by, w: total_w, h: bh }, ListHit::Inside));
    hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, ListHit::Outside));
    ListPainted { hits, stops, cal_scroll, cal_area }
}

/// Where a notification row's highlight goes: inside the list's
/// padding, clear of its separators.
fn card_rect(row: Rect) -> Rect {
    Rect { x: row.x + LIST_PAD / 2, y: row.y + 2, w: row.w - LIST_PAD, h: row.h - 3 }
}

/// A notification row's highlight under the pointer: a rounded card in
/// [`theme::CARD_HOVER`] (its secondary text stays readable on it).
fn card(canvas: &mut Canvas, row: Rect, look: theme::Look) {
    if !(look.hovered || look.pressed) {
        return;
    }
    let c = card_rect(row);
    let rect = (f64::from(c.x), f64::from(c.y), f64::from(c.w), f64::from(c.h));
    if helix_draw::high_contrast() {
        theme::inner_outline(canvas, rect, theme::RADIUS_ROW, if look.pressed { 2.0 } else { 1.0 }, 0xFFFF_FFFF);
    } else {
        canvas.round_rect(rect.0, rect.1, rect.2, rect.3, theme::RADIUS_ROW, theme::CARD_HOVER);
    }
}

/// Body lines a banner shows: 2, or 8 for apps with `force-expanded`.
fn banner_lines(banner: &Banner) -> usize {
    if banner.expanded {
        8
    } else {
        2
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::shell_keys::Playback;

    /// Unix seconds `secs` ago.
    fn arrived_ago(secs: i64) -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) - secs
    }

    pub(in crate::ui) fn note(id: u32, app: &str, app_id: &str) -> Banner {
        Banner { id, app_name: app.into(), app_id: app_id.into(), summary: String::new(), body: String::new(), buttons: Vec::new(), icon: None, markup: false, expanded: false, arrived: arrived_ago(i64::from(id) * 420) }
    }

    /// A list to look at: a group of chat messages, mail, an update.
    pub(in crate::ui) fn sample() -> Vec<Banner> {
        let with = |id, app: &str, app_id: &str, summary: &str, body: &str| Banner { summary: summary.into(), body: body.into(), ..note(id, app, app_id) };
        vec![
            with(6, "Chat", "org.example.Chat", "Alice", "Are we still on for lunch? I booked the place on 5th street for 12:30."),
            with(5, "Mail", "", "Invoice 2041", "Your invoice is ready to download."),
            with(4, "Chat", "org.example.Chat", "Bob", "sent a photo"),
            with(3, "Software", "", "Updates available", "3 updates are ready to install."),
        ]
    }

    /// The calendar with an event today and a player.
    pub(in crate::ui) fn sample_calendar(player: bool) -> super::super::calendar::CalendarState {
        let mut cal = super::super::calendar::CalendarState::default();
        let now = gio::glib::DateTime::now_local().unwrap().to_unix();
        let events = vec![crate::ical::Occurrence { start: now - now.rem_euclid(3600), end: now + 1800, all_day: false, summary: "Standup".into() }];
        let player = player.then(|| crate::calendar::Player { identity: "Music".into(), title: "Clair de Lune".into(), artist: "Claude Debussy".into(), ..Default::default() });
        cal.data = Some(crate::calendar::CalendarData { events, player, clocks: vec![("Tokyo".into(), "Asia/Tokyo".into())], ..Default::default() });
        cal
    }

    /// Paint `entries` (as ListView would) on a `w`×`h` surface.
    pub(in crate::ui) fn paint_sample(canvas: &mut Canvas, fonts: (Option<&Text>, Option<&Text>), (w, h): (usize, usize), entries: &[Banner], expanded: &HashSet<String>, cal: &super::super::calendar::CalendarState, focus: Option<&ListHit>) -> ListPainted {
        let rows = list_rows(entries, expanded);
        let f = fonts.0.map(|regular| Fonts { regular, bold: fonts.1 });
        let indents = row_indents(entries, &rows);
        let heights: Vec<i32> = rows.iter().zip(&indents).map(|(row, indent)| match row { Row::Header { .. } => group_header(), Row::Entry(i) => entry_height(f.as_ref(), &entries[*i], *indent) }).collect();
        let icons = vec![None; rows.len()];
        let media_icons: super::super::calendar::MediaIcons = [None, None, None, None];
        let cal_h = super::super::calendar::height(cal);
        let model = ListModel { entries, rows: &rows, heights: &heights, icons: &icons, scroll: 0, do_not_disturb: false, calendar: cal, media_icons: &media_icons, cal_h, cal_scroll: 0, hover: None, pressed: None };
        paint_list(canvas, fonts, (w, h), &model, focus)
    }

    /// On a short screen the calendar column scrolls: nothing is drawn
    /// below the edge and its media buttons can be reached.
    #[test]
    fn short_screens() {
        let entries = sample();
        let cal = sample_calendar(true);
        let (w, h) = (853, 480);
        let mut data = vec![0u8; w * h * 4];
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
        let painted = paint_sample(&mut canvas, (None, None), (w, h), &entries, &HashSet::new(), &cal, None);
        let area = painted.cal_area;
        assert!(area.y + area.h <= h as i32 - 8, "{area:?}");
        for (r, hit) in &painted.hits {
            assert!(*hit == ListHit::Outside || r.y + r.h <= h as i32 - 8, "{hit:?} at {r:?}");
        }
        assert!(!painted.hits.iter().any(|(_, hit)| matches!(hit, ListHit::Media(_))), "the media buttons start out of sight");
        let mut canvas = Canvas::scaled(&mut data, w, h, 1.0);
        let play = ListHit::Media(Playback::PlayPause);
        let painted = paint_sample(&mut canvas, (None, None), (w, h), &entries, &HashSet::new(), &cal, Some(&play));
        assert!(painted.cal_scroll > 0);
        assert!(painted.hits.iter().any(|(_, hit)| *hit == play), "focusing them scrolls them into sight");
    }

    /// A grouped notification is measured at the width it is painted at:
    /// a body whose width lies between the grouped and ungrouped widths
    /// wraps the same in both, and nothing is painted below the row.
    #[test]
    fn grouped_rows_fit() {
        let Some(regular) = Text::load() else { return };
        let fonts = Fonts { regular: &regular, bold: None };
        let (narrow, wide) = ((entry_w(GROUP_INDENT) - ICON - 12) as f32, (entry_w(0) - ICON - 12) as f32);
        // A body just wider than the indented text: one line at the full
        // width, two indented.
        // Grown a character at a time so it works with whichever font the
        // machine has (a word at a time can jump past `wide`).
        let text = "Are we still on for lunch? I booked the place on 5th street for 12:30. OK see you there then, and bring the slides from the review so we can go over them";
        let mut body = String::new();
        for c in text.chars() {
            body.push(c);
            if c != ' ' && regular.width(&body, BODY_PX) > narrow {
                break;
            }
        }
        let width = regular.width(&body, BODY_PX);
        assert!(width > narrow && width <= wide, "{width} between {narrow} and {wide}");
        let entries = vec![
            Banner { summary: "Alice".into(), body: body.clone(), ..note(2, "Chat", "org.example.Chat") },
            Banner { summary: "Bob".into(), body, ..note(1, "Chat", "org.example.Chat") },
        ];
        let expanded: HashSet<String> = ["app:org.example.Chat".to_owned()].into();
        let rows = list_rows(&entries, &expanded);
        let indents = row_indents(&entries, &rows);
        assert_eq!(indents, [0, GROUP_INDENT, GROUP_INDENT]);
        for (row, indent) in rows.iter().zip(&indents) {
            let Row::Entry(i) = row else { continue };
            let measured = entry_height(Some(&fonts), &entries[*i], *indent);
            let (w, h) = (entry_w(*indent) + 40, 400);
            let mut data = vec![0u8; w as usize * h * 4];
            let mut canvas = Canvas::scaled(&mut data, w as usize, h, 1.0);
            let (_, bottom) = paint_notification(&mut canvas, (Some(&regular), None), None, &entries[*i], (0, LIST_PAD), entry_w(*indent), 2);
            assert!(bottom + LIST_PAD <= measured, "painted to {bottom}, measured {measured}");
            assert_eq!(measured, entry_height(Some(&fonts), &entries[*i], GROUP_INDENT), "two body lines measured");
        }
    }

    #[test]
    fn keyboard() {
        let entries = sample();
        let cal = sample_calendar(true);
        let expanded: HashSet<String> = ["app:org.example.Chat".to_owned()].into();
        let mut data = vec![0u8; 1280 * 900 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 900, 1.0);
        let painted = paint_sample(&mut canvas, (None, None), (1280, 900), &entries, &expanded, &cal, None);
        let rows = list_rows(&entries, &expanded);
        let targets = row_targets(&entries, &rows);
        let group = ListHit::GroupToggle("app:org.example.Chat".into());
        assert_eq!(targets, [group.clone(), ListHit::Entry(6), ListHit::Entry(4), ListHit::Entry(5), ListHit::Entry(3)]);
        let order: Vec<ListHit> = painted.stops.iter().map(|(_, h)| h.clone()).collect();
        assert_eq!(order[..5], [ListHit::DoNotDisturb, ListHit::Clear, group.clone(), ListHit::GroupClear("app:org.example.Chat".into()), ListHit::Entry(6)]);
        assert_eq!(order[5], ListHit::Dismiss(6), "a row's buttons follow it");
        let (y, m, d) = cal.selected;
        assert!(order.contains(&ListHit::Day(y, m, d)), "the selected day");
        assert_eq!(order.iter().filter(|h| matches!(h, ListHit::Day(..))).count(), 1, "one stop for the days");
        assert!(order.contains(&ListHit::Media(Playback::PlayPause)));
        let key = |k: Keysym, focus: &ListHit| list_key_action(k, false, &painted.stops, Some(focus), &targets, &entries);
        // Opened by a click: the first move focuses the first row.
        assert_eq!(list_key_action(Keysym::Down, false, &painted.stops, None, &targets, &entries), ListKey::Focus(group.clone()));
        // Rows with Up and Down; Do Not Disturb above them.
        assert_eq!(key(Keysym::Down, &group), ListKey::Focus(ListHit::Entry(6)));
        assert_eq!(key(Keysym::Down, &ListHit::Dismiss(6)), ListKey::Focus(ListHit::Entry(4)), "from a row's button");
        assert_eq!(key(Keysym::Up, &group), ListKey::Focus(ListHit::DoNotDisturb));
        assert_eq!(key(Keysym::Right, &ListHit::DoNotDisturb), ListKey::Focus(ListHit::Clear));
        // Enter activates, Delete dismisses, Left/Right close/open groups.
        assert_eq!(key(Keysym::Return, &ListHit::Entry(5)), ListKey::Activate(ListHit::Entry(5)));
        assert_eq!(key(Keysym::Delete, &ListHit::Entry(5)), ListKey::Activate(ListHit::Dismiss(5)));
        assert_eq!(key(Keysym::Delete, &group), ListKey::Activate(ListHit::GroupClear("app:org.example.Chat".into())));
        assert_eq!(key(Keysym::Left, &group), ListKey::Expand("app:org.example.Chat".into(), false));
        assert_eq!(key(Keysym::Right, &group), ListKey::Expand("app:org.example.Chat".into(), true));
        assert_eq!(key(Keysym::Left, &ListHit::Entry(4)), ListKey::Expand("app:org.example.Chat".into(), false), "back to the group");
        // Tab goes on through the calendar; Ctrl+Tab jumps there.
        assert_eq!(key(Keysym::Tab, &ListHit::DoNotDisturb), ListKey::Focus(ListHit::Clear));
        assert_eq!(list_key_action(Keysym::Tab, true, &painted.stops, Some(&ListHit::Entry(6)), &targets, &entries), ListKey::Focus(ListHit::Month(-1)));
        assert_eq!(list_key_action(Keysym::Tab, true, &painted.stops, Some(&ListHit::Month(1)), &targets, &entries), ListKey::Focus(ListHit::DoNotDisturb));
        // The calendar.
        let day = ListHit::Day(y, m, d);
        assert_eq!(key(Keysym::Right, &day), ListKey::Day(1));
        assert_eq!(key(Keysym::Up, &day), ListKey::Day(-7));
        assert_eq!(key(Keysym::Page_Down, &day), ListKey::Month(1));
        assert_eq!(key(Keysym::Home, &day), ListKey::Today);
        assert_eq!(key(Keysym::Right, &ListHit::Month(-1)), ListKey::Focus(ListHit::Month(1)));
        assert_eq!(key(Keysym::Down, &ListHit::Month(1)), ListKey::Focus(day.clone()));
        assert_eq!(key(Keysym::Up, &ListHit::Media(Playback::Next)), ListKey::Focus(day));
        assert_eq!(key(Keysym::space, &ListHit::Media(Playback::PlayPause)), ListKey::Activate(ListHit::Media(Playback::PlayPause)));
        assert_eq!(key(Keysym::Escape, &ListHit::Clear), ListKey::Close);
    }

    #[test]
    fn groups() {
        // Newest first.
        let entries = vec![note(6, "Chat", "org.example.Chat"), note(5, "Mail", ""), note(4, "Chat", "org.example.Chat"), note(3, "Updates", ""), note(2, "Mail", ""), note(1, "Chat", "org.example.Chat")];
        let header = |key: &str, app: &str, count, open| Row::Header { key: key.into(), app: app.into(), count, open };
        let closed = list_rows(&entries, &HashSet::new());
        assert_eq!(closed, [header("app:org.example.Chat", "Chat", 3, false), Row::Entry(0), header("name:Mail", "Mail", 2, false), Row::Entry(1), Row::Entry(3)]);
        let open = list_rows(&entries, &["app:org.example.Chat".to_owned()].into());
        assert_eq!(open[..4], [header("app:org.example.Chat", "Chat", 3, true), Row::Entry(0), Row::Entry(2), Row::Entry(5)]);
        assert_eq!(open.len(), 7);
    }
}

