//! Notification banners and the notification list (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.Notifications.md`, "Helix"): icons,
//! body markup with links, and the list opened from the panel's clock.

use helix_draw::{draw_lines, wrap_spans, Fonts, Image, Span};

use super::*;
use crate::icons::IconSource;
use crate::notifications::{Banner, List};

/// Icon size in the banner and the list (logical pixels).
pub(super) const ICON: i32 = 32;
/// Icons are decoded at this many pixels, sharp up to scale 2.
const ICON_PX: u32 = 64;
const LIST_W: i32 = 420;
const LIST_PAD: i32 = 10;
/// The clock's clickable area in the panel, either side of the middle.
pub(super) const CLOCK_HALF_W: i32 = 70;
const BODY_PX: f32 = 14.0;
const BODY_LINE: i32 = 19;

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
enum ListHit {
    Entry(u32),
    Dismiss(u32),
    Link(String),
    Clear,
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
    /// Entries scrolled past.
    scroll: usize,
}

/// Notification state the UI keeps.
#[derive(Default)]
pub(super) struct NotifyState {
    pub list: List,
    pub do_not_disturb: bool,
    pub view: Option<ListView>,
    /// Decoded icons by notification id (and where they came from).
    icons: HashMap<u32, (Option<IconSource>, Option<Image>)>,
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
fn paint_notification(
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
        None => canvas.circle(f64::from(x + ICON / 2), f64::from(y + 4 + ICON / 2), f64::from(ICON) / 2.0 - 2.0, BANNER_BORDER),
    }
    label(text, canvas, &banner.app_name, text_x as f32, (y + 12) as f32, 12.0, DIM_FG, text_w - 24.0);
    let mut bottom = y + 16;
    match bold {
        Some(bold) => bold.draw(canvas, &banner.summary, text_x as f32, (bottom + 20) as f32, Style { px: 16.0, color: FG, max_width: text_w }),
        None => label(text, canvas, &banner.summary, text_x as f32, (bottom + 20) as f32, 16.0, FG, text_w),
    }
    bottom += 26;
    let mut links = Vec::new();
    if let (Some(regular), false) = (text, banner.body.is_empty()) {
        let fonts = Fonts { regular, bold };
        let spans = spans(banner);
        let lines = wrap_spans(&fonts, &spans, BODY_PX, text_w, max_lines);
        let areas = draw_lines(canvas, &fonts, &spans, &lines, (text_x as f32, (bottom + 14) as f32), BODY_PX, BODY_LINE as f32, (DIM_FG, ACCENT));
        for ((lx, ly, lw, lh), uri) in areas {
            links.push((Rect { x: lx as i32, y: ly as i32, w: lw.ceil() as i32, h: lh as i32 }, uri));
        }
        bottom += lines.len() as i32 * BODY_LINE;
    }
    (links, bottom.max(y + ICON + 4))
}

