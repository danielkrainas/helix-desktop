//! The lock screen's picture (helix-desktop-spec
//! `contracts/dbus/org.gnome.ScreenSaver.md`, "Lock screen (Helix)").
//! What one monitor's lock surface shows is a [`LockModel`]; [`paint`]
//! draws it over the lock screen's background and says where its
//! buttons are and what the accessibility bus is told. It reads nothing
//! else, so tests paint it into plain buffers.

use super::lock_extras::{paint_media, paint_notes, LockApp, LockIcons, CARD_GAP, MEDIA_COMPACT_H, MEDIA_H};
use super::atspi::playback_name;
use super::*;
use crate::a11y::{Node, Role};
use helix_draw::screen::{BODY, CAPTION};
use helix_draw::Image;
use crate::calendar::Player;
use crate::shell_keys::Playback;

/// What a place on a lock surface does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LockHit {
    /// The button at the field's end: what Enter does.
    Unlock,
    /// The keyboard layout in the field: switches to the next one.
    Layout,
    Media(Playback),
    Power(LockPowerHit),
    SwitchUser,
    /// The resting state: anywhere else shows the prompt.
    Reveal,
}

/// Everything one lock surface shows.
pub(super) struct LockModel<'a> {
    /// This monitor has the prompt (the others show the clock and date
    /// only).
    pub prompt: bool,
    /// The time ("14:05", "2:05"), its AM/PM suffix in 12-hour format,
    /// and the date ("Saturday, November 14").
    pub clock: &'a str,
    pub suffix: Option<&'a str>,
    pub date: &'a str,
    /// The user's real name (else login name), and picture.
    pub user: &'a str,
    pub picture: Option<&'a Image>,
    /// Characters typed (or being checked).
    pub typed: usize,
    /// The password is being checked; `spinner` is the spinner's angle
    /// (`None`: no motion, it stands still).
    pub checking: bool,
    pub spinner: Option<f64>,
    /// The last check failed (until a key is typed): the field's ring is
    /// red; `message` says why.
    pub failed: bool,
    /// The field's sideways offset (a shake after a failure).
    pub shake: f64,
    /// Under the field: a failure; "Checking…" is only told, not drawn.
    pub message: Option<&'a str>,
    pub media: Option<&'a Player>,
    pub counts: &'a [LockApp],
    pub icons: &'a LockIcons,
    /// "Restart…" and "Power Off…" offered (`restart-enabled` and logind).
    pub restart: bool,
    pub power_off: bool,
    /// Restart or Power Off asked, to be confirmed, and the apps busy.
    pub confirming: Option<PowerAction>,
    pub blockers: &'a [(String, String)],
    /// The button the pointer is over, the one pressed, and the one with
    /// the keyboard focus (`None`: the password field has it).
    pub hover: Option<LockHit>,
    pub pressed: Option<LockHit>,
    pub focus: Option<LockHit>,
    /// Caps Lock is on: a warning on the field.
    pub caps_lock: bool,
    /// The input source's short name, with two or more sources.
    pub layout: Option<&'a str>,
    /// "Switch User…" offered.
    pub switch_user: bool,
    /// The prompt is shown (else the resting state: the clock, the date
    /// and the cards); `reveal_pos` and `reveal_alpha` are how far it got
    /// (0 resting, 1 shown), for places and for opacity.
    pub revealed: bool,
    pub reveal_pos: f64,
    pub reveal_alpha: f64,
    /// Everything's opacity over the background (fading in when locking,
    /// out when unlocking), and how far it is still below its place.
    pub alpha: f64,
    pub slide: f64,
}

/// What [`paint`] drew: the places that react to clicks, and the same on
/// the accessibility bus.
pub(super) struct Painted {
    pub hits: Vec<(Rect, LockHit)>,
    pub nodes: Vec<Node>,
}

/// What a lock surface's blurred background was made for: the picture
/// (its serial), the buffer size, the scale (its bits) and high contrast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BackdropKey {
    pub serial: u64,
    pub size: (usize, usize),
    pub scale: u64,
    pub high_contrast: bool,
}

/// Secondary text on the lock background (the user, messages): light
/// enough for 4.5:1 on the toned picture.
const SOFT_FG: u32 = helix_draw::backdrop::TEXT;

/// Warnings (Caps Lock) on the lock background: yellow.
const WARNING: u32 = 0xFFF8E45C;
const CAPS_ON: &str = "Caps Lock is on";
/// The Caps Lock sign's room in the field.
const CAPS_W: f64 = 28.0;

/// The Caps Lock sign centered at (cx, cy): an arrow up over a bar.
fn caps_sign(canvas: &mut Canvas, cx: f64, cy: f64, color: u32) {
    canvas.line((cx - 6.0, cy + 1.0), (cx, cy - 6.0), 2.0, color);
    canvas.line((cx, cy - 6.0), (cx + 6.0, cy + 1.0), 2.0, color);
    canvas.line((cx - 6.0, cy + 1.0), (cx + 6.0, cy + 1.0), 2.0, color);
    canvas.line((cx - 4.0, cy + 6.0), (cx + 4.0, cy + 6.0), 2.0, color);
}

/// Error messages on the lock background: a light red with 4.5:1 there.
const ERROR_TEXT: u32 = 0xFFFFB8B0;

/// The lock background of a `w`×`h` (logical) surface at `scale`: the
/// picture blurred and toned (helix-desktop-spec `background.md`, "Lock
/// screen"), or black in high contrast. Pixels in ARGB8888.
pub(super) fn backdrop(background: &crate::wallpaper::Background, (w, h): (usize, usize), scale: f64) -> Vec<u8> {
    let (pw, ph) = (physical(w, scale), physical(h, scale));
    let mut pixels = vec![0u8; pw * ph * 4];
    if helix_draw::high_contrast() {
        helix_draw::backdrop::high_contrast(&mut pixels);
    } else {
        super::background::paint(&mut pixels, pw, ph, scale, background, (w as f64, h as f64), None);
        helix_draw::backdrop::apply(&mut pixels, pw, ph, scale, &helix_draw::backdrop::LOCK);
    }
    pixels
}

/// The fonts the lock screen draws with: regular, bold, and the big
/// clock's.
pub(super) struct LockFonts<'a> {
    pub text: Option<&'a Text>,
    pub bold: Option<&'a Text>,
    pub display: Option<&'a Text>,
}

/// The bottom buttons: their size, their captions' size, the margin
/// around them.
const BOTTOM_BUTTON: i32 = 48;
const BOTTOM_MARGIN: i32 = 24;

/// How button `hit` looks.
fn state(m: &LockModel, hit: LockHit) -> helix_draw::screen::ButtonState {
    helix_draw::screen::ButtonState { hovered: m.hover == Some(hit), pressed: m.pressed == Some(hit), focused: m.focus == Some(hit) }
}

