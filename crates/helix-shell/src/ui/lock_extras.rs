//! What the lock screen shows besides the password (helix-desktop-spec
//! `contracts/dbus/org.gnome.ScreenSaver.md`, "Lock screen"): the media
//! player and counts of notifications that arrived while locked.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use helix_draw::Image;

use super::lock::LockHit;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind};
use super::*;
use crate::calendar::Player;
use crate::notifications::Banner;
use crate::shell_keys::Playback;

const MEDIA_EVERY: Duration = Duration::from_secs(5);
const MAX_APPS: usize = 5;

#[derive(Default)]
pub(super) struct LockExtras {
    pub media: Option<Player>,
    /// Album art by URL: `None` until read, then the picture if it could
    /// be had.
    pub art: HashMap<String, Option<Image>>,
    /// Notifications in the list when the screen locked.
    pub known: HashSet<u32>,
    pub asked: Option<Instant>,
}

/// Detail lines shown per app at most.
const MAX_DETAILS: usize = 2;

/// An app's notifications on the lock screen.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LockApp {
    pub name: String,
    /// Its desktop file id (for its icon).
    pub app_id: String,
    pub count: usize,
    /// The newest ones' (summary, body), when the app allows details.
    pub details: Vec<(String, String)>,
}

/// Per app, the notifications not in `known` (`entries` newest first),
/// for the apps `policy` (app id → (show, details)) lets show: most
/// first, then by name.
pub(super) fn lock_counts(entries: &[Banner], known: &HashSet<u32>, policy: impl Fn(&str) -> (bool, bool)) -> Vec<LockApp> {
    let mut apps: Vec<(LockApp, bool)> = Vec::new();
    let mut policies: Vec<(String, (bool, bool))> = Vec::new();
    for e in entries.iter().filter(|e| !known.contains(&e.id)) {
        let (show, details) = match policies.iter().find(|(id, _)| *id == e.app_id) {
            Some((_, p)) => *p,
            None => {
                let p = policy(&e.app_id);
                policies.push((e.app_id.clone(), p));
                p
            }
        };
        if !show {
            continue;
        }
        let i = match apps.iter().position(|(a, _)| a.name == e.app_name) {
            Some(i) => i,
            None => {
                apps.push((LockApp { name: e.app_name.clone(), app_id: e.app_id.clone(), count: 0, details: Vec::new() }, details));
                apps.len() - 1
            }
        };
        let (app, details) = &mut apps[i];
        app.count += 1;
        if *details && app.details.len() < MAX_DETAILS {
            let body = if e.markup { crate::markup::plain(&e.body) } else { e.body.clone() };
            app.details.push((e.summary.clone(), body.lines().next().unwrap_or("").to_owned()));
        }
    }
    let mut apps: Vec<LockApp> = apps.into_iter().map(|(a, _)| a).collect();
    apps.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    apps.truncate(MAX_APPS);
    apps
}

fn show_in_lock_screen() -> bool {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.notifications";
    !helix_settings::has_schema(SCHEMA) || gio::Settings::new(SCHEMA).boolean("show-in-lock-screen")
}

/// The lock screen's pictures: the media buttons' icons (previous, play
/// or pause, next), the media card's art (the track's, else the player's
/// icon), and the notification apps' icons (in the counts' order).
#[derive(Default)]
pub(super) struct LockIcons {
    pub media: [Option<Image>; 3],
    pub art: Option<Image>,
    pub art_is_cover: bool,
    pub apps: Vec<Option<Image>>,
    /// The bottom buttons' icons.
    pub restart: Option<Image>,
    pub power_off: Option<Image>,
    pub switch_user: Option<Image>,
}

impl Ui {
    /// The screen just locked.
    pub(super) fn lock_extras_start(&mut self) {
        self.lock_extras.known = self.notify.list.entries.iter().map(|e| e.id).collect();
        self.lock_extras.media = None;
        self.lock_extras.asked = None;
        self.lock_extras_tick();
    }

    /// Read the player again now and then while locked.
    pub(super) fn lock_extras_tick(&mut self) {
        if self.lock.session.is_none() || self.lock_extras.asked.is_some_and(|t| t.elapsed() < MEDIA_EVERY) {
            return;
        }
        self.lock_extras.asked = Some(Instant::now());
        let _ = self.requests.send(UiRequest::LockMedia(None));
    }

