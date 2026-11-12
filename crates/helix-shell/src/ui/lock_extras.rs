//! What the lock screen shows besides the password (helix-desktop-spec
//! `contracts/dbus/org.gnome.ScreenSaver.md`, "Lock screen"): the media
//! player and counts of notifications that arrived while locked.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use helix_draw::Image;

use super::*;
use crate::calendar::Player;
use crate::notifications::Banner;
use crate::shell_keys::Playback;

const MEDIA_EVERY: Duration = Duration::from_secs(5);
const MAX_APPS: usize = 5;

#[derive(Default)]
pub(super) struct LockExtras {
    pub media: Option<Player>,
    /// Notifications in the list when the screen locked.
    pub known: HashSet<u32>,
    pub asked: Option<Instant>,
}

/// Per app, the notifications not in `known`: most first, then by name.
pub(super) fn lock_counts(entries: &[Banner], known: &HashSet<u32>) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for e in entries.iter().filter(|e| !known.contains(&e.id)) {
        match counts.iter_mut().find(|(app, _)| *app == e.app_name) {
            Some(c) => c.1 += 1,
            None => counts.push((e.app_name.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    counts.truncate(MAX_APPS);
    counts
}

fn show_in_lock_screen() -> bool {
    use gio::prelude::*;
    const SCHEMA: &str = "org.gnome.desktop.notifications";
    !helix_settings::has_schema(SCHEMA) || gio::Settings::new(SCHEMA).boolean("show-in-lock-screen")
}

/// The media buttons' icons: previous, play or pause, next.
pub(super) type LockIcons = [Option<Image>; 3];

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
            self.lock_extras.media = player;
            self.draw_lock();
        }
    }

    pub(super) fn lock_icons(&mut self, scale: f64) -> LockIcons {
        if self.lock_extras.media.is_none() {
            return [None, None, None];
        }
        let playing = self.lock_extras.media.as_ref().is_some_and(|p| p.playing);
        let play = if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
        [
            self.symbolic_icon("media-skip-backward-symbolic", scale),
            self.symbolic_icon(play, scale),
            self.symbolic_icon("media-skip-forward-symbolic", scale),
        ]
    }

    /// The counts to show (none unless allowed).
    pub(super) fn lock_note_counts(&self) -> Vec<(String, usize)> {
        if show_in_lock_screen() {
            lock_counts(&self.notify.list.entries, &self.lock_extras.known)
        } else {
            Vec::new()
        }
    }

    /// A click on a lock surface.
    pub(super) fn lock_click(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) {
        let Some(view) = self.lock.surfaces.iter().find(|v| v.surface.wl_surface() == surface) else { return };
        if let Some(&(_, command)) = view.hits.iter().find(|(r, _)| r.contains(x, y)) {
            let _ = self.requests.send(UiRequest::LockMedia(Some(command)));
        }
    }
}

/// Draw the media player and the counts from `y` down; returns the
/// buttons' rectangles.
pub(super) fn paint_lock_extras(
    canvas: &mut Canvas,
    (text, bold): (Option<&Text>, Option<&Text>),
    w: i32,
    mut y: i32,
    media: Option<&Player>,
    counts: &[(String, usize)],
    icons: &LockIcons,
) -> Vec<(Rect, Playback)> {
    let mut hits = Vec::new();
    let center = |t: Option<&Text>, s: &str, px: f32| (w as f32 - t.map_or(0.0, |t| t.width(s, px))) / 2.0;
    if let Some(player) = media {
        let bw = 360.min(w - 40);
        let bx = (w - bw) / 2;
        canvas.rect(bx, y, bw, 104, 0x60000000);
        let title = if player.title.is_empty() { player.identity.as_str() } else { player.title.as_str() };
        label(bold.or(text), canvas, title, center(bold.or(text), title, 15.0).max(bx as f32 + 12.0), (y + 26) as f32, 15.0, FG, (bw - 24) as f32);
        label(text, canvas, &player.artist, center(text, &player.artist, 13.0).max(bx as f32 + 12.0), (y + 46) as f32, 13.0, DIM_FG, (bw - 24) as f32);
        let commands = [Playback::Previous, Playback::PlayPause, Playback::Next];
        let (btn, gap) = (44, 12);
        let row_x = (w - 3 * btn - 2 * gap) / 2;
        for (i, command) in commands.into_iter().enumerate() {
            let r = Rect { x: row_x + i as i32 * (btn + gap), y: y + 60, w: btn, h: 32 };
            canvas.rect(r.x, r.y, r.w, r.h, 0x40ffffff);
            if let Some(icon) = &icons[i] {
                canvas.image(f64::from(r.x) + f64::from(r.w - 16) / 2.0, f64::from(r.y) + f64::from(r.h - 16) / 2.0, 16.0, 16.0, icon);
            }
            hits.push((r, command));
        }
        y += 120;
    }
    for (app, n) in counts {
        let line = format!("{app}: {n} notification{}", if *n == 1 { "" } else { "s" });
        label(text, canvas, &line, center(text, &line, 14.0), (y + 18) as f32, 14.0, FG, w as f32);
        y += 24;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u32, app: &str) -> Banner {
        Banner { id, app_name: app.into(), summary: "secret".into(), body: String::new(), buttons: Vec::new(), icon: None, markup: false }
    }

    #[test]
    fn counts() {
        let entries = vec![note(5, "Chat"), note(4, "Mail"), note(3, "Chat"), note(2, "Calendar"), note(1, "Mail")];
        let known: HashSet<u32> = [1].into();
        assert_eq!(lock_counts(&entries, &known), [("Chat".to_owned(), 2), ("Calendar".to_owned(), 1), ("Mail".to_owned(), 1)]);
        assert!(lock_counts(&entries, &entries.iter().map(|e| e.id).collect()).is_empty());
    }
}