/// A round bottom button with `caption` under it, from `edge` (its left
/// edge, or with `from_right` its right edge); returns its target.
#[allow(clippy::too_many_arguments)]
fn bottom_button(canvas: &mut Canvas, text: Option<&Text>, m: &LockModel, edge: i32, h: i32, caption: &str, icon: Option<&Image>, hit: LockHit, from_right: bool, alpha: f64) -> Rect {
    let cw = text.map_or(72.0, |t| t.width(caption, CAPTION)).ceil() as i32;
    let slot = (cw + 8).max(BOTTOM_BUTTON + 24);
    let x = if from_right { edge - slot } else { edge };
    let cx = f64::from(x) + f64::from(slot) / 2.0;
    let caption_base = h - BOTTOM_MARGIN;
    let cy = f64::from(caption_base - 20 - BOTTOM_BUTTON / 2);
    helix_draw::screen::round_button(canvas, cx, cy, f64::from(BOTTOM_BUTTON), icon, 20.0, state(m, hit), alpha);
    label(text, canvas, caption, (cx - f64::from(cw) / 2.0) as f32, caption_base as f32, CAPTION, helix_draw::screen::fade(FG, alpha), cw as f32 + 1.0);
    Rect { x, y: cy as i32 - BOTTOM_BUTTON / 2 - 4, w: slot, h: BOTTOM_BUTTON + 4 + 24 }
}

/// The Restart / Power Off question, in a card in the middle over a
/// scrim: the question, the apps that are busy, Cancel (focused unless
/// the keyboard moved) and the destructive button. Returns what it asks
/// (and the busy apps) for the accessibility bus.
fn confirm_card(canvas: &mut Canvas, fonts: &LockFonts, m: &LockModel, action: PowerAction, (w, h): (i32, i32), hits: &mut Vec<(Rect, LockHit)>, a: f64) -> (&'static str, Vec<String>) {
    use helix_draw::screen::{fade, push_button, ButtonKind};
    let (text, bold) = (fonts.text, fonts.bold);
    let anyway = !m.blockers.is_empty();
    let (question, verb) = match (action == PowerAction::Restart, anyway) {
        (true, false) => ("Restart this computer?", "Restart"),
        (true, true) => ("Restart anyway?", "Restart"),
        (false, false) => ("Power off this computer?", "Power Off"),
        (false, true) => ("Power off anyway?", "Power Off"),
    };
    let busy: Vec<&(String, String)> = m.blockers.iter().take(4).collect();
    let rows = busy.len() as i32;
    let row_h = 40;
    let cw = 420.min(w - 32);
    let ch = 24 + 30 + if anyway { 30 + rows * row_h + 8 } else { 8 } + 44 + 24;
    let (cx, cy) = ((w - cw) / 2, ((h - ch) / 2).max(16));
    canvas.rect(0, 0, w, h, fade(0x99000000, a));
    // Opaque: nothing of the prompt shows through.
    let (fx, fy, fw, fh) = (f64::from(cx), f64::from(cy), f64::from(cw), f64::from(ch));
    canvas.round_rect(fx, fy, fw, fh, 14.0, fade(0xFF24252D, a));
    canvas.round_outline(fx, fy, fw, fh, 14.0, 1.0, fade(if helix_draw::high_contrast() { 0xFFFFFFFF } else { 0xFF3C3D48 }, a));
    let inner = (cw - 48) as f32;
    let title_face = bold.or(text);
    label(title_face, canvas, question, cx as f32 + 24.0, (cy + 24 + 22) as f32, 20.0, fade(FG, a), inner);
    let mut y = cy + 24 + 30;
    if anyway {
        label(text, canvas, "Some apps are busy:", cx as f32 + 24.0, (y + 22) as f32, BODY, fade(SOFT_FG, a), inner);
        y += 30;
        for (app, reason) in &busy {
            label(bold.or(text), canvas, app, cx as f32 + 24.0, (y + 16) as f32, BODY, fade(FG, a), inner);
            label(text, canvas, reason, cx as f32 + 24.0, (y + 34) as f32, CAPTION, fade(SOFT_FG, a), inner);
            y += row_h;
        }
    }
    let by = cy + ch - 24 - 44;
    let half = (cw - 48 - 12) / 2;
    let cancel = Rect { x: cx + 24, y: by, w: half, h: 44 };
    let confirm = Rect { x: cx + 24 + half + 12, y: by, w: half, h: 44 };
    let (cancel_hit, confirm_hit) = (LockHit::Power(LockPowerHit::Cancel), LockHit::Power(LockPowerHit::Confirm));
    let r = |r: Rect| (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
    push_button(canvas, text, r(cancel), "Cancel", ButtonKind::Normal, state(m, cancel_hit), a);
    push_button(canvas, text, r(confirm), verb, ButtonKind::Destructive, state(m, confirm_hit), a);
    hits.push((cancel, cancel_hit));
    hits.push((confirm, confirm_hit));
    let shown = if anyway { busy.iter().map(|(app, reason)| if reason.is_empty() { app.clone() } else { format!("{app}: {reason}") }).collect() } else { Vec::new() };
    (question, shown)
}

/// Draw the clock (time and AM/PM suffix) centered on `cx` with its
/// baseline at `base`, the digits `px` tall: tabular digits, the suffix
/// smaller and lighter.
fn draw_clock(canvas: &mut Canvas, fonts: &LockFonts, m: &LockModel, cx: f32, base: f32, px: f32, alpha: f64) {
    use helix_draw::screen::fade;
    let Some(face) = fonts.display.or(fonts.text) else { return };
    let time_w = helix_draw::screen::tabular_width(face, m.clock, px);
    let suffix_px = (px * 0.38).round();
    let (suffix_w, gap) = match (m.suffix, fonts.text) {
        (Some(suffix), Some(text)) => (text.width(suffix, suffix_px), px * 0.12),
        _ => (0.0, 0.0),
    };
    let x = cx - (time_w + gap + suffix_w) / 2.0;
    helix_draw::screen::draw_tabular(canvas, face, m.clock, x, base, px, fade(FG, alpha));
    if let (Some(suffix), Some(text)) = (m.suffix, fonts.text) {
        label(Some(text), canvas, suffix, x + time_w + gap, base, suffix_px, fade(SOFT_FG, alpha), suffix_w + 1.0);
    }
}

/// Where the lock screen's pieces go on a `w`×`h` monitor: one centered
/// column, spaced by [`helix_draw::screen::SPACE`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Layout {
    /// The column: its left edge and width.
    pub col: (i32, i32),
    /// The clock's size and baseline, the date's baseline.
    pub clock_px: f32,
    pub clock_base: i32,
    pub date_base: i32,
    /// The avatar's center and radius, the name's baseline.
    pub avatar: (f64, f64, f64),
    pub name_base: i32,
    /// The password field (its place does not depend on the extras).
    pub field: Rect,
    /// Where the extras may go: from `extras.0` to `extras.1`, above the
    /// bottom buttons' band.
    pub extras: (i32, i32),
    /// Lines a message under the field may take.
    pub message_lines: usize,
}