    pub(super) fn lock_media(&mut self, player: Option<Player>) {
        if self.lock_extras.media != player {
            // New art is read off the UI thread; the card shows the
            // player's icon until it comes (or if it cannot be had).
            let url = player.as_ref().map(|p| p.art_url.clone()).filter(|u| !u.is_empty());
            if let Some(url) = url.filter(|u| !self.lock_extras.art.contains_key(u)) {
                if self.lock_extras.art.len() > 16 {
                    self.lock_extras.art.clear();
                }
                self.lock_extras.art.insert(url.clone(), None);
                let _ = self.requests.send(UiRequest::LockArt(url));
            }
            self.lock_extras.media = player;
            self.draw_lock();
        }
    }

    /// Album art read (or not).
    pub(super) fn lock_art(&mut self, url: String, art: Option<Image>) {
        self.lock_extras.art.insert(url.clone(), art);
        if self.lock_extras.media.as_ref().is_some_and(|p| p.art_url == url) {
            self.draw_lock();
        }
    }

    pub(super) fn lock_icons(&mut self, scale: f64, counts: &[LockApp]) -> LockIcons {
        let apps = counts.iter().map(|a| self.app_icon_px(&a.app_id, 24.0, scale)).collect();
        let restart = self.symbolic_icon_px("system-reboot-symbolic", 20.0, scale);
        let power_off = self.symbolic_icon_px("system-shutdown-symbolic", 20.0, scale);
        let switch_user = self.symbolic_icon_px("system-switch-user-symbolic", 20.0, scale);
        let Some(player) = self.lock_extras.media.clone() else {
            return LockIcons { apps, restart, power_off, switch_user, ..Default::default() };
        };
        let play = if player.playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
        let media = [
            self.symbolic_icon_px("media-skip-backward-symbolic", 20.0, scale),
            self.symbolic_icon_px(play, 20.0, scale),
            self.symbolic_icon_px("media-skip-forward-symbolic", 20.0, scale),
        ];
        let cover = self.lock_extras.art.get(&player.art_url).cloned().flatten();
        let art_is_cover = cover.is_some();
        let art = cover.or_else(|| self.app_icon_px(&player.desktop_entry, 48.0, scale)).or_else(|| self.symbolic_icon_px("audio-x-generic-symbolic", 48.0, scale));
        LockIcons { media, art, art_is_cover, apps, restart, power_off, switch_user }
    }

    /// The counts to show (none unless allowed).
    pub(super) fn lock_note_counts(&self) -> Vec<LockApp> {
        if show_in_lock_screen() {
            lock_counts(&self.notify.list.entries, &self.lock_extras.known, crate::app_notify::lock_policy)
        } else {
            Vec::new()
        }
    }