impl Ui {
    fn fonts(&self) -> Option<Fonts<'_>> {
        Some(Fonts { regular: self.text.as_ref()?, bold: self.bold.as_ref() })
    }

    /// The icon of notification `id`, decoded once.
    fn notification_icon(&mut self, id: u32, source: &Option<IconSource>) -> Option<Image> {
        if let Some((cached_source, image)) = self.notify.icons.get(&id) {
            if cached_source == source {
                return image.clone();
            }
        }
        let image = source.as_ref().and_then(|s| crate::icons::load(s, ICON_PX));
        self.notify.icons.insert(id, (source.clone(), image.clone()));
        image
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
        let text_w = (BANNER_W as i32 - 2 * BANNER_PAD - ICON - 12) as f32;
        let body = self.body_lines(banner, text_w, 2) as i32;
        let buttons = if banner.buttons.is_empty() { 0 } else { 44 };
        let content = (16 + 26 + body * BODY_LINE).max(ICON + 4);
        (BANNER_PAD + content + buttons + BANNER_PAD) as u32
    }

    pub(super) fn paint_banner(&mut self, view: &mut BannerView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let icon = self.notification_icon(view.data.id, &view.data.icon);
        let scale = self.scales.of(view.layer.wl_surface());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate banner buffer");
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        // Lighter than the desktop, with a thin border, so it stands out.
        canvas.fill(if view.focused { ACCENT } else { BANNER_BORDER });
        let edge = if view.focused { 2 } else { 1 };
        canvas.rect(edge, edge, w as i32 - 2 * edge, h as i32 - 2 * edge, BANNER_BG);
        view.hits.clear();
        let inner = w as i32 - 2 * BANNER_PAD;
        // Close button, top right.
        let close = Rect { x: w as i32 - BANNER_PAD - 18, y: BANNER_PAD - 4, w: 22, h: 22 };
        let text = self.text.as_ref();
        let (links, mut y) =
            paint_notification(&mut canvas, (text, self.bold.as_ref()), icon.as_ref(), &view.data, (BANNER_PAD, BANNER_PAD), inner - 24, 2);
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
                canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                let lw = text.map_or(0.0, |t| t.width(button_label, 14.0)).min((bw - 12) as f32);
                let base = baseline(text, r.y, r.h, 14.0);
                label(text, &mut canvas, button_label, r.x as f32 + (bw as f32 - lw) / 2.0, base, 14.0, FG, (bw - 12) as f32);
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

    /// Give the banner keyboard focus (or take it away).
    pub(super) fn focus_banner(&mut self, focus: bool) {
        let Some(view) = self.banner.as_mut() else { return };
        if view.focused == focus {
            return;
        }
        view.focused = focus;
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
        self.notify.icons.retain(|id, _| list.entries.iter().any(|e| e.id == *id));
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
    }

    /// Whether the panel shows the unseen-notifications dot.
    pub(super) fn unseen_dot(&self) -> bool {
        self.notify.list.unseen && !self.notify.list.entries.is_empty() && self.notify.view.is_none()
    }

    pub(super) fn toggle_notification_list(&mut self) {
        if self.notify.view.take().is_some() {
            self.draw_panel();
            return;
        }
        self.close_launcher(true);
        self.power_menu = None;
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-notification-list"), None);
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_size(0, 0);
        layer.commit();
        self.notify.view = Some(ListView { layer, width: 0, height: 0, configured: false, hits: Vec::new(), scroll: 0 });
        let _ = self.requests.send(UiRequest::NotificationListOpened);
        self.draw_panel();
    }

    pub(super) fn close_notification_list(&mut self) {
        if self.notify.view.take().is_some() {
            self.draw_panel();
        }
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

    /// Height of one list entry.
    fn entry_height(&self, banner: &Banner) -> i32 {
        let text_w = (LIST_W - 2 * LIST_PAD - ICON - 12 - 24) as f32;
        let body = self.body_lines(banner, text_w, 2) as i32;
        (16 + 26 + body * BODY_LINE).max(ICON + 4) + 2 * LIST_PAD
    }

    pub(super) fn draw_notification_list(&mut self) {
        let Some(mut view) = self.notify.view.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_notification_list(&mut view);
        }
        self.notify.view = Some(view);
    }

    fn paint_notification_list(&mut self, view: &mut ListView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let entries = self.notify.list.entries.clone();
        view.scroll = view.scroll.min(entries.len().saturating_sub(1));
        let heights: Vec<i32> = entries.iter().map(|e| self.entry_height(e)).collect();
        let icons: Vec<Option<Image>> = entries.iter().skip(view.scroll).map(|e| self.notification_icon(e.id, &e.icon)).collect();
        let do_not_disturb = self.notify.do_not_disturb;
        let scale = self.scales.of(view.layer.wl_surface());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        view.hits.clear();
        let bx = (w as i32 - LIST_W) / 2;
        let by = PANEL_HEIGHT as i32 + 4;
        let max_h = h as i32 - by - 8;
        // Entries that fit, from the scroll position.
        let header = 44;
        let mut used = header;
        let mut shown = Vec::new();
        for ((entry, eh), icon) in entries.iter().zip(&heights).skip(view.scroll).zip(&icons) {
            if used + eh > max_h && !shown.is_empty() {
                break;
            }
            used += eh;
            shown.push((entry, *eh, icon.as_ref()));
        }
        let empty_h = if entries.is_empty() { 60 } else { 0 };
        let bh = used + empty_h;
        canvas.rect(bx - 1, by - 1, LIST_W + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, LIST_W, bh, BANNER_BG);
        let text = self.text.as_ref();
        // Header: Do Not Disturb switch and Clear.
        let switch = Rect { x: bx + LIST_PAD, y: by + 10, w: 40, h: 22 };
        let on = do_not_disturb;
        let track = if on { ACCENT } else { TRACK };
        canvas.circle(f64::from(switch.x + 11), f64::from(switch.y + 11), 11.0, track);
        canvas.circle(f64::from(switch.x + switch.w - 11), f64::from(switch.y + 11), 11.0, track);
        canvas.rect(switch.x + 11, switch.y, switch.w - 22, switch.h, track);
        let knob = if on { switch.x + switch.w - 11 } else { switch.x + 11 };
        canvas.circle(f64::from(knob), f64::from(switch.y + 11), 9.0, FG);
        label(text, &mut canvas, "Do Not Disturb", (switch.x + switch.w + 10) as f32, (switch.y + 16) as f32, 14.0, FG, 160.0);
        view.hits.push((Rect { x: switch.x, y: switch.y, w: switch.w + 140, h: switch.h }, ListHit::DoNotDisturb));
        if !entries.is_empty() {
            let clear = Rect { x: bx + LIST_W - LIST_PAD - 64, y: by + 8, w: 64, h: 26 };
            canvas.rect(clear.x, clear.y, clear.w, clear.h, BANNER_BORDER);
            let cw = text.map_or(0.0, |t| t.width("Clear", 14.0));
            label(text, &mut canvas, "Clear", clear.x as f32 + (clear.w as f32 - cw) / 2.0, baseline(text, clear.y, clear.h, 14.0), 14.0, FG, 60.0);
            view.hits.push((clear, ListHit::Clear));
        }
        let mut y = by + header;
        if entries.is_empty() {
            let message = "No Notifications";
            let mw = text.map_or(0.0, |t| t.width(message, 15.0));
            label(text, &mut canvas, message, bx as f32 + (LIST_W as f32 - mw) / 2.0, (y + 30) as f32, 15.0, DIM_FG, LIST_W as f32);
        }
        for (entry, eh, icon) in shown {
            canvas.rect(bx + LIST_PAD, y, LIST_W - 2 * LIST_PAD, 1, BANNER_BORDER);
            let (links, _) = paint_notification(
                &mut canvas,
                (text, self.bold.as_ref()),
                icon,
                entry,
                (bx + LIST_PAD, y + LIST_PAD),
                LIST_W - 2 * LIST_PAD - 24,
                2,
            );
            let close = Rect { x: bx + LIST_W - LIST_PAD - 22, y: y + LIST_PAD - 2, w: 22, h: 22 };
            label(text, &mut canvas, "×", (close.x + 5) as f32, (close.y + 17) as f32, 18.0, DIM_FG, 20.0);
            view.hits.push((close, ListHit::Dismiss(entry.id)));
            for (rect, uri) in links {
                view.hits.push((rect, ListHit::Link(uri)));
            }
            view.hits.push((Rect { x: bx, y, w: LIST_W, h: eh }, ListHit::Entry(entry.id)));
            y += eh;
        }
        view.hits.push((Rect { x: bx, y: by, w: LIST_W, h: bh }, ListHit::Inside));
        view.hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, ListHit::Outside));
        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    /// Pointer input on the list; true if it was for it.
    pub(super) fn notification_list_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(view) = self.notify.view.as_mut().filter(|v| v.layer.wl_surface() == &event.surface) else { return false };
        match event.kind {
            PointerEventKind::Axis { vertical, .. } if vertical.absolute != 0.0 || vertical.discrete != 0 => {
                let down = vertical.absolute > 0.0 || vertical.discrete > 0;
                let count = self.notify.list.entries.len();
                view.scroll = if down { (view.scroll + 1).min(count.saturating_sub(1)) } else { view.scroll.saturating_sub(1) };
                self.draw_notification_list();
            }
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                let (x, y) = event.position;
                let hit = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone());
                match hit {
                    Some(ListHit::Entry(id)) => {
                        let _ = self.requests.send(UiRequest::NotificationActivated { id, action: None });
                        self.close_notification_list();
                    }
                    Some(ListHit::Dismiss(id)) => {
                        let _ = self.requests.send(UiRequest::NotificationDismissed { id });
                    }
                    Some(ListHit::Link(uri)) => {
                        let _ = self.requests.send(UiRequest::OpenUri(uri));
                        self.close_notification_list();
                    }
                    Some(ListHit::Clear) => {
                        let _ = self.requests.send(UiRequest::NotificationsCleared);
                    }
                    Some(ListHit::DoNotDisturb) => {
                        let on = !self.notify.do_not_disturb;
                        self.notify.do_not_disturb = on;
                        let _ = self.requests.send(UiRequest::SetDoNotDisturb(on));
                        self.draw_notification_list();
                    }
                    Some(ListHit::Inside) => {}
                    Some(ListHit::Outside) | None => self.close_notification_list(),
                }
            }
            _ => {}
        }
        true
    }
}