/// A message line's height under the field.
const MESSAGE_LINE: i32 = 22;
/// The band at the bottom the buttons keep for themselves.
pub(super) const BOTTOM_BAND: i32 = BOTTOM_MARGIN + 20 + BOTTOM_BUTTON + 8;

/// The column's layout on a `w`×`h` monitor (`cap`: the clock's cap
/// height for its size). The identity and the field are centered at 45%
/// of the height, the clock and date above with a clear gap. Shorter
/// monitors (under 800 and 700 px) tighten the gaps and raise the group
/// a little to leave the extras room; everything moves down only as far
/// as the clock needs.
pub(super) fn layout(w: i32, h: i32, cap: impl Fn(f32) -> f32) -> Layout {
    use helix_draw::screen::{display_px, ENTRY_H, SPACE};
    let col_w = 380.min(w - 32);
    let col = ((w - col_w) / 2, col_w);
    let clock_px = display_px(f64::from(h));
    let r = (f64::from(h) * 0.045).clamp(32.0, 56.0);
    let (name_gap, field_gap, entry_h) = (SPACE[3], SPACE[4], ENTRY_H as i32);
    // Avatar, name (a 28 px line), field.
    let group = (2.0 * r) as i32 + name_gap + 28 + field_gap + entry_h;
    // Above the group: the clock's cap height, the date (a 26 px line)
    // and a 48 px gap; then a margin to the top.
    let date_gap = SPACE[3];
    let (gap, center, message_lines) = match h {
        ..700 => (SPACE[4], 0.40, 1),
        700..800 => (SPACE[5], 0.42, 2),
        _ => (SPACE[6], 0.45, 2),
    };
    let above = cap(clock_px).ceil() as i32 + date_gap + 26 + gap;
    let top = ((f64::from(h) * center) as i32 - group / 2).max(SPACE[5] + above);
    let clock_base = top - gap - 26 - date_gap + 4;
    let date_base = top - gap - 6;
    let avatar = (f64::from(w) / 2.0, f64::from(top) + r, r);
    let name_base = top + (2.0 * r) as i32 + name_gap + 22;
    let field = Rect { x: col.0, y: name_base + 6 + field_gap, w: col.1, h: entry_h };
    let extras = (field.y + field.h + message_lines as i32 * MESSAGE_LINE + 4, h - BOTTOM_BAND - SPACE[3]);
    Layout { col, clock_px, clock_base, date_base, avatar, name_base, field, extras, message_lines }
}

/// What of the extras fits in `room` px: whether the media card shows
/// (and compact), the apps shown (details dropped from the last first,
/// then apps), and how many notifications were left out ("+N more").
pub(super) fn fit_extras(room: i32, media: bool, counts: &[LockApp]) -> (Option<bool>, Vec<LockApp>, usize) {
    use super::lock_extras::{notes_height, MEDIA_COMPACT_H};
    // With the media card as it is (`Some(false)`), compact, or without
    // it: drop detail lines (the last app's first), then apps, until the
    // rest fits.
    let trim = |media_h: i32| -> Option<(Vec<LockApp>, usize)> {
        let mut apps = counts.to_vec();
        let mut more = 0;
        while media_h + notes_height(&apps, more) > room {
            if let Some(app) = apps.iter_mut().rev().find(|a| !a.details.is_empty()) {
                app.details.pop();
            } else {
                more += apps.pop()?.count;
            }
        }
        Some((apps, more))
    };
    if !media {
        return trim(0).map_or((None, Vec::new(), 0), |(apps, more)| (None, apps, more));
    }
    // The full card while at least one app still shows (or none came);
    // then the compact card, likewise; then whatever fits.
    for compact in [false, true] {
        let media_h = if compact { MEDIA_COMPACT_H } else { MEDIA_H } + CARD_GAP;
        if let Some((apps, more)) = trim(media_h).filter(|(apps, _)| !apps.is_empty() || counts.is_empty()) {
            return (Some(compact), apps, more);
        }
    }
    for compact in [false, true] {
        let media_h = if compact { MEDIA_COMPACT_H } else { MEDIA_H } + CARD_GAP;
        if let Some((apps, more)) = trim(media_h) {
            return (Some(compact), apps, more);
        }
    }
    trim(0).map_or((None, Vec::new(), 0), |(apps, more)| (None, apps, more))
}

/// The resting state's places on a `w`×`h` monitor: a larger clock and
/// the date a little above the middle, the cards under them, a hint at
/// the bottom.
#[derive(Debug, Clone, Copy)]
pub(super) struct Rest {
    pub clock_px: f32,
    pub clock_base: i32,
    pub date_base: i32,
    pub extras: (i32, i32),
    pub hint_base: i32,
}

/// The resting state's layout (`cap` as for [`layout`]).
pub(super) fn rest_layout(h: i32, cap: impl Fn(f32) -> f32) -> Rest {
    use helix_draw::screen::{display_px, SPACE};
    let clock_px = (display_px(f64::from(h)) * 1.25).round();
    let clock_base = (f64::from(h) * 0.30 + f64::from(cap(clock_px)) / 2.0).round() as i32;
    let date_base = clock_base + 40;
    let hint_base = h - SPACE[5];
    Rest { clock_px, clock_base, date_base, extras: (date_base + SPACE[6], hint_base - 20 - SPACE[4]), hint_base }
}

const HINT: &str = "Press a key or click to unlock";