    /// The pointer on a lock surface: hovering, pressing buttons.
    pub(super) fn lock_pointer(&mut self, event: &PointerEvent) {
        let (x, y) = event.position;
        if self.lock.exiting {
            return;
        }
        if let PointerEventKind::Press { .. } = event.kind {
            self.lock.last_input = Instant::now();
        }
        // A click on another monitor brings the prompt there (with what
        // was typed); not while asking about Restart or Power Off.
        if let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind {
            let origin = self.lock.surfaces.iter().find(|v| v.surface.wl_surface() == &event.surface).map(|v| v.origin);
            if origin.is_some() && origin != self.lock_prompt_origin() && self.lock.confirming.is_none() {
                self.lock.prompt_at = origin;
                self.lock.hover = None;
                self.draw_lock();
                return;
            }
        }
        let hit = self.lock.surfaces.iter().find(|v| v.surface.wl_surface() == &event.surface).and_then(|v| v.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h));
        let at = hit.map(|h| (event.surface.clone(), h));
        match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                if self.lock.hover != at {
                    self.lock.hover = at;
                    self.draw_lock();
                }
            }
            PointerEventKind::Leave { .. } => {
                if self.lock.hover.take().is_some() | self.lock.pressed.take().is_some() {
                    self.draw_lock();
                }
            }
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                self.lock.pressed = at;
                match hit {
                    Some(hit) => self.lock_activate(hit),
                    None => {
                        // Elsewhere: the keyboard goes back to the field.
                        if self.lock.confirming.is_none() && self.lock.focus.take().is_some() {
                            self.draw_lock();
                        }
                    }
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } if self.lock.pressed.take().is_some() => self.draw_lock(),
            _ => {}
        }
    }

    /// Move the keyboard focus to the next (or previous) lock screen
    /// button, the password field counting as one (not while asking).
    pub(super) fn lock_move_focus(&mut self, forward: bool) {
        let Some(view) = self.lock_primary() else { return };
        let mut order: Vec<Option<LockHit>> = view.hits.iter().map(|(_, h)| Some(*h)).filter(|h| !matches!(h, Some(LockHit::Unlock | LockHit::Reveal))).collect();
        if self.lock.confirming.is_none() {
            order.insert(0, None);
        }
        if order.is_empty() {
            return;
        }
        let at = order.iter().position(|h| *h == self.lock.focus);
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % order.len(),
            (Some(i), false) => (i + order.len() - 1) % order.len(),
            (None, true) => 0,
            (None, false) => order.len() - 1,
        };
        self.lock.focus = order[next];
    }

    /// Do what lock screen button `hit` does.
    pub(super) fn lock_activate(&mut self, hit: LockHit) {
        match hit {
            LockHit::Power(hit) => {
                match hit {
                    LockPowerHit::Ask(action) => {
                        self.lock.confirming = Some(action);
                        self.lock.blockers.clear();
                        // Cancel first: the safe answer.
                        self.lock.focus = Some(LockHit::Power(LockPowerHit::Cancel));
                        let _ = self.requests.send(UiRequest::LockBlockers);
                    }
                    LockPowerHit::Cancel => {
                        self.lock.confirming = None;
                        self.lock.focus = None;
                    }
                    LockPowerHit::Confirm => {
                        self.lock.focus = None;
                        if let Some(action) = self.lock.confirming.take() {
                            tracing::info!(?action, "power action from the lock screen");
                            let _ = self.requests.send(UiRequest::LockPower(action));
                        }
                    }
                }
                self.draw_lock();
            }
            LockHit::SwitchUser => {
                tracing::info!("switch user from the lock screen");
                let _ = self.requests.send(UiRequest::Power(crate::PowerAction::SwitchUser));
                self.draw_lock();
            }
            LockHit::Media(command) => {
                let _ = self.requests.send(UiRequest::LockMedia(Some(command)));
                self.draw_lock();
            }
            LockHit::Layout => {
                // The next input source (the least recently used: all of
                // them in turn).
                let sources = crate::input_sources::current();
                if let Some(next) = sources.last().filter(|_| sources.len() > 1) {
                    crate::input_sources::activate(next);
                    self.ime_sync();
                    self.refresh_source_label();
                }
                self.draw_lock();
            }
            LockHit::Reveal => self.lock_reveal(true),
            LockHit::Unlock => {
                self.lock.focus = None;
                if !self.lock.checking {
                    self.submit_password();
                }
                self.draw_lock();
            }
        }
    }
}

/// The extras' cards: dark, rounded, padded.
const CARD_FILL: u32 = 0x8C000000;
const CARD_RADIUS: f64 = 12.0;
const CARD_PAD: i32 = 12;
/// The media card's height, its art's size and its buttons'.
pub(super) const MEDIA_H: i32 = 108;
/// The media card without the artist, on short monitors.
pub(super) const MEDIA_COMPACT_H: i32 = 64;
const ART: i32 = 84;
const MEDIA_BUTTON: i32 = 40;
/// The notification card's rows: an app, a detail line.
pub(super) const APP_ROW: i32 = 36;
pub(super) const DETAIL_ROW: i32 = 20;
/// Between cards.
pub(super) const CARD_GAP: i32 = 12;

/// Secondary text on a card.
const CARD_DIM: u32 = 0xFFC4C4CC;

/// A card's background at (x, y, w, h); a white border in high contrast.
pub(super) fn card(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, alpha: f64) {
    use helix_draw::screen::fade;
    let (x, y, w, h) = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    if helix_draw::high_contrast() {
        canvas.round_rect(x, y, w, h, CARD_RADIUS, fade(0xFF000000, alpha));
        canvas.round_outline(x + 1.0, y + 1.0, w - 2.0, h - 2.0, CARD_RADIUS - 1.0, 1.0, fade(0xFFFFFFFF, alpha));
    } else {
        canvas.round_rect(x, y, w, h, CARD_RADIUS, fade(CARD_FILL, alpha));
    }
}