/// Draw `m` on a `w`×`h` (logical) surface whose background is painted.
pub(super) fn paint(canvas: &mut Canvas, fonts: &LockFonts, m: &LockModel, w: usize, h: usize) -> Painted {
    use helix_draw::screen::{fade, Entry, Trailing, ENTRY_H, ERROR, SUBTITLE, TITLE};
    let (text, bold) = (fonts.text, fonts.bold);
    let mut hits = Vec::new();
    let center = |s: &str, px: f32| -> f32 { (w as f32 - text.map_or(0.0, |t| t.width(s, px))) / 2.0 };
    let cap = |px: f32| fonts.display.or(text).map_or(px * 0.72, |t| t.ascent(px) * 0.72);
    let l = layout(w as i32, h as i32, cap);
    let rest = rest_layout(h as i32, cap);
    let (fx, fw) = l.col;
    // Everything's opacity (fading in when locking, out when unlocking),
    // and the prompt's (revealed from the resting state).
    let a = m.alpha;
    // (The prompt fades in once the cards have mostly moved out of its
    // way.)
    let pa = a * ((m.reveal_alpha - 0.55) / 0.45).clamp(0.0, 1.0);
    let t = if m.prompt { m.reveal_pos } else { 0.0 };
    let lerp = |from: i32, to: i32| f64::from(from) + f64::from(to - from) * t;
    let slide = m.slide;

    // The time and the date: large while resting, moving up and shrinking
    // as the prompt shows (other monitors keep the resting look).
    let clock_px = rest.clock_px + (l.clock_px - rest.clock_px) * t as f32;
    let clock_base = (lerp(rest.clock_base, l.clock_base) + slide) as f32;
    draw_clock(canvas, fonts, m, w as f32 / 2.0, clock_base, clock_px, a);
    let date_base = (lerp(rest.date_base, l.date_base) + slide) as f32;
    label(text, canvas, m.date, center(m.date, SUBTITLE), date_base, SUBTITLE, fade(FG, 0.85 * a), w as f32);
    if !m.prompt {
        // Another monitor: nothing to press.
        return Painted { hits, nodes: Vec::new() };
    }
    // The prompt rises a little as it fades in.
    let dy = (1.0 - t) * 24.0 + slide;

    if pa > 0.0 {
        // Who: the picture (or initials), the name.
        let (ax, ay, ar) = l.avatar;
        helix_draw::screen::avatar(canvas, text, bold, m.user, m.picture, ax, ay + dy, ar, pa);
        let name_face = bold.or(text);
        let name_x = (w as f32 - name_face.map_or(0.0, |t| t.width(m.user, TITLE).min(w as f32 - 32.0))) / 2.0;
        label(name_face, canvas, m.user, name_x, l.name_base as f32 + dy as f32, TITLE, fade(FG, pa), w as f32 - 32.0);
    }

    // The password field, rounded, with the unlock button at its end.
    let field = Rect { y: l.field.y + dy.round() as i32, ..l.field };
    // Caps Lock and the layout matter while typing.
    let caps = m.caps_lock && !m.checking;
    let layout_w = m.layout.map_or(0.0, |name| f64::from(text.map_or(24.0, |t| t.width(name, CAPTION)).ceil()).max(14.0) + 20.0 + 8.0);
    if pa > 0.0 {
        let entry = Entry {
            x: f64::from(field.x) + m.shake,
            y: f64::from(field.y),
            w: f64::from(field.w),
            focused: m.revealed && !m.checking && m.focus.is_none() && m.confirming.is_none(),
            ring: if m.failed { ERROR } else { accent() },
            // While checking: as many dots as were typed, dimmed.
            dots: m.typed,
            dots_alpha: if m.checking { 0.5 } else { 1.0 },
            placeholder: "Password",
            trailing: if m.checking { Trailing::Busy(m.spinner) } else { Trailing::Go { enabled: m.typed > 0, hovered: m.hover == Some(LockHit::Unlock) } },
            // Room for the layout and the Caps Lock sign.
            reserve: layout_w + if caps { CAPS_W } else { 0.0 },
            alpha: pa,
        };
        let placed = helix_draw::screen::entry(canvas, text, &entry);
        if let Some((bx, by, bw, bh)) = placed.button.filter(|_| m.revealed) {
            hits.push((Rect { x: bx as i32, y: by as i32, w: bw as i32, h: bh as i32 }, LockHit::Unlock));
        }
        // From the right: the layout's name in a pill, then the Caps Lock
        // sign.
        let (_, cy) = placed.trailing_center;
        let mut right = placed.room_end + layout_w + if caps { CAPS_W } else { 0.0 };
        if let Some(name) = m.layout {
            let pw = layout_w - 8.0;
            let px = right - pw;
            let hit = LockHit::Layout;
            let st = state(m, hit);
            let fill = if st.pressed { 0x59FFFFFF } else if st.hovered { 0x40FFFFFF } else { 0x26FFFFFF };
            canvas.round_rect(px, cy - 12.0, pw, 24.0, 12.0, fade(fill, pa));
            if st.focused {
                canvas.round_outline(px - 2.0, cy - 14.0, pw + 4.0, 28.0, 14.0, 2.0, fade(accent(), pa));
            }
            let tw = text.map_or(0.0, |t| t.width(name, CAPTION));
            label(text, canvas, name, (px + (pw - f64::from(tw)) / 2.0) as f32, (cy + 4.5) as f32, CAPTION, fade(FG, pa), pw as f32);
            // A target as tall as the field.
            if m.revealed {
                hits.push((Rect { x: px as i32 - 2, y: field.y, w: pw as i32 + 4, h: field.h }, hit));
            }
            right = px - 8.0;
        }
        if caps {
            caps_sign(canvas, right - CAPS_W / 2.0 + 4.0, cy, fade(WARNING, pa));
        }
        // Right under the field: Caps Lock, a failure.
        let mut below = field.y + ENTRY_H as i32;
        let mut room = l.message_lines;
        if caps {
            below += MESSAGE_LINE;
            room = room.saturating_sub(1).max(1);
            label(text, canvas, CAPS_ON, center(CAPS_ON, BODY), below as f32, BODY, fade(WARNING, pa), fw as f32);
        }
        if let Some(message) = m.message.filter(|_| !m.checking) {
            let lines = text.map_or_else(|| vec![message.to_owned()], |t| t.wrap(message, BODY, fw as f32, room));
            for line in &lines {
                below += MESSAGE_LINE;
                label(text, canvas, line, center(line, BODY), below as f32, BODY, fade(if m.failed { ERROR_TEXT } else { SOFT_FG }, pa), fw as f32);
            }
        }
    }
    // The extras, as much of them as fits (above the bottom buttons, or
    // the hint), under the clock while resting and under the field with
    // the prompt.
    let (top, bottom) = if m.revealed { l.extras } else { rest.extras };
    let (media, apps, more) = fit_extras(bottom - top, m.media.is_some(), m.counts);
    let mut y = (lerp(rest.extras.0, l.extras.0) + slide).round() as i32;
    if let (Some(player), Some(compact)) = (m.media, media) {
        // The media buttons work in the resting state too.
        let media = paint_media(canvas, (text, bold), (fx, y, fw), player, m.icons, compact, |c| state(m, LockHit::Media(c)), a);
        hits.extend(media.into_iter().map(|(r, command)| (r, LockHit::Media(command))));
        y += if compact { MEDIA_COMPACT_H } else { MEDIA_H } + CARD_GAP;
    }
    paint_notes(canvas, (text, bold), (fx, y, fw), &apps, &m.icons.apps, more, a);
    // Resting: how to go on.
    let hint_a = a * (1.0 - m.reveal_alpha);
    if hint_a > 0.0 {
        label(text, canvas, HINT, center(HINT, CAPTION + 1.0), rest.hint_base as f32, CAPTION + 1.0, fade(SOFT_FG, hint_a), w as f32);
    }
    // Restart and Power Off at the bottom left (`restart-enabled`),
    // "Switch User…" at the bottom right: round icon buttons, captioned.
    let mut asked = None;
    if pa > 0.0 {
        let mut left = BOTTOM_MARGIN;
        for (allowed, caption, action, icon) in [(m.restart, "Restart…", PowerAction::Restart, &m.icons.restart), (m.power_off, "Power Off…", PowerAction::PowerOff, &m.icons.power_off)] {
            if allowed {
                let r = bottom_button(canvas, text, m, left, h as i32, caption, icon.as_ref(), LockHit::Power(LockPowerHit::Ask(action)), false, pa);
                if m.revealed {
                    hits.push((r, LockHit::Power(LockPowerHit::Ask(action))));
                }
                left = r.x + r.w + 8;
            }
        }
        if m.switch_user {
            let r = bottom_button(canvas, text, m, w as i32 - BOTTOM_MARGIN, h as i32, "Switch User…", m.icons.switch_user.as_ref(), LockHit::SwitchUser, true, pa);
            if m.revealed {
                hits.push((r, LockHit::SwitchUser));
            }
        }
    }
    if !m.revealed {
        // Resting: a click anywhere else shows the prompt.
        hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, LockHit::Reveal));
    }
    // Confirming Restart or Power Off: a card in the middle over the rest,
    // which no longer reacts.
    if let Some(action) = m.confirming.filter(|_| m.revealed) {
        hits.clear();
        asked = Some(confirm_card(canvas, fonts, m, action, (w as i32, h as i32), &mut hits, a));
    }
    // (Resting, the way to the prompt is told where the hint is.)
    let reveal_at = Rect { x: fx, y: rest.hint_base - 20, w: fw, h: 28 };
    let nodes = nodes(m, &hits, field, reveal_at, asked);
    Painted { hits, nodes }
}

/// The lock screen on the accessibility bus: the clock, the user, the
/// password field and its message, the media player and notification
/// counts, and the buttons; `asked` is the power question shown, with the
/// apps that are busy.
fn nodes(m: &LockModel, hits: &[(Rect, LockHit)], field: Rect, reveal_at: Rect, asked: Option<(&str, Vec<String>)>) -> Vec<Node> {
    let clock = match m.suffix {
        Some(suffix) => format!("{} {suffix}", m.clock),
        None => m.clock.to_owned(),
    };
    let mut out = vec![Node::label(clock).id("clock"), Node::label(m.date).id("date")];
    if !m.revealed {
        // Resting: the clock, the cards, and a way to the prompt.
        extras_nodes(m, hits, &mut out);
        out.push(Node::button("Unlock", reveal_at.into()).id("reveal").describe(HINT).focused(m.focus.is_none()));
        return out;
    }
    out.push(Node::label(m.user).id("user"));
    let password = Node::text_field(Role::PasswordInput, "Password", "•".repeat(m.typed), field.into()).id("password");
    out.push(password.focused(m.confirming.is_none() && m.focus.is_none()).disabled(m.checking));
    if let Some(&(r, _)) = hits.iter().find(|(_, h)| *h == LockHit::Unlock) {
        out.push(Node::button("Unlock", r.into()).id("unlock").disabled(m.typed == 0 || m.checking));
    }
    if m.caps_lock && !m.checking {
        out.push(Node::label(CAPS_ON).id("caps-lock").live());
    }
    if let (Some(name), Some(&(r, _))) = (m.layout, hits.iter().find(|(_, h)| *h == LockHit::Layout)) {
        out.push(Node::button(name, r.into()).id("layout").describe("Keyboard layout: switch to the next").focused(m.focus == Some(LockHit::Layout)));
    }
    if let Some(message) = m.message {
        out.push(Node::label(message).id("message").live());
    }
    extras_nodes(m, hits, &mut out);
    if let Some((question, busy)) = asked {
        if !busy.is_empty() {
            out.push(Node::label("Some apps are busy:"));
            out.extend(busy.into_iter().map(Node::label));
        }
        out.push(Node::label(question).id("question").live());
    }
    for &(r, hit) in hits {
        let LockHit::Power(hit) = hit else { continue };
        let name = match hit {
            LockPowerHit::Ask(PowerAction::Restart) => "Restart…",
            LockPowerHit::Ask(_) => "Power Off…",
            LockPowerHit::Cancel => "Cancel",
            LockPowerHit::Confirm if m.confirming == Some(PowerAction::Restart) => "Restart",
            LockPowerHit::Confirm => "Power Off",
        };
        // In the question, Cancel has the focus unless the keyboard moved.
        let focused = m.focus == Some(LockHit::Power(hit)) || (m.focus.is_none() && hit == LockPowerHit::Cancel);
        out.push(Node::button(name, r.into()).id(format!("{hit:?}")).focused(focused));
    }
    if let Some(&(r, _)) = hits.iter().find(|(_, h)| *h == LockHit::SwitchUser) {
        out.push(Node::button("Switch User…", r.into()).id("switch-user").focused(m.focus == Some(LockHit::SwitchUser)));
    }
    out
}