/// The notification card's height for `counts` (none: 0).
pub(super) fn notes_height(counts: &[LockApp], more: usize) -> i32 {
    if counts.is_empty() && more == 0 {
        return 0;
    }
    let rows: i32 = counts.iter().map(|a| APP_ROW + a.details.len() as i32 * DETAIL_ROW).sum();
    2 * CARD_PAD - 4 + rows + if more > 0 { DETAIL_ROW + 4 } else { 0 }
}

/// The media card at (x, y), `w` wide: art (or the player's icon) at the
/// left, the title and artist, and round previous / play-pause / next
/// buttons; returns the buttons' targets.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_media(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (x, y, w): (i32, i32, i32), player: &Player, icons: &LockIcons, compact: bool, state: impl Fn(Playback) -> helix_draw::screen::ButtonState, alpha: f64) -> Vec<(Rect, Playback)> {
    use helix_draw::screen::fade;
    let (card_h, art_px) = if compact { (MEDIA_COMPACT_H, 40) } else { (MEDIA_H, ART) };
    card(canvas, x, y, w, card_h, alpha);
    let (ax, ay) = (x + CARD_PAD, y + CARD_PAD);
    let (fx, fy, fs) = (f64::from(ax), f64::from(ay), f64::from(art_px));
    match (&icons.art, icons.art_is_cover) {
        (Some(art), true) => canvas.image_rounded(fx, fy, fs, fs, 8.0, art, alpha as f32),
        (art, _) => {
            canvas.round_rect(fx, fy, fs, fs, 8.0, fade(0x1FFFFFFF, alpha));
            if let Some(icon) = art {
                let s = (fs * 0.57).round();
                canvas.image_alpha(fx + (fs - s) / 2.0, fy + (fs - s) / 2.0, s, s, icon, alpha as f32);
            }
        }
    }
    // The buttons at the bottom right (in a compact card: the right),
    // play/pause larger.
    let commands = [Playback::Previous, Playback::PlayPause, Playback::Next];
    let mut hits = Vec::new();
    let cy = if compact { f64::from(y + card_h / 2) } else { f64::from(y + card_h - CARD_PAD - MEDIA_BUTTON / 2) };
    let mut right = f64::from(x + w - CARD_PAD);
    for (i, command) in commands.into_iter().enumerate().rev() {
        let d = if command == Playback::PlayPause { 44.0 } else { f64::from(MEDIA_BUTTON) };
        let cx = right - d / 2.0;
        helix_draw::screen::round_button(canvas, cx, cy, d, icons.media[i].as_ref(), 20.0, state(command), alpha);
        // Targets at least 44 px square.
        let t = d.max(44.0);
        hits.push((Rect { x: (cx - t / 2.0) as i32, y: (cy - t / 2.0) as i32, w: t as i32, h: t as i32 }, command));
        right -= d + 6.0;
    }
    hits.reverse();
    let tx = ax + art_px + 14;
    let title = if player.title.is_empty() { player.identity.as_str() } else { player.title.as_str() };
    if compact {
        // The title only, beside the buttons.
        let tw = (right as i32 - 8 - tx) as f32;
        label(bold.or(text), canvas, title, tx as f32, baseline(bold.or(text), y, card_h, size::BODY), size::BODY, fade(FG, alpha), tw);
    } else {
        let tw = (x + w - CARD_PAD - tx) as f32;
        label(bold.or(text), canvas, title, tx as f32, (y + CARD_PAD + 18) as f32, size::BODY, fade(FG, alpha), tw);
        let artist = if player.artist.is_empty() && !player.title.is_empty() { player.identity.as_str() } else { player.artist.as_str() };
        label(text, canvas, artist, tx as f32, (y + CARD_PAD + 38) as f32, size::CAPTION, fade(CARD_DIM, alpha), tw);
    }
    hits
}