/// The media player, its buttons and the notification counts.
fn extras_nodes(m: &LockModel, hits: &[(Rect, LockHit)], out: &mut Vec<Node>) {
    if let Some(player) = m.media {
        let title = if player.title.is_empty() { &player.identity } else { &player.title };
        out.push(Node::label(title).id("media-title"));
        if !player.artist.is_empty() {
            out.push(Node::label(&player.artist).id("media-artist"));
        }
    }
    for &(r, hit) in hits {
        if let LockHit::Media(command) = hit {
            out.push(Node::button(playback_name(command, m.media.is_some_and(|p| p.playing)), r.into()).id(format!("{command:?}")).focused(m.focus == Some(hit)));
        }
    }
    for app in m.counts {
        out.push(Node::label(format!("{}: {} notification{}", app.name, app.count, if app.count == 1 { "" } else { "s" })));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A model with nothing but the clock and the user.
    pub(crate) fn model<'a>(icons: &'a LockIcons) -> LockModel<'a> {
        LockModel {
            prompt: true,
            clock: "14:05",
            suffix: None,
            date: "Saturday, November 14",
            user: "Ann Smith",
            picture: None,
            typed: 0,
            spinner: None,
            failed: false,
            shake: 0.0,
            checking: false,
            message: None,
            media: None,
            counts: &[],
            icons,
            restart: false,
            power_off: false,
            confirming: None,
            blockers: &[],
            hover: None,
            pressed: None,
            focus: None,
            caps_lock: false,
            layout: None,
            revealed: true,
            reveal_pos: 1.0,
            reveal_alpha: 1.0,
            alpha: 1.0,
            slide: 0.0,
            switch_user: false,
        }
    }

    /// Paint `m` on a `w`×`h` surface at `scale` over a mid-gray picture.
    pub(crate) fn render(m: &LockModel, w: usize, h: usize, scale: f64) -> (Vec<u8>, Painted) {
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        let mut data = vec![0u8; pw * ph * 4];
        for px in data.as_chunks_mut::<4>().0 {
            *px = [0x80, 0x70, 0x60, 0xFF];
        }
        let faces = (Text::load(), Text::load_matching("sans-serif:bold"), helix_draw::screen::display_face());
        let fonts = LockFonts { text: faces.0.as_ref(), bold: faces.1.as_ref(), display: faces.2.as_ref() };
        let mut canvas = Canvas::scaled(&mut data, w, h, scale);
        let painted = paint(&mut canvas, &fonts, m, w, h);
        (data, painted)
    }

    #[test]
    fn opaque_and_described() {
        let icons = LockIcons::default();
        let (data, painted) = render(&LockModel { typed: 3, ..model(&icons) }, 640, 480, 1.0);
        assert!(data.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "lock frames are opaque");
        let field = painted.nodes.iter().find(|n| n.role == Role::PasswordInput).expect("a password field");
        assert_eq!(field.text.as_deref(), Some("•••"));
        assert!(field.focused);
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Saturday, November 14")), "the date is told");
        let kinds: Vec<LockHit> = painted.hits.iter().map(|(_, h)| *h).collect();
        assert_eq!(kinds, [LockHit::Unlock], "no buttons offered but the field's");
        let unlock = painted.nodes.iter().find(|n| n.name == "Unlock").expect("the unlock button is told");
        assert!(!unlock.disabled);
        let (_, painted) = render(&model(&icons), 640, 480, 1.0);
        assert!(painted.nodes.iter().find(|n| n.name == "Unlock").unwrap().disabled, "nothing typed, nothing to unlock with");
    }

    #[test]
    fn layouts() {
        let cap = |px: f32| px * 0.72;
        for (w, h) in [(1920, 1080), (1280, 720), (1366, 768), (1280, 800), (1440, 960), (1080, 1920), (800, 600), (3840, 2160)] {
            let l = layout(w, h, cap);
            assert!(l.clock_base as f32 - cap(l.clock_px) >= 24.0, "{w}x{h}: the clock fits: {l:?}");
            assert!(l.field.y + l.field.h + l.message_lines as i32 * MESSAGE_LINE <= h - BOTTOM_BAND, "{w}x{h}: the field and its message stay above the buttons");
            assert!(l.extras.1 <= h - BOTTOM_BAND, "{w}x{h}");
            assert!(l.col.1 <= 380 && l.col.0 >= 16 && l.col.0 + l.col.1 <= w - 16);
            // Balanced: the field's middle between 40% and 65% of the height.
            let mid = f64::from(l.field.y + l.field.h / 2) / f64::from(h);
            assert!((0.40..0.66).contains(&mid), "{w}x{h}: field at {mid}");
        }
    }

    #[test]
    fn extras_fit() {
        let app = |name: &str, count: usize, details: usize| LockApp { name: name.into(), app_id: String::new(), count, details: vec![("s".into(), "b".into()); details] };
        let busy: Vec<LockApp> = ["Chat", "Mail", "Calendar", "Updates", "Files"].iter().map(|n| app(n, 3, 2)).collect();
        // 1280x720 with media and five apps with details.
        let l = layout(1280, 720, |px| px * 0.72);
        let room = l.extras.1 - l.extras.0;
        let (media, apps, more) = fit_extras(room, true, &busy);
        let used = media.map_or(0, |c| if c { MEDIA_COMPACT_H } else { MEDIA_H } + CARD_GAP) + super::super::lock_extras::notes_height(&apps, more);
        assert!(used <= room, "{used} > {room}");
        assert!(more > 0, "the rest is counted");
        assert_eq!(apps.iter().map(|a| a.count).sum::<usize>() + more, 15, "nothing lost from the counts");
        // Plenty of room: everything.
        let (media, apps, more) = fit_extras(2000, true, &busy);
        assert_eq!((media, apps.len(), more), (Some(false), 5, 0));
        // No room at all: nothing.
        let (media, apps, more) = fit_extras(10, true, &busy);
        assert_eq!((media, apps.len(), more), (None, 0, 0));
    }

    #[test]
    fn caps_lock_and_layout() {
        let icons = LockIcons::default();
        let (_, painted) = render(&LockModel { typed: 2, caps_lock: true, layout: Some("de"), ..model(&icons) }, 1280, 720, 1.0);
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Caps Lock is on") && n.live), "told at once");
        assert!(painted.nodes.iter().any(|n| n.name == "de"), "the layout is a button");
        let kinds: Vec<LockHit> = painted.hits.iter().map(|(_, h)| *h).collect();
        assert_eq!(kinds, [LockHit::Unlock, LockHit::Layout]);
        let (_, painted) = render(&LockModel { typed: 2, ..model(&icons) }, 1280, 720, 1.0);
        assert!(!painted.nodes.iter().any(|n| n.text.as_deref() == Some("Caps Lock is on")));
    }

    #[test]
    fn resting() {
        let icons = LockIcons::default();
        let player = Player { title: "Song".into(), ..Default::default() };
        let m = LockModel { revealed: false, reveal_pos: 0.0, reveal_alpha: 0.0, media: Some(&player), restart: true, switch_user: true, ..model(&icons) };
        let (_, painted) = render(&m, 1280, 720, 1.0);
        let kinds: Vec<LockHit> = painted.hits.iter().map(|(_, h)| *h).collect();
        // The media buttons first (they work while resting), then anywhere.
        assert_eq!(kinds, [LockHit::Media(Playback::Previous), LockHit::Media(Playback::PlayPause), LockHit::Media(Playback::Next), LockHit::Reveal]);
        assert!(!painted.nodes.iter().any(|n| n.role == Role::PasswordInput), "no field while resting");
        let reveal = painted.nodes.iter().find(|n| n.id == "reveal").expect("a way to the prompt");
        assert!(reveal.clickable && reveal.focused);
        assert!(painted.nodes.iter().any(|n| n.name == "Play"), "the player is told");
    }

    #[test]
    fn other_monitors() {
        let icons = LockIcons::default();
        let m = LockModel { prompt: false, typed: 3, restart: true, power_off: true, switch_user: true, ..model(&icons) };
        let (_, painted) = render(&m, 1280, 720, 1.0);
        assert!(painted.hits.is_empty(), "nothing to press away from the prompt");
        assert!(painted.nodes.is_empty(), "and nothing published: one password field in the tree");
    }

    #[test]
    fn checking_and_failing() {
        let icons = LockIcons::default();
        let m = LockModel { typed: 5, checking: true, spinner: None, message: Some("Checking…"), ..model(&icons) };
        let (_, painted) = render(&m, 800, 600, 1.0);
        let field = painted.nodes.iter().find(|n| n.role == Role::PasswordInput).unwrap();
        assert_eq!(field.text.as_deref(), Some("•••••"), "what was typed stays shown while checking");
        assert!(field.disabled);
        assert!(!painted.hits.iter().any(|(_, h)| *h == LockHit::Unlock), "a spinner, no button");
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Checking…") && n.live), "told, though not drawn");
        let m = LockModel { message: Some("Incorrect password. Try again."), failed: true, ..model(&icons) };
        let (_, painted) = render(&m, 800, 600, 1.0);
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Incorrect password. Try again.") && n.live));
    }

    #[test]
    fn buttons_and_question() {
        let icons = LockIcons::default();
        let blockers = vec![("Editor".to_owned(), "Unsaved changes".to_owned())];
        let m = LockModel { restart: true, power_off: true, switch_user: true, ..model(&icons) };
        let (_, painted) = render(&m, 1280, 720, 1.0);
        for (r, hit) in &painted.hits {
            assert!(r.w >= 44 && r.h >= 44, "{hit:?} {r:?}");
        }
        let kinds: Vec<LockHit> = painted.hits.iter().map(|(_, h)| *h).collect();
        assert_eq!(kinds, [LockHit::Unlock, LockHit::Power(LockPowerHit::Ask(PowerAction::Restart)), LockHit::Power(LockPowerHit::Ask(PowerAction::PowerOff)), LockHit::SwitchUser]);
        let m = LockModel { confirming: Some(PowerAction::PowerOff), blockers: &blockers, ..m };
        let (_, painted) = render(&m, 1280, 720, 1.0);
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Power off anyway?") && n.live));
        assert!(painted.nodes.iter().any(|n| n.text.as_deref() == Some("Editor: Unsaved changes")));
        // Only the question's buttons react; Cancel has the focus.
        let kinds: Vec<LockHit> = painted.hits.iter().map(|(_, h)| *h).collect();
        assert_eq!(kinds, [LockHit::Power(LockPowerHit::Cancel), LockHit::Power(LockPowerHit::Confirm)]);
        assert!(painted.nodes.iter().find(|n| n.name == "Cancel").unwrap().focused);
        assert!(!painted.nodes.iter().find(|n| n.name == "Power Off").unwrap().focused);
        for (r, _) in &painted.hits {
            assert!(r.w >= 44 && r.h >= 44, "{r:?}");
        }
    }

    /// Write an ARGB8888 buffer as a PNG.
    pub(crate) fn save_png(path: &std::path::Path, data: &[u8], pw: usize, ph: usize) {
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut encoder = png::Encoder::new(file, pw as u32, ph as u32);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        let rgb: Vec<u8> = data.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect();
        writer.write_image_data(&rgb).unwrap();
    }

    /// Renders of the lock screen's states over real pictures, for
    /// looking at: `HELIX_LOCK_RENDER_DIR=dir cargo test -p helix-shell
    /// lock::tests::gallery -- --ignored`.
    #[test]
    #[ignore]
    fn gallery() {
        let Some(dir) = std::env::var_os("HELIX_LOCK_RENDER_DIR").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let picture = |name: &str| -> crate::wallpaper::Background {
            let image = crate::wallpaper::decode(std::path::Path::new(&format!("/usr/share/backgrounds/{name}"))).ok().map(std::sync::Arc::new);
            crate::wallpaper::Background { image, ..Default::default() }
        };
        let faces = (Text::load(), Text::load_matching("sans-serif:bold"), helix_draw::screen::display_face());
        let fonts = LockFonts { text: faces.0.as_ref(), bold: faces.1.as_ref(), display: faces.2.as_ref() };
        let icon = |name: &str| crate::icons::symbolic(name, 40, [0xED, 0xED, 0xEF]);
        let themed = |name: &str| crate::icons::load(&crate::icons::IconSource::Names(vec![name.to_owned()]), 48);
        let apps = ["internet-chat", "internet-mail", "x-office-calendar", "system-software-update", "system-file-manager"].map(themed).to_vec();
        let cover = crate::media_art::fetch("file:///usr/share/backgrounds/warty-final-ubuntu.png");
        let media = [icon("media-skip-backward-symbolic"), icon("media-playback-start-symbolic"), icon("media-skip-forward-symbolic")];
        let small = |name: &str| crate::icons::symbolic(name, 20, [0xED, 0xED, 0xEF]);
        let bottom = || (small("system-reboot-symbolic"), small("system-shutdown-symbolic"), small("system-switch-user-symbolic"));
        let (restart, power_off, switch_user) = bottom();
        let icons = LockIcons { media: media.clone(), art: cover, art_is_cover: true, apps: apps.clone(), restart, power_off, switch_user };
        let (restart, power_off, switch_user) = bottom();
        let icons_no_art = LockIcons { media, art: icon("audio-x-generic-symbolic"), art_is_cover: false, apps, restart, power_off, switch_user };
        let player = Player { identity: "Music".into(), title: "Clair de Lune".into(), artist: "Claude Debussy".into(), ..Default::default() };
        let app = |name: &str, count: usize, details: &[(&str, &str)]| LockApp { name: name.into(), app_id: String::new(), count, details: details.iter().map(|(s, b)| ((*s).to_owned(), (*b).to_owned())).collect() };
        let counts = vec![app("Chat", 3, &[("Alice", "are we still on for lunch?"), ("Bob", "sent a photo")]), app("Mail", 1, &[])];
        let busy = vec![
            app("Chat", 7, &[("Alice", "are we still on for lunch?"), ("Bob", "sent a photo")]),
            app("Mail", 4, &[("Invoice 2041", "Your invoice is ready"), ("Team", "Weekly notes")]),
            app("Calendar", 2, &[("Standup", "in 10 minutes"), ("Review", "at 15:00")]),
            app("Updates", 1, &[("Software", "3 updates available")]),
            app("Files", 1, &[("Copy finished", "12 items")]),
        ];
        let blockers = vec![("Text Editor".to_owned(), "Unsaved document".to_owned()), ("Terminal".to_owned(), String::new())];
        let face = crate::user_identity::load_picture(std::path::Path::new("/usr/share/backgrounds/mendhak-Red_Acer.jpg"));
        let base = LockModel { restart: true, power_off: true, switch_user: true, ..model(&icons) };
        /// (file name, picture, (width, height, scale), what it shows)
        type Scene<'a> = (&'a str, &'a str, (usize, usize, f64), LockModel<'a>);
        let scenes: Vec<Scene> = vec![
            ("01-prompt", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { ..base }),
            ("02-prompt-photo", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { ..base }),
            ("03-typing", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { typed: 13, ..base }),
            ("04-checking", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { typed: 13, checking: true, spinner: Some(1.0), message: Some("Checking…"), ..base }),
            ("04b-typing-long", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 2.0), LockModel { typed: 37, ..base }),
            ("05-wrong-photo", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { message: Some("Incorrect password. Try again."), failed: true, ..base }),
            ("06-media-notes", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { typed: 4, media: Some(&player), counts: &counts, ..base }),
            ("07-media-photo", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { media: Some(&player), counts: &counts, icons: &icons_no_art, ..base }),
            ("08-confirm", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { confirming: Some(PowerAction::PowerOff), blockers: &blockers, ..base }),
            ("08b-confirm-plain", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { confirming: Some(PowerAction::Restart), focus: Some(LockHit::Power(LockPowerHit::Confirm)), ..base }),
            ("08c-buttons-states", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { hover: Some(LockHit::Power(LockPowerHit::Ask(PowerAction::Restart))), focus: Some(LockHit::Power(LockPowerHit::Ask(PowerAction::PowerOff))), pressed: Some(LockHit::SwitchUser), ..base }),
            ("09-720p-busy", "ubuntu-wallpaper-d.png", (1280, 720, 1.0), LockModel { media: Some(&player), counts: &busy, ..base }),
            ("10-scale-1.5", "mendhak-Red_Acer.jpg", (1280, 720, 1.5), LockModel { typed: 3, media: Some(&player), counts: &counts, clock: "2:05", suffix: Some("PM"), ..base }),
            ("11-portrait", "manuej-Vor.jpg", (1080, 1920, 1.0), LockModel { media: Some(&player), counts: &counts, ..base }),
            ("11b-1366-125", "mendhak-Red_Acer.jpg", (1093, 614, 1.25), LockModel { media: Some(&player), counts: &busy, ..base }),
            ("11c-1280x800-15", "ubuntu-wallpaper-d.png", (853, 533, 1.5), LockModel { media: Some(&player), counts: &counts, ..base }),
            ("11d-3x2", "ubuntu-wallpaper-d.png", (1440, 960, 1.0), LockModel { media: Some(&player), counts: &counts, ..base }),
            ("16-resting", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { revealed: false, reveal_pos: 0.0, reveal_alpha: 0.0, media: Some(&player), counts: &counts, ..base }),
            ("16b-resting-720", "ubuntu-wallpaper-d.png", (1280, 720, 1.0), LockModel { revealed: false, reveal_pos: 0.0, reveal_alpha: 0.0, media: Some(&player), counts: &busy, ..base }),
            ("16c-revealing", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { revealed: true, reveal_pos: 0.5, reveal_alpha: 0.5, typed: 1, media: Some(&player), counts: &counts, ..base }),
            ("17-fading-in", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { revealed: false, reveal_pos: 0.0, reveal_alpha: 0.0, alpha: 0.4, slide: 7.0, media: Some(&player), counts: &counts, ..base }),
            ("15-caps-lock", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { typed: 6, caps_lock: true, layout: Some("en"), ..base }),
            ("15b-caps-wrong", "ubuntu-wallpaper-d.png", (1280, 720, 1.0), LockModel { caps_lock: true, layout: Some("de"), message: Some("Incorrect password. Try again."), failed: true, media: Some(&player), counts: &counts, ..base }),
            ("14-second-monitor", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { prompt: false, typed: 4, media: Some(&player), counts: &counts, ..base }),
            ("12-high-contrast", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { typed: 4, media: Some(&player), counts: &counts, ..base }),
            ("12b-high-contrast-wrong", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { message: Some("Incorrect password. Try again."), failed: true, ..base }),
            ("05b-checking-still", "mendhak-Bluebells_Suspended_In_Time.jpg", (1920, 1080, 1.0), LockModel { typed: 8, checking: true, spinner: None, message: Some("Checking…"), ..base }),
            ("05c-unavailable", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { message: Some("Cannot check password: the authentication service did not answer in time"), failed: true, ..base }),
            ("13-avatar-picture", "ubuntu-wallpaper-d.png", (1920, 1080, 1.0), LockModel { picture: face.as_ref(), ..base }),
        ];
        for (name, picture_name, (w, h, scale), m) in &scenes {
            // (Global: run the gallery on its own.)
            helix_draw::set_high_contrast(name.contains("high-contrast"));
            let (pw, ph) = (physical(*w, *scale), physical(*h, *scale));
            let mut data = backdrop(&picture(picture_name), (*w, *h), *scale);
            let mut canvas = Canvas::scaled(&mut data, *w, *h, *scale);
            paint(&mut canvas, &fonts, m, *w, *h);
            save_png(&dir.join(format!("{name}.png")), &data, pw, ph);
        }
        // The user's accent (and high contrast, which ignores it).
        use helix_draw::accent::Accent;
        for (accent, hc) in [(Accent::Blue, false), (Accent::Orange, false), (Accent::Yellow, false), (Accent::Slate, false), (Accent::Yellow, true)] {
            helix_draw::accent::set(accent);
            helix_draw::set_high_contrast(hc);
            let shown = [
                ("typing", LockModel { typed: 6, ..base }),
                ("go-hover", LockModel { typed: 6, hover: Some(LockHit::Unlock), ..base }),
                ("confirm-focus", LockModel { confirming: Some(PowerAction::Restart), focus: Some(LockHit::Power(LockPowerHit::Cancel)), ..base }),
            ];
            for (what, m) in &shown {
                let name = format!("20-accent-{}{}-{what}", accent.name(), if hc { "-hc" } else { "" });
                let (w, h, scale) = (1280, 720, 1.0);
                let (pw, ph) = (physical(w, scale), physical(h, scale));
                let mut data = backdrop(&picture("ubuntu-wallpaper-d.png"), (w, h), scale);
                let mut canvas = Canvas::scaled(&mut data, w, h, scale);
                paint(&mut canvas, &fonts, m, w, h);
                save_png(&dir.join(format!("{name}.png")), &data, pw, ph);
            }
        }
        helix_draw::accent::set(Accent::Blue);
        helix_draw::set_high_contrast(false);
    }
}