/// The notification card at (x, y), `w` wide: per app its icon, name and
/// count, and the detail lines it allows; `more` apps that did not fit
/// make a last "+N more" line. Returns its height.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_notes(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (x, y, w): (i32, i32, i32), counts: &[LockApp], app_icons: &[Option<Image>], more: usize, alpha: f64) -> i32 {
    use helix_draw::screen::fade;
    let h = notes_height(counts, more);
    if h == 0 {
        return 0;
    }
    card(canvas, x, y, w, h, alpha);
    let ix = x + CARD_PAD + 2;
    let tx = ix + 24 + 10;
    let mut row = y + CARD_PAD - 2;
    for (i, app) in counts.iter().enumerate() {
        let mid = row + APP_ROW / 2;
        match app_icons.get(i).and_then(Option::as_ref) {
            Some(icon) => canvas.image_alpha(f64::from(ix), f64::from(mid - 12), 24.0, 24.0, icon, alpha as f32),
            None => canvas.round_rect(f64::from(ix), f64::from(mid - 12), 24.0, 24.0, 6.0, fade(0x33FFFFFF, alpha)),
        }
        // The count in a pill at the right.
        let count = app.count.to_string();
        let cw = text.map_or(10.0, |t| t.width(&count, size::CAPTION)).ceil() as i32 + 16;
        let (px, py) = (x + w - CARD_PAD - cw, mid - 11);
        canvas.round_rect(f64::from(px), f64::from(py), f64::from(cw), 22.0, 11.0, fade(0x33FFFFFF, alpha));
        label(text, canvas, &count, (px + 8) as f32, baseline(text, py, 22, size::CAPTION), size::CAPTION, fade(FG, alpha), cw as f32);
        label(bold.or(text), canvas, &app.name, tx as f32, baseline(bold.or(text), row, APP_ROW, size::LABEL), size::LABEL, fade(FG, alpha), (px - 8 - tx) as f32);
        row += APP_ROW;
        for (summary, body) in &app.details {
            let max = (x + w - CARD_PAD - tx) as f32;
            let base = (row + 14) as f32;
            // The summary in bold, then the body's first line.
            let sw = bold.or(text).map_or(0.0, |t| t.width(summary, size::CAPTION)).min(max);
            label(bold.or(text), canvas, summary, tx as f32, base, size::CAPTION, fade(FG, alpha), max);
            if !body.is_empty() && sw + 24.0 < max {
                label(text, canvas, body, tx as f32 + sw + 6.0, base, size::CAPTION, fade(CARD_DIM, alpha), max - sw - 6.0);
            }
            row += DETAIL_ROW;
        }
    }
    if more > 0 {
        let line = helix_i18n::trnf!("+{n} more notification", "+{n} more notifications", more);
        let lx = if counts.is_empty() { ix } else { tx };
        label(text, canvas, &line, lx as f32, (row + 16) as f32, size::CAPTION, fade(CARD_DIM, alpha), (w - 2 * CARD_PAD) as f32);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u32, app: &str) -> Banner {
        Banner { id, app_name: app.into(), app_id: app.to_lowercase(), summary: format!("secret {id}"), body: "<b>line</b>\nmore".into(), buttons: Vec::new(), icon: None, markup: true, expanded: false, arrived: 0 }
    }

    #[test]
    fn counts() {
        let entries = vec![note(5, "Chat"), note(4, "Mail"), note(3, "Chat"), note(2, "Calendar"), note(1, "Mail"), note(0, "Chat")];
        let known: HashSet<u32> = [1].into();
        let counts = |policy: fn(&str) -> (bool, bool)| lock_counts(&entries, &known, policy).into_iter().map(|a| (a.name, a.count, a.details.len())).collect::<Vec<_>>();
        let plain = |_: &str| (true, false);
        assert_eq!(counts(plain), [("Chat".into(), 3, 0), ("Calendar".into(), 1, 0), ("Mail".into(), 1, 0)]);
        // Mail hidden; Chat with its two newest notifications' text.
        let mixed = |app: &str| (app != "mail", app == "chat");
        assert_eq!(counts(mixed), [("Chat".into(), 3, 2), ("Calendar".into(), 1, 0)]);
        let chat = &lock_counts(&entries, &known, mixed)[0];
        assert_eq!(chat.details[0], ("secret 5".to_owned(), "line".to_owned()));
        assert!(lock_counts(&entries, &entries.iter().map(|e| e.id).collect(), plain).is_empty());
    }
}
