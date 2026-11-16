//! Screenshot primitives for the D-Bus side (`crate::screenshot`):
//! captures through the compositor's `helix_screenshot_v1`, the picker
//! (area, point or the screenshot UI) on every monitor, and the flash.

use std::fs::File;
use std::os::fd::AsFd;
use std::sync::Mutex;

use helix_protocols::client::helix_screenshot_frame_v1::{self, HelixScreenshotFrameV1, Reason};
use helix_protocols::client::helix_screenshot_manager_v1::{Cursor, HelixScreenshotManagerV1};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use tokio::sync::oneshot;

use super::*;
use crate::screenshot::{CaptureError, Job, PickMode, Picked, Rect as Area, Shot, Target};
use helix_i18n::tr;

const BTN_RIGHT: u32 = 0x111;
/// Drags smaller than this (logical pixels) are taken as clicks.
const MIN_DRAG: f64 = 2.0;
const FLASH_STEPS: u32 = 6;
const FLASH_STEP: Duration = Duration::from_millis(40);
const PICK_SCRIM: u32 = 0x80000000;

/// One monitor's picker surface.
pub(super) struct PickView {
    overlay: Overlay<()>,
    /// The output's position in the layout.
    origin: (i32, i32),
}

struct Drag {
    view: usize,
    start: (f64, f64),
    now: (f64, f64),
}

/// The picker while open.
pub(super) struct Picker {
    mode: PickMode,
    reply: Option<oneshot::Sender<Picked>>,
    views: Vec<PickView>,
    drag: Option<Drag>,
    /// The screenshot UI's state (`PickMode::Ui`); its selection is also
    /// the area picker's while the keyboard makes it.
    shot: ShotState,
    /// The point the arrow keys move (point and window pickers, the
    /// screenshot UI's Window mode): view, place in it.
    point: Option<(usize, (f64, f64))>,
    /// The arrow keys made the selection or the point: it is announced.
    keyboard: bool,
}

/// What the screenshot UI captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShotMode {
    Area,
    Screen,
    Window,
}

/// A toolbar button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Mode(ShotMode),
    Record(bool),
    Pointer,
    Capture,
    /// Close (cancel), as Escape.
    Close,
}

/// A part of the area selection the pointer can take: a corner or edge
/// handle (by the sides it moves) or the inside (moving it whole).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Grip {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Grip {
    const MOVE: Grip = Grip { left: false, right: false, top: false, bottom: false };

    /// The eight handles, with their places as fractions of the selection.
    fn handles() -> [(Grip, (f64, f64)); 8] {
        let g = |left, right, top, bottom| Grip { left, right, top, bottom };
        [
            (g(true, false, true, false), (0.0, 0.0)),
            (g(false, false, true, false), (0.5, 0.0)),
            (g(false, true, true, false), (1.0, 0.0)),
            (g(false, true, false, false), (1.0, 0.5)),
            (g(false, true, false, true), (1.0, 1.0)),
            (g(false, false, false, true), (0.5, 1.0)),
            (g(true, false, false, true), (0.0, 1.0)),
            (g(true, false, false, false), (0.0, 0.5)),
        ]
    }
}

/// How far from a handle (logical px) a press still takes it.
const GRIP_REACH: f64 = 12.0;
/// A handle's radius.
const HANDLE_R: f64 = 6.0;

/// What a press at `p` on selection `sel` takes: a handle, the inside,
/// or nothing (outside: a new selection).
pub(super) fn grip_at(sel: (f64, f64, f64, f64), p: (f64, f64)) -> Option<Grip> {
    let (x, y, w, h) = sel;
    let near = Grip::handles().into_iter().find(|(_, (fx, fy))| (p.0 - (x + fx * w)).hypot(p.1 - (y + fy * h)) <= GRIP_REACH).map(|(g, _)| g);
    near.or_else(|| (p.0 >= x && p.0 < x + w && p.1 >= y && p.1 < y + h).then_some(Grip::MOVE))
}

/// Selection `sel` dragged by `grip` by (dx, dy), kept on a `bounds` (w, h)
/// view and at least 1 px: moved whole, or its taken sides moved (past
/// the opposite side it flips).
pub(super) fn grip_drag(sel: (f64, f64, f64, f64), grip: Grip, (dx, dy): (f64, f64), bounds: (f64, f64)) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = sel;
    if grip == Grip::MOVE {
        return ((x + dx).clamp(0.0, (bounds.0 - w).max(0.0)), (y + dy).clamp(0.0, (bounds.1 - h).max(0.0)), w, h);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
    if grip.left {
        x0 += dx;
    }
    if grip.right {
        x1 += dx;
    }
    if grip.top {
        y0 += dy;
    }
    if grip.bottom {
        y1 += dy;
    }
    let clamp = |v: f64, max: f64| v.clamp(0.0, max);
    let (x0, x1) = (clamp(x0.min(x1), bounds.0), clamp(x0.max(x1), bounds.0));
    let (y0, y1) = (clamp(y0.min(y1), bounds.1), clamp(y0.max(y1), bounds.1));
    (x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0))
}

/// How far an arrow key moves the area or point (logical px): Shift for
/// larger steps, Alt for single pixels.
pub(super) fn key_step(shift: bool, alt: bool) -> f64 {
    if alt {
        1.0
    } else if shift {
        50.0
    } else {
        10.0
    }
}

/// The smallest area the arrow keys make (logical px a side).
const MIN_KEY_SIZE: f64 = 10.0;

/// Area `sel` after an arrow key, kept on a `bounds` (w, h) view: moved
/// by (dx, dy), or with `resize` (Ctrl) its right and bottom edges moved
/// (Right and Down grow it, Left and Up shrink it).
pub(super) fn nudge_area(sel: (f64, f64, f64, f64), (dx, dy): (f64, f64), resize: bool, bounds: (f64, f64)) -> (f64, f64, f64, f64) {
    if !resize {
        return grip_drag(sel, Grip::MOVE, (dx, dy), bounds);
    }
    let (x, y, w, h) = sel;
    let size = |v: f64, room: f64| v.min(room).max(MIN_KEY_SIZE.min(room)).max(1.0);
    (x, y, size(w + dx, bounds.0 - x), size(h + dy, bounds.1 - y))
}

/// Point `p` after an arrow key moving it by (dx, dy), kept on a
/// `bounds` (w, h) view.
pub(super) fn nudge_point(p: (f64, f64), (dx, dy): (f64, f64), bounds: (f64, f64)) -> (f64, f64) {
    ((p.0 + dx).clamp(0.0, (bounds.0 - 1.0).max(0.0)), (p.1 + dy).clamp(0.0, (bounds.1 - 1.0).max(0.0)))
}

/// The area the arrow keys start with: the middle quarter of a `bounds`
/// view.
fn first_area(bounds: (f64, f64)) -> (f64, f64, f64, f64) {
    ((bounds.0 / 4.0).round(), (bounds.1 / 4.0).round(), (bounds.0 / 2.0).round().max(1.0), (bounds.1 / 2.0).round().max(1.0))
}

/// What the keyboard's selection is, said on the accessibility bus as
/// it changes: an area (`Some` size) or a point, in layout coordinates.
pub(super) fn keyboard_said(at: (i32, i32), size: Option<(i32, i32)>) -> String {
    match size {
        Some((w, h)) => helix_i18n::trf!("Area at {x}, {y}: {w} × {h}", x = at.0, y = at.1, w = w, h = h),
        None => helix_i18n::trf!("Point at {x}, {y}", x = at.0, y = at.1),
    }
}

/// A selection being moved or resized: on which view, by what, from
/// where the pointer was pressed, and the selection then.
struct Edit {
    view: usize,
    grip: Grip,
    from: (f64, f64),
    start: (f64, f64, f64, f64),
}

struct ShotState {
    mode: ShotMode,
    record: bool,
    allow_record: bool,
    pointer: bool,
    /// The selected rectangle: (view, x, y, w, h in that view).
    selection: Option<(usize, (f64, f64, f64, f64))>,
    /// The selected window: a point on it (layout).
    window: Option<(i32, i32)>,
    /// Where the pointer is (layout), for the window outline.
    hover: Option<(i32, i32)>,
    /// The toolbar's buttons (on view 0).
    tools: Vec<(Rect, Tool)>,
    /// The toolbar button under the pointer, and the one pressed.
    tool_hover: Option<Tool>,
    tool_pressed: Option<Tool>,
    /// The selection being moved or resized.
    edit: Option<Edit>,
}

impl ShotState {
    fn new(record: bool, allow_record: bool) -> Self {
        Self { mode: ShotMode::Area, record: record && allow_record, allow_record, pointer: false, selection: None, window: None, hover: None, tools: Vec::new(), tool_hover: None, tool_pressed: None, edit: None }
    }
}

/// The toolbar: its toggles' size, the gap between them and groups, its
/// padding, the capture button, and how far above the screen's bottom.
const TOOL_H: i32 = 40;
const TOOL_W: i32 = 48;
const TOOL_GAP: i32 = 4;
const GROUP_GAP: i32 = 16;
const TOOL_PAD: i32 = 10;
const CAPTURE_D: i32 = 52;
const TOOLBAR_BOTTOM: i32 = 40;

/// A fading white rectangle on one monitor.
pub(super) struct Flash {
    overlay: Overlay<()>,
    /// The flashed part, in surface coordinates.
    local: Rect,
    step: u32,
}

/// What a capture's frame object carries until it is answered.
pub struct FrameData {
    reply: Mutex<Option<oneshot::Sender<Result<Area, CaptureError>>>>,
    /// Kept open until the compositor answered.
    _file: File,
}

fn hint(mode: PickMode) -> &'static str {
    match mode {
        PickMode::Area => tr("Drag, or arrows to move and Ctrl+arrows to resize · Enter to confirm · Esc to cancel"),
        PickMode::Point => tr("Click, or arrows and Enter, to pick a color · Esc to cancel"),
        PickMode::Window => tr("Click a window, or arrows and Enter, to capture it · Esc to cancel"),
        PickMode::Ui { .. } => tr("S, C, W: area, screen, window · V: record · P: pointer · Enter: capture · Esc: cancel"),
    }
}

/// A tool bar button of the screenshot UI on the accessibility bus: the
/// modes (and Screenshot / Record) are radio buttons, checked when chosen;
/// "Show Pointer" a toggle button; Capture a button.
fn tool_node(name: &str, tool: Tool, on: bool, enabled: bool, r: Rect) -> crate::a11y::Node {
    use crate::a11y::{Node, Role};
    let node = match tool {
        Tool::Mode(_) | Tool::Record(_) => Node::new(Role::RadioButton, name).toggled(on),
        Tool::Pointer => Node::new(Role::Button, name).toggled(on),
        Tool::Capture | Tool::Close => Node::new(Role::Button, name),
    };
    node.id(format!("{tool:?}")).at(r.into()).click().disabled(!enabled)
}

/// A recording tool bar (Area chosen, Window off while recording, the
/// pointer shown), for the accessibility tests.
#[cfg(test)]
pub(super) fn a11y_sample_tools() -> Vec<crate::a11y::Node> {
    let r = Rect { x: 0, y: 0, w: 80, h: TOOL_H };
    let tools = vec![
        tool_node("Area", Tool::Mode(ShotMode::Area), true, true, r),
        tool_node("Screen", Tool::Mode(ShotMode::Screen), false, true, r),
        tool_node("Window", Tool::Mode(ShotMode::Window), false, false, r),
        tool_node("Screenshot", Tool::Record(false), false, true, r),
        tool_node("Record", Tool::Record(true), true, true, r),
        tool_node("Show Pointer", Tool::Pointer, true, true, r),
        tool_node("Start Recording", Tool::Capture, true, true, r),
        tool_node("Close", Tool::Close, true, true, r),
    ];
    vec![crate::a11y::Node::new(crate::a11y::Role::Toolbar, "Screenshot").id("tools").children(tools)]
}

/// Outline the selection (x, y, w, h) in the accent; with `handles`, its
/// eight round handles; with `scale` (the view's), its size in pixels in
/// a small pill under it (above it at the screen's bottom).
fn paint_selection(canvas: &mut Canvas, text: Option<&crate::text::Text>, (x, y, w, h): (i32, i32, i32, i32), handles: bool, scale: Option<f64>) {
    let (fx, fy, fw, fh) = (f64::from(x), f64::from(y), f64::from(w), f64::from(h));
    canvas.round_outline(fx, fy, fw, fh, 0.0, 2.0, theme::focus());
    if handles {
        for (_, (hx, hy)) in Grip::handles() {
            let (cx, cy) = (fx + hx * fw, fy + hy * fh);
            canvas.circle(cx, cy, HANDLE_R + 1.0, if helix_draw::high_contrast() { 0xFF00_0000 } else { 0x66000000 });
            canvas.circle(cx, cy, HANDLE_R, 0xFFFF_FFFF);
        }
    }
    let Some(scale) = scale else { return };
    let words = helix_i18n::trf!("{w} × {h}", w = (fw * scale).round(), h = (fh * scale).round());
    let tw = text.map_or(60.0, |t| t.width(&words, size::CAPTION)) as i32;
    let (pw, ph) = (tw + 20, helix_draw::theme::grow(24));
    let below = y + h + 12;
    let py = if below + ph + 8 > (canvas.height as f64 / canvas.scale) as i32 { y - ph - 12 } else { below };
    let px = x + w / 2 - pw / 2;
    theme::popover_round(canvas, f64::from(px), f64::from(py), f64::from(pw), f64::from(ph), f64::from(ph) / 2.0);
    label(text, canvas, &words, (px + 10) as f32, baseline(text, py, ph, size::CAPTION), size::CAPTION, FG, tw as f32 + 1.0);
}

/// A toolbar button's name, the key that does the same, and its look.
struct ToolButton {
    name: String,
    key: &'static str,
    tool: Tool,
    on: bool,
    enabled: bool,
}

/// The screenshot UI's toolbar on a `w`×`h` view: a rounded panel near the
/// bottom with icon toggles in groups (area, screen, window; screenshot,
/// record; show pointer), the round capture button (white for a
/// screenshot, red for a recording) and a close button; under the pointer
/// a button is lit (pressed: darker) with a tooltip naming it and its
/// key. Returns the buttons that can be clicked and the toolbar for the
/// accessibility bus.
fn paint_toolbar(canvas: &mut Canvas, text: Option<&crate::text::Text>, (w, h): (i32, i32), st: &ShotState) -> (Vec<(Rect, Tool)>, crate::a11y::Node) {
    let button = |name: &str, key, tool, on, enabled| ToolButton { name: name.into(), key, tool, on, enabled };
    let mut groups: Vec<Vec<ToolButton>> = vec![vec![
        button(tr("Area"), "S", Tool::Mode(ShotMode::Area), st.mode == ShotMode::Area, true),
        button(tr("Screen"), "C", Tool::Mode(ShotMode::Screen), st.mode == ShotMode::Screen, true),
        button(tr("Window"), "W", Tool::Mode(ShotMode::Window), st.mode == ShotMode::Window, !st.record),
    ]];
    if st.allow_record {
        groups.push(vec![button(tr("Screenshot"), "V", Tool::Record(false), !st.record, true), button(tr("Record"), "V", Tool::Record(true), st.record, true)]);
    }
    groups.push(vec![button(tr("Show Pointer"), "P", Tool::Pointer, st.pointer, true)]);
    let toggles: i32 = groups.iter().map(|g| g.len() as i32).sum();
    let gaps: i32 = groups.iter().map(|g| (g.len() as i32 - 1) * TOOL_GAP).sum::<i32>() + (groups.len() as i32) * GROUP_GAP;
    let close_d = TOOL_H;
    let total = TOOL_PAD + toggles * TOOL_W + gaps + CAPTURE_D + GROUP_GAP + close_d + TOOL_PAD;
    let bar_h = CAPTURE_D + 2 * TOOL_PAD;
    let (tx, ty) = ((w - total) / 2, h - bar_h - TOOLBAR_BOTTOM);
    theme::popover_round(canvas, f64::from(tx), f64::from(ty), f64::from(total), f64::from(bar_h), f64::from(bar_h) / 2.0);
    let look = |tool: Tool, on: bool| theme::Look { on, hovered: st.tool_hover == Some(tool), pressed: st.tool_pressed == Some(tool) };
    let mut x = tx + TOOL_PAD;
    let mut placed = Vec::new();
    let mut a11y = Vec::new();
    let mut tip: Option<(Rect, String)> = None;
    let cy = ty + bar_h / 2;
    for group in &groups {
        for (i, b) in group.iter().enumerate() {
            if i > 0 {
                x += TOOL_GAP;
            }
            let r = Rect { x, y: cy - TOOL_H / 2, w: TOOL_W, h: TOOL_H };
            let l = look(b.tool, b.on);
            let rect = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
            theme::flat_toggle(canvas, rect, theme::RADIUS_BUTTON + 2.0, l);
            let fg = if b.enabled { theme::control_text(l) } else { theme::disabled_text() };
            tool_symbol(canvas, b.tool, f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(cy), fg);
            if b.enabled {
                placed.push((r, b.tool));
            }
            if st.tool_hover == Some(b.tool) {
                tip = Some((r, helix_i18n::trf!("{name} ({key})", name = b.name, key = b.key)));
            }
            a11y.push(tool_node(&b.name, b.tool, b.on, b.enabled, r).describe(helix_i18n::trf!("Key: {key}", key = b.key)));
            x += TOOL_W;
        }
        x += GROUP_GAP;
    }
    // The capture button: a white disc in a ring, red while recording.
    let r = Rect { x, y: cy - CAPTURE_D / 2, w: CAPTURE_D, h: CAPTURE_D };
    let (ccx, ccy, cr) = (f64::from(r.x) + f64::from(CAPTURE_D) / 2.0, f64::from(cy), f64::from(CAPTURE_D) / 2.0);
    let held = st.tool_pressed == Some(Tool::Capture);
    let lit = st.tool_hover == Some(Tool::Capture);
    canvas.ring(ccx, ccy, cr - 2.0, 3.0, if helix_draw::high_contrast() { 0xFFFF_FFFF } else { 0xE6FF_FFFF });
    let disc = if st.record { theme::ERROR } else { 0xFFFF_FFFF };
    // Under the pointer a little see-through, pressed more.
    let alpha: u32 = if held { 0xB3 } else if lit { 0xE0 } else { 0xFF };
    canvas.circle(ccx, ccy, cr - 7.0, (alpha << 24) | (disc & 0x00FF_FFFF));
    let capture_name = if st.record { tr("Start Recording") } else { tr("Capture") };
    placed.push((r, Tool::Capture));
    if lit {
        tip = Some((r, helix_i18n::trf!("{name} ({key})", name = capture_name, key = tr("Enter"))));
    }
    a11y.push(tool_node(capture_name, Tool::Capture, true, true, r).describe(helix_i18n::trf!("Key: {key}", key = tr("Enter"))));
    x += CAPTURE_D + GROUP_GAP;
    // Close.
    let r = Rect { x, y: cy - close_d / 2, w: close_d, h: close_d };
    let (ccx, ccy) = (f64::from(r.x) + f64::from(close_d) / 2.0, f64::from(cy));
    theme::round_control(canvas, ccx, ccy, f64::from(close_d), look(Tool::Close, false), false);
    theme::close_symbol(canvas, ccx, ccy, 11.0, FG);
    placed.push((r, Tool::Close));
    if st.tool_hover == Some(Tool::Close) {
        tip = Some((r, helix_i18n::trf!("{name} ({key})", name = tr("Close"), key = tr("Esc"))));
    }
    a11y.push(tool_node(tr("Close"), Tool::Close, false, true, r).describe(helix_i18n::trf!("Key: {key}", key = tr("Esc"))));
    // The tooltip above the button under the pointer.
    if let Some((r, words)) = tip {
        let tw = text.map_or(80.0, |t| t.width(&words, size::LABEL)) as i32;
        let (pw, ph) = (tw + 20, helix_draw::theme::grow(30));
        let (px, py) = ((r.x + r.w / 2 - pw / 2).clamp(4, (w - pw - 4).max(4)), ty - ph - 8);
        theme::tooltip(canvas, px, py, pw, ph);
        label(text, canvas, &words, (px + 10) as f32, baseline(text, py, ph, size::LABEL), size::LABEL, FG, tw as f32 + 1.0);
    }
    (placed, crate::a11y::Node::new(crate::a11y::Role::Toolbar, tr("Screenshot")).id("tools").children(a11y))
}

/// A toolbar button's symbol (Helix's own drawings), centered at (cx, cy).
fn tool_symbol(canvas: &mut Canvas, tool: Tool, cx: f64, cy: f64, color: u32) {
    let line = |canvas: &mut Canvas, a: (f64, f64), b: (f64, f64)| canvas.line((cx + a.0, cy + a.1), (cx + b.0, cy + b.1), 1.8, color);
    match tool {
        // Area: the corners of a selection.
        Tool::Mode(ShotMode::Area) => {
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let (x, y) = (sx * 8.0, sy * 6.5);
                line(canvas, (x, y), (x - sx * 4.5, y));
                line(canvas, (x, y), (x, y - sy * 4.0));
            }
        }
        // Screen: a monitor on its stand.
        Tool::Mode(ShotMode::Screen) => {
            canvas.round_outline(cx - 8.0, cy - 7.0, 16.0, 11.0, 2.0, 1.6, color);
            line(canvas, (0.0, 5.5), (0.0, 8.0));
            line(canvas, (-4.0, 8.5), (4.0, 8.5));
        }
        // Window: a window with its title bar.
        Tool::Mode(ShotMode::Window) => {
            canvas.round_outline(cx - 8.0, cy - 7.0, 16.0, 14.0, 2.5, 1.6, color);
            canvas.rect((cx - 8.0) as i32, (cy - 6.0) as i32, 16, 3, color);
        }
        // Screenshot: a camera.
        Tool::Record(false) => {
            canvas.round_outline(cx - 8.5, cy - 5.0, 17.0, 12.0, 2.5, 1.6, color);
            canvas.round_rect(cx - 3.5, cy - 8.0, 7.0, 3.5, 1.0, color);
            canvas.ring(cx, cy + 1.0, 3.2, 1.6, color);
        }
        // Record: a video camera.
        Tool::Record(true) => {
            canvas.round_outline(cx - 9.0, cy - 5.0, 12.0, 10.0, 2.0, 1.6, color);
            line(canvas, (5.0, -1.5), (9.0, -4.5));
            line(canvas, (9.0, -4.5), (9.0, 4.5));
            line(canvas, (9.0, 4.5), (5.0, 1.5));
        }
        // Show pointer: an arrow.
        Tool::Pointer => {
            let p = [(-4.0, -8.0), (-4.0, 6.0), (-0.5, 2.5), (2.5, 8.5), (4.5, 7.5), (1.5, 1.5), (6.0, 1.5)];
            for k in 0..p.len() {
                line(canvas, p[k], p[(k + 1) % p.len()]);
            }
        }
        Tool::Capture | Tool::Close => {}
    }
}

/// The screenshot UI on one monitor for the gallery: an area selected
/// (with its handles and size), the toolbar (recording when `record`)
/// with the pointer on `hover` (0: none, 1: Window, 2: capture, 3: close)
/// and Area pressed when `pressed`.
#[cfg(test)]
pub(super) fn gallery_scene(canvas: &mut Canvas, text: Option<&crate::text::Text>, (w, h): (i32, i32), record: bool, hover: u8, pressed: bool) {
    let mut st = ShotState::new(record, true);
    st.tool_hover = match hover {
        1 => Some(Tool::Mode(ShotMode::Window)),
        2 => Some(Tool::Capture),
        3 => Some(Tool::Close),
        _ => None,
    };
    st.tool_pressed = pressed.then_some(Tool::Mode(ShotMode::Area));
    let (x, y, sw, sh) = (w / 4, h / 5, w / 2, h / 2);
    canvas.rect(0, 0, w, y, PICK_SCRIM);
    canvas.rect(0, y + sh, w, h - y - sh, PICK_SCRIM);
    canvas.rect(0, y, x, sh, PICK_SCRIM);
    canvas.rect(x + sw, y, w - x - sw, sh, PICK_SCRIM);
    paint_selection(canvas, text, (x, y, sw, sh), true, Some(canvas.scale));
    paint_toolbar(canvas, text, (w, h), &st);
}

/// The rectangle between two points.
fn span(a: (f64, f64), b: (f64, f64)) -> (f64, f64, f64, f64) {
    (a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
}

impl Ui {
    pub(super) fn screenshot_job(&mut self, job: Job) {
        match job {
            Job::Capture { target, cursor, file, reply } => self.capture(target, cursor, file, reply),
            Job::Pick { mode, reply } => self.open_picker(mode, reply),
            Job::Flash(area) => self.flash(area),
            Job::Copy(file) => match self.screenshots.as_ref().filter(|m| m.version() >= 2) {
                Some(manager) => manager.copy_image(file.as_fd()),
                None => tracing::info!("compositor cannot take the clipboard; screenshot not copied"),
            },
        }
    }

    // ----- Captures ---------------------------------------------------------

    fn capture(&mut self, target: Target, cursor: bool, file: File, reply: oneshot::Sender<Result<Area, CaptureError>>) {
        let Some(manager) = &self.screenshots else {
            let _ = reply.send(Err(CaptureError::Unavailable));
            return;
        };
        if self.lock.session.is_some() {
            let _ = reply.send(Err(CaptureError::Locked));
            return;
        }
        let fd = match file.try_clone() {
            Ok(f) => f,
            Err(_) => {
                let _ = reply.send(Err(CaptureError::Failed));
                return;
            }
        };
        let cursor = if cursor { Cursor::Shown } else { Cursor::Hidden };
        let data = FrameData { reply: Mutex::new(Some(reply)), _file: file };
        let qh = &self.qh;
        if matches!(target, Target::WindowAt(..)) && manager.version() < 4 {
            if let Some(reply) = data.reply.lock().unwrap().take() {
                let _ = reply.send(Err(CaptureError::Unavailable));
            }
            return;
        }
        match target {
            Target::Screen => manager.capture_screen(cursor, fd.as_fd(), qh, data),
            Target::Window { frame: true } if manager.version() >= 3 => manager.capture_window_with_frame(cursor, fd.as_fd(), qh, data),
            Target::Window { .. } => manager.capture_window(cursor, fd.as_fd(), qh, data),
            Target::Area(a) => manager.capture_area(a.x, a.y, a.width, a.height, cursor, fd.as_fd(), qh, data),
            Target::WindowAt(x, y) => manager.capture_window_at(x, y, cursor, fd.as_fd(), qh, data),
        };
    }

    // ----- Picker -----------------------------------------------------------

    fn output_origin(&self, output: &WlOutput) -> Option<(i32, i32)> {
        self.output_state.info(output).and_then(|i| i.logical_position)
    }

    fn open_picker(&mut self, mode: PickMode, reply: oneshot::Sender<Picked>) {
        if self.picker.is_some() || self.lock.session.is_some() {
            let _ = reply.send(Picked::Busy);
            return;
        }
        self.close_launcher(true);
        self.power_menu = None;
        self.finish_switching(false);
        let mut views = Vec::new();
        for output in self.output_state.outputs() {
            let Some(origin) = self.output_origin(&output) else { continue };
            // One surface takes the keyboard.
            let keyboard = if views.is_empty() { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None };
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-screenshot".to_owned()), Some(&output));
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(keyboard);
            layer.set_size(0, 0);
            layer.commit();
            let overlay = Overlay { layer, width: 0, height: 0, configured: false, hits: Vec::new(), state: () };
            views.push(PickView { overlay, origin });
        }
        if views.is_empty() {
            let _ = reply.send(Picked::Cancelled);
            return;
        }
        tracing::debug!(?mode, monitors = views.len(), "picker open");
        let shot = match mode {
            PickMode::Ui { record, allow_record } => ShotState::new(record, allow_record),
            _ => ShotState::new(false, false),
        };
        self.picker = Some(Picker { mode, reply: Some(reply), views, drag: None, shot, point: None, keyboard: false });
    }

    /// Close the picker with `result`; its surfaces are gone before the
    /// answer goes out, so they are not in a capture that follows.
    pub(super) fn finish_pick(&mut self, result: Picked) {
        let Some(mut picker) = self.picker.take() else { return };
        let reply = picker.reply.take();
        drop(picker);
        tracing::debug!(?result, "picker closed");
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    pub(super) fn picker_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(picker) = self.picker.as_mut() else { return false };
        let Some(index) = picker.views.iter().position(|v| &v.overlay.layer == layer) else { return false };
        let overlay = &mut picker.views[index].overlay;
        overlay.width = size.0;
        overlay.height = size.1;
        overlay.configured = true;
        self.draw_pick_view(index);
        true
    }

    pub(super) fn picker_closed(&mut self, layer: &LayerSurface) -> bool {
        let owns = self.picker.as_ref().is_some_and(|p| p.views.iter().any(|v| &v.overlay.layer == layer));
        if owns {
            self.finish_pick(Picked::Cancelled);
        }
        owns
    }

    fn draw_pick_view(&mut self, index: usize) {
        let Some(picker) = self.picker.as_ref() else { return };
        let view = &picker.views[index];
        let (w, h) = (view.overlay.width as usize, view.overlay.height as usize);
        if !view.overlay.configured || w == 0 || h == 0 {
            return;
        }
        let scale = self.scales.of(view.overlay.layer.wl_surface());
        let window_rect = picker.shot.window.or(picker.shot.hover).and_then(|(px, py)| self.window_rect_at(px, py));
        let Ok((buffer, data)) = self.buffers.create_buffer(view.overlay.layer.wl_surface(), Ui::redraw_picker, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        let is_ui = matches!(picker.mode, PickMode::Ui { .. });
        // The part left undimmed (and outlined), in this view.
        let clear: Option<(i32, i32, i32, i32)> = match (picker.drag.as_ref().filter(|d| d.view == index), is_ui, picker.shot.mode) {
            (Some(drag), _, _) => {
                let (x, y, sw, sh) = span(drag.start, drag.now);
                Some((x as i32, y as i32, sw.round() as i32, sh.round() as i32))
            }
            (None, true, ShotMode::Area) => picker.shot.selection.filter(|(v, _)| *v == index).map(|(_, (x, y, sw, sh))| (x as i32, y as i32, sw.round() as i32, sh.round() as i32)),
            (None, true, ShotMode::Window) => window_rect.map(|(x, y, ww, wh)| (x - view.origin.0, y - view.origin.1, ww, wh)),
            // The area picker's selection made with the arrow keys.
            (None, false, _) if picker.mode == PickMode::Area => picker.shot.selection.filter(|(v, _)| *v == index).map(|(_, (x, y, sw, sh))| (x as i32, y as i32, sw.round() as i32, sh.round() as i32)),
            _ => None,
        };
        // The point the arrow keys move (pickers of a point or a window).
        let key_point = picker.point.filter(|(v, _)| *v == index && (!is_ui || picker.shot.mode == ShotMode::Window)).map(|(_, (px, py))| (px as i32, py as i32));
        let screen_mode = is_ui && picker.shot.mode == ShotMode::Screen;
        match clear {
            _ if screen_mode => {
                for (rx, ry, rw, rh) in [(0, 0, w as i32, 2), (0, h as i32 - 2, w as i32, 2), (0, 0, 2, h as i32), (w as i32 - 2, 0, 2, h as i32)] {
                    canvas.rect(rx, ry, rw, rh, accent());
                }
            }
            Some((x, y, sw, sh)) => {
                canvas.rect(0, 0, w as i32, y.max(0), PICK_SCRIM);
                canvas.rect(0, (y + sh).max(0), w as i32, h as i32 - (y + sh).max(0), PICK_SCRIM);
                canvas.rect(0, y.max(0), x.max(0), sh.min(h as i32), PICK_SCRIM);
                canvas.rect((x + sw).max(0), y.max(0), w as i32 - (x + sw).max(0), sh.min(h as i32), PICK_SCRIM);
                // An area of the screenshot UI can be adjusted: handles, and
                // its size in pixels.
                let area = is_ui && picker.shot.mode == ShotMode::Area;
                paint_selection(&mut canvas, self.text.as_ref(), (x, y, sw, sh), area, if area || !is_ui { Some(scale) } else { None });
            }
            None => canvas.rect(0, 0, w as i32, h as i32, PICK_SCRIM),
        }
        if let Some((px, py)) = key_point {
            // A small outlined box around the point.
            paint_selection(&mut canvas, None, (px - 4, py - 4, 9, 9), false, None);
        }
        let mut tools = Vec::new();
        let mut nodes = Vec::new();
        if index == 0 && picker.keyboard {
            // What the arrow keys picked, announced as it changes.
            let area_mode = matches!(picker.mode, PickMode::Area) || (is_ui && picker.shot.mode == ShotMode::Area);
            let said = if area_mode {
                picker.shot.selection.map(|(v, (x, y, sw, sh))| {
                    let o = picker.views[v].origin;
                    let r = crate::a11y::Rect::new(x as i32, y as i32, sw.round() as i32, sh.round() as i32);
                    (keyboard_said((o.0 + r.x, o.1 + r.y), Some((r.w, r.h))), r)
                })
            } else {
                picker.point.map(|(v, (px, py))| {
                    let o = picker.views[v].origin;
                    (keyboard_said((o.0 + px as i32, o.1 + py as i32), None), crate::a11y::Rect::new(px as i32 - 4, py as i32 - 4, 9, 9))
                })
            };
            if let Some((said, r)) = said {
                nodes.push(crate::a11y::Node::label(said).id("keyboard").at(r).live());
            }
        }
        if index == 0 {
            let text = self.text.as_ref();
            // The pickers' hint (the screenshot UI's keys are in its
            // buttons' tooltips instead).
            if !is_ui {
                let message = hint(picker.mode);
                let tw = text.map_or(0.0, |t| t.width(message, size::LABEL)) as i32;
                let (bw, bh) = (tw + 32, 36);
                let bx = (w as i32 - bw) / 2;
                theme::popover_round(&mut canvas, f64::from(bx), 24.0, f64::from(bw), f64::from(bh), theme::pill(f64::from(bh)));
                label(text, &mut canvas, message, (bx + 16) as f32, baseline(text, 24, bh, size::LABEL), size::LABEL, FG, tw as f32 + 1.0);
                // Announced when the picker opens: how to pick with keys.
                nodes.push(crate::a11y::Node::label(message).id("hint").at(crate::a11y::Rect::new(bx, 24, bw, bh)).live());
            }
            if let (true, Some((x, y, sw, sh))) = (is_ui && picker.shot.mode != ShotMode::Screen, clear) {
                // The area or window that will be captured.
                let what = helix_i18n::trf!("Selected: {w} × {h}", w = sw, h = sh);
                nodes.push(crate::a11y::Node::label(what).id("selection").at(crate::a11y::Rect::new(x, y, sw, sh)));
            }
            if is_ui {
                let (placed, node) = paint_toolbar(&mut canvas, text, (w as i32, h as i32), &picker.shot);
                tools = placed;
                nodes.push(node);
            }
        }
        let surface = view.overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.overlay.layer.commit();
        }
        if index == 0 {
            let surface = surface.clone();
            if let Some(picker) = self.picker.as_mut() {
                picker.shot.tools = tools;
            }
            self.atspi_record(&surface, nodes);
        }
    }

    /// The picker for the accessibility bus: the monitor with the hint
    /// and the tool bar (its surface, place and size).
    pub(super) fn picker_a11y(&self) -> Option<super::atspi::SurfaceAt<'_>> {
        let view = self.picker.as_ref()?.views.first().filter(|v| v.overlay.configured && v.overlay.width > 0)?;
        Some((&view.overlay.layer, (view.origin.0, view.origin.1, view.overlay.width, view.overlay.height)))
    }

    /// The topmost window of the active workspace at a layout point:
    /// (x, y, width, height).
    fn window_rect_at(&self, px: i32, py: i32) -> Option<(i32, i32, i32, i32)> {
        self.windows
            .mru()
            .iter()
            .filter(|e| e.workspace == self.windows.active || e.workspace == crate::windows::ALL_WORKSPACES)
            .filter_map(|e| self.window_geometry.get(&e.id))
            .find(|g| !g.4 && px >= g.0 && py >= g.1 && px < g.0 + g.2 && py < g.1 + g.3)
            .map(|g| (g.0, g.1, g.2, g.3))
    }

    /// The screenshot UI's toolbar or a key changed something.
    fn shot_tool(&mut self, tool: Tool) {
        let Some(picker) = self.picker.as_mut() else { return };
        let st = &mut picker.shot;
        match tool {
            Tool::Mode(ShotMode::Window) if st.record => {}
            Tool::Mode(mode) => st.mode = mode,
            Tool::Record(on) if st.allow_record => {
                st.record = on;
                if on && st.mode == ShotMode::Window {
                    st.mode = ShotMode::Area;
                }
            }
            Tool::Record(_) => {}
            Tool::Pointer => st.pointer = !st.pointer,
            Tool::Close => {
                self.finish_pick(Picked::Cancelled);
                return;
            }
            Tool::Capture => {
                let target = match st.mode {
                    ShotMode::Screen => Some(Shot::Screen),
                    ShotMode::Area => st.selection.map(|(view, (x, y, w, h))| {
                        let origin = picker.views[view].origin;
                        Shot::Area(Area::new(origin.0 + x.round() as i32, origin.1 + y.round() as i32, w.round() as i32, h.round() as i32))
                    }),
                    ShotMode::Window => st.window.map(|(x, y)| Shot::WindowAt(x, y)),
                };
                if let Some(target) = target {
                    let (record, pointer) = (st.record, st.pointer);
                    self.finish_pick(Picked::Shot { target, record, pointer });
                }
                return;
            }
        }
        self.redraw_picker();
    }

    /// Pointer input on the picker; true if the event was for it.
    pub(super) fn picker_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(picker) = self.picker.as_mut() else { return false };
        let Some(index) = picker.views.iter().position(|v| v.overlay.layer.wl_surface() == &event.surface) else {
            return false;
        };
        let origin = picker.views[index].origin;
        let mode = picker.mode;
        let ui = matches!(mode, PickMode::Ui { .. });
        if matches!(event.kind, PointerEventKind::Press { .. }) {
            // The pointer took over: its drags are not read out.
            picker.keyboard = false;
        }
        let layout = (origin.0 + event.position.0.floor() as i32, origin.1 + event.position.1.floor() as i32);
        if ui {
            let (x, y) = event.position;
            // The toolbar: lit under the pointer, a click on release.
            let tool = (index == 0).then(|| picker.shot.tools.iter().find(|(r, _)| r.contains(x, y)).map(|(_, t)| *t)).flatten();
            let st = &mut picker.shot;
            let on_tools = tool.is_some() || st.tool_pressed.is_some();
            let out = super::press::track(&mut st.tool_hover, &mut st.tool_pressed, &event.kind, tool);
            if out.redraw {
                self.draw_pick_view(0);
            }
            if let Some(tool) = out.click {
                self.shot_tool(tool);
                return true;
            }
            if on_tools && matches!(event.kind, PointerEventKind::Press { .. } | PointerEventKind::Release { .. }) {
                return true;
            }
            let Some(picker) = self.picker.as_mut() else { return true };
            // Moving or resizing the selection.
            if let Some(edit) = picker.shot.edit.as_ref() {
                let bounds = { let v = &picker.views[edit.view].overlay; (f64::from(v.width), f64::from(v.height)) };
                let moved = grip_drag(edit.start, edit.grip, (x - edit.from.0, y - edit.from.1), bounds);
                let view = edit.view;
                match event.kind {
                    PointerEventKind::Motion { .. } => {
                        picker.shot.selection = Some((view, moved));
                        self.draw_pick_view(view);
                    }
                    PointerEventKind::Release { button: BTN_LEFT, .. } => {
                        picker.shot.selection = Some((view, moved));
                        picker.shot.edit = None;
                        self.draw_pick_view(view);
                    }
                    _ => {}
                }
                return true;
            }
            match event.kind {
                PointerEventKind::Press { button: BTN_RIGHT, .. } => self.finish_pick(Picked::Cancelled),
                PointerEventKind::Press { button: BTN_LEFT, .. } => {
                    // A press on the selection's handles or inside it takes
                    // it; elsewhere it starts a new one.
                    let grip = picker.shot.selection.filter(|(v, _)| *v == index && picker.shot.mode == ShotMode::Area).and_then(|(_, sel)| grip_at(sel, (x, y)).map(|g| (g, sel)));
                    if let Some((grip, sel)) = grip {
                        picker.shot.edit = Some(Edit { view: index, grip, from: (x, y), start: sel });
                        return true;
                    }
                    match picker.shot.mode {
                        ShotMode::Area => {
                            picker.drag = Some(Drag { view: index, start: event.position, now: event.position });
                            self.draw_pick_view(index);
                        }
                        ShotMode::Window => {
                            if self.window_rect_at(layout.0, layout.1).is_some() {
                                if let Some(p) = self.picker.as_mut() {
                                    p.shot.window = Some(layout);
                                }
                                self.redraw_picker();
                            }
                        }
                        ShotMode::Screen => {}
                    }
                }
                PointerEventKind::Motion { .. } => {
                    if let Some(drag) = picker.drag.as_mut() {
                        drag.now = event.position;
                        let view = drag.view;
                        self.draw_pick_view(view);
                    } else if picker.shot.mode == ShotMode::Window && picker.shot.window.is_none() {
                        let before = picker.shot.hover.and_then(|(x, y)| self.window_rect_at(x, y));
                        let after = self.window_rect_at(layout.0, layout.1);
                        if let Some(p) = self.picker.as_mut() {
                            p.shot.hover = Some(layout);
                        }
                        if before != after {
                            self.redraw_picker();
                        }
                    }
                }
                PointerEventKind::Release { button: BTN_LEFT, .. } => {
                    let Some(drag) = picker.drag.take() else { return true };
                    let (x, y, w, h) = span(drag.start, event.position);
                    if w >= MIN_DRAG && h >= MIN_DRAG {
                        picker.shot.selection = Some((drag.view, (x, y, w, h)));
                    }
                    self.redraw_picker();
                }
                _ => {}
            }
            return true;
        }
        match event.kind {
            PointerEventKind::Press { button: BTN_RIGHT, .. } => self.finish_pick(Picked::Cancelled),
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                if matches!(mode, PickMode::Point | PickMode::Window) {
                    let (x, y) = event.position;
                    self.finish_pick(Picked::Point(origin.0 + x.floor() as i32, origin.1 + y.floor() as i32));
                } else {
                    picker.drag = Some(Drag { view: index, start: event.position, now: event.position });
                    self.draw_pick_view(index);
                }
            }
            PointerEventKind::Motion { .. } => {
                if let Some(drag) = picker.drag.as_mut() {
                    drag.now = event.position;
                    let view = drag.view;
                    self.draw_pick_view(view);
                }
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                let Some(drag) = picker.drag.take() else { return true };
                let (x, y, w, h) = span(drag.start, event.position);
                if w < MIN_DRAG || h < MIN_DRAG {
                    self.draw_pick_view(drag.view);
                    return true;
                }
                let origin = picker.views[drag.view].origin;
                let area = Area::new(origin.0 + x.round() as i32, origin.1 + y.round() as i32, w.round() as i32, h.round() as i32);
                self.finish_pick(Picked::Area(area));
            }
            _ => {}
        }
        true
    }

    /// Draw the picker's views again.
    pub(super) fn redraw_picker(&mut self) {
        let count = self.picker.as_ref().map_or(0, |p| p.views.len());
        for index in 0..count {
            self.draw_pick_view(index);
        }
    }

    /// A touch drag on the picker was cancelled.
    pub(super) fn picker_cancel_drag(&mut self) {
        let Some(drag) = self.picker.as_mut().and_then(|p| p.drag.take()) else { return };
        self.draw_pick_view(drag.view);
    }

    /// Keys while the picker is open.
    pub(super) fn picker_key(&mut self, event: &KeyEvent) {
        let Some(picker) = self.picker.as_ref() else { return };
        let ui = matches!(picker.mode, PickMode::Ui { .. });
        let record = picker.shot.record;
        let arrow = match event.keysym {
            Keysym::Left | Keysym::KP_Left => Some((-1.0, 0.0)),
            Keysym::Right | Keysym::KP_Right => Some((1.0, 0.0)),
            Keysym::Up | Keysym::KP_Up => Some((0.0, -1.0)),
            Keysym::Down | Keysym::KP_Down => Some((0.0, 1.0)),
            _ => None,
        };
        if let Some(unit) = arrow {
            self.picker_arrow(unit);
            return;
        }
        match event.keysym {
            Keysym::Escape => self.finish_pick(Picked::Cancelled),
            Keysym::Return | Keysym::KP_Enter | Keysym::space if !ui => self.picker_confirm(),
            Keysym::Return | Keysym::KP_Enter | Keysym::space if ui => self.shot_tool(Tool::Capture),
            Keysym::s | Keysym::S if ui => self.shot_tool(Tool::Mode(ShotMode::Area)),
            Keysym::c | Keysym::C if ui => self.shot_tool(Tool::Mode(ShotMode::Screen)),
            Keysym::w | Keysym::W if ui => self.shot_tool(Tool::Mode(ShotMode::Window)),
            Keysym::v | Keysym::V if ui => self.shot_tool(Tool::Record(!record)),
            Keysym::p | Keysym::P if ui => self.shot_tool(Tool::Pointer),
            _ => {}
        }
    }

    /// An arrow key (`unit` its direction) on the picker: the area is
    /// moved (Ctrl: resized), or the point moved; the first one makes it
    /// in the middle of the first monitor.
    fn picker_arrow(&mut self, unit: (f64, f64)) {
        let (shift, ctrl, alt) = (self.mods.shift, self.mods.ctrl, self.mods.alt);
        let step = key_step(shift, alt);
        let d = (unit.0 * step, unit.1 * step);
        let Some(picker) = self.picker.as_mut() else { return };
        if picker.drag.is_some() || picker.shot.edit.is_some() {
            return;
        }
        let ui = matches!(picker.mode, PickMode::Ui { .. });
        let bounds = |v: &PickView| (f64::from(v.overlay.width), f64::from(v.overlay.height));
        let area = picker.mode == PickMode::Area || (ui && picker.shot.mode == ShotMode::Area);
        if ui && picker.shot.mode == ShotMode::Screen {
            return;
        }
        picker.keyboard = true;
        if area {
            picker.shot.selection = Some(match picker.shot.selection {
                Some((v, sel)) => (v, nudge_area(sel, d, ctrl, bounds(&picker.views[v]))),
                None => (0, first_area(bounds(&picker.views[0]))),
            });
        } else {
            let (v, p) = match picker.point {
                Some((v, p)) => (v, nudge_point(p, d, bounds(&picker.views[v]))),
                None => {
                    let b = bounds(&picker.views[0]);
                    (0, ((b.0 / 2.0).floor(), (b.1 / 2.0).floor()))
                }
            };
            picker.point = Some((v, p));
            if ui {
                // The screenshot UI's Window mode: the window under the point.
                let origin = picker.views[v].origin;
                let at = (origin.0 + p.0 as i32, origin.1 + p.1 as i32);
                let on_window = self.window_rect_at(at.0, at.1).is_some();
                if let Some(picker) = self.picker.as_mut() {
                    picker.shot.hover = Some(at);
                    picker.shot.window = on_window.then_some(at);
                }
            }
        }
        self.redraw_picker();
    }

    /// Enter on an area, point or window picker: what the arrow keys
    /// picked.
    fn picker_confirm(&mut self) {
        let Some(picker) = self.picker.as_ref() else { return };
        let picked = match picker.mode {
            PickMode::Area => picker.shot.selection.map(|(v, (x, y, w, h))| {
                let o = picker.views[v].origin;
                Picked::Area(Area::new(o.0 + x.round() as i32, o.1 + y.round() as i32, w.round() as i32, h.round() as i32))
            }),
            PickMode::Point | PickMode::Window => picker.point.map(|(v, (x, y))| {
                let o = picker.views[v].origin;
                Picked::Point(o.0 + x.floor() as i32, o.1 + y.floor() as i32)
            }),
            PickMode::Ui { .. } => None,
        };
        if let Some(picked) = picked {
            self.finish_pick(picked);
        }
    }

    // ----- Flash ------------------------------------------------------------

    fn flash(&mut self, area: Area) {
        let area_rect = Rect { x: area.x, y: area.y, w: area.width, h: area.height };
        for output in self.output_state.outputs() {
            let Some(info) = self.output_state.info(&output) else { continue };
            let (Some((ox, oy)), Some((ow, oh))) = (info.logical_position, info.logical_size) else { continue };
            let x0 = area_rect.x.max(ox);
            let y0 = area_rect.y.max(oy);
            let x1 = (area_rect.x + area_rect.w).min(ox + ow);
            let y1 = (area_rect.y + area_rect.h).min(oy + oh);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let surface = self.compositor.create_surface(&self.qh);
            if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
                surface.set_input_region(Some(region.wl_region()));
            }
            let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-flash".to_owned()), Some(&output));
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.set_size(0, 0);
            layer.commit();
            let overlay = Overlay { layer, width: 0, height: 0, configured: false, hits: Vec::new(), state: () };
            let local = Rect { x: x0 - ox, y: y0 - oy, w: x1 - x0, h: y1 - y0 };
            self.flashes.push(Flash { overlay, local, step: 0 });
        }
        if let Some(handle) = &self.loop_handle {
            let _ = handle.insert_source(Timer::from_duration(FLASH_STEP), |_, _, ui| {
                ui.flashes.retain_mut(|f| {
                    f.step += 1;
                    f.step < FLASH_STEPS
                });
                for index in 0..ui.flashes.len() {
                    ui.draw_flash(index);
                }
                if ui.flashes.is_empty() {
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(FLASH_STEP)
                }
            });
        }
    }

    pub(super) fn flash_configured(&mut self, layer: &LayerSurface, size: (u32, u32)) -> bool {
        let Some(index) = self.flashes.iter().position(|f| &f.overlay.layer == layer) else { return false };
        let overlay = &mut self.flashes[index].overlay;
        overlay.width = size.0;
        overlay.height = size.1;
        overlay.configured = true;
        self.draw_flash(index);
        true
    }

    pub(super) fn flash_closed(&mut self, layer: &LayerSurface) -> bool {
        let before = self.flashes.len();
        self.flashes.retain(|f| &f.overlay.layer != layer);
        before != self.flashes.len()
    }

    /// Draw every flash again.
    fn draw_flashes(&mut self) {
        for index in 0..self.flashes.len() {
            self.draw_flash(index);
        }
    }

    fn draw_flash(&mut self, index: usize) {
        let Some(flash) = self.flashes.get(index) else { return };
        let (w, h) = (flash.overlay.width as usize, flash.overlay.height as usize);
        if !flash.overlay.configured || w == 0 || h == 0 {
            return;
        }
        let scale = self.scales.of(flash.overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.buffers.create_buffer(flash.overlay.layer.wl_surface(), Ui::draw_flashes, physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        let alpha = 0.8 * (1.0 - flash.step as f32 / FLASH_STEPS as f32);
        let a = (alpha * 255.0) as u32;
        let r = flash.local;
        canvas.rect(r.x, r.y, r.w, r.h, (a << 24) | 0x00FFFFFF);
        let surface = flash.overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            flash.overlay.layer.commit();
        }
    }
}

impl Dispatch<HelixScreenshotManagerV1, ()> for Ui {
    fn event(
        _: &mut Self,
        _: &HelixScreenshotManagerV1,
        _: <HelixScreenshotManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HelixScreenshotFrameV1, FrameData> for Ui {
    fn event(
        _: &mut Self,
        frame: &HelixScreenshotFrameV1,
        event: helix_screenshot_frame_v1::Event,
        data: &FrameData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let result = match event {
            helix_screenshot_frame_v1::Event::Done { x, y, width, height } => Ok(Area::new(x, y, width, height)),
            helix_screenshot_frame_v1::Event::Failed { reason } => Err(match reason {
                WEnum::Value(Reason::Locked) => CaptureError::Locked,
                WEnum::Value(Reason::NoWindow) => CaptureError::NoWindow,
                WEnum::Value(Reason::Invalid) => CaptureError::Invalid,
                _ => CaptureError::Failed,
            }),
            _ => return,
        };
        frame.destroy();
        if let Some(reply) = data.reply.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
mod grip_tests {
    use super::*;

    /// Handles and the inside are taken where pressed; a drag moves the
    /// taken sides (or the whole), kept on the monitor.
    #[test]
    fn handles() {
        let sel = (100.0, 100.0, 200.0, 100.0);
        let corner = grip_at(sel, (98.0, 103.0)).expect("the top left handle");
        assert!(corner.left && corner.top && !corner.right && !corner.bottom);
        let edge = grip_at(sel, (300.0, 150.0)).expect("the right edge's handle");
        assert!(edge.right && !edge.left && !edge.top && !edge.bottom);
        assert_eq!(grip_at(sel, (200.0, 150.0)), Some(Grip::MOVE), "inside: moves it");
        assert_eq!(grip_at(sel, (50.0, 50.0)), None, "outside: a new selection");
        let bounds = (1280.0, 720.0);
        assert_eq!(grip_drag(sel, Grip::MOVE, (50.0, -20.0), bounds), (150.0, 80.0, 200.0, 100.0));
        assert_eq!(grip_drag(sel, Grip::MOVE, (2000.0, 0.0), bounds), (1080.0, 100.0, 200.0, 100.0), "kept on the monitor");
        assert_eq!(grip_drag(sel, corner, (20.0, 10.0), bounds), (120.0, 110.0, 180.0, 90.0));
        assert_eq!(grip_drag(sel, edge, (-250.0, 0.0), bounds), (50.0, 100.0, 50.0, 100.0), "past the other side: flipped");
        assert_eq!(grip_drag(sel, edge, (5000.0, 0.0), bounds), (100.0, 100.0, 1180.0, 100.0), "kept on the monitor");
    }

    /// Arrow keys: steps of 10 px (Shift 50, Alt 1); the area moves, or
    /// with Ctrl its right and bottom edges, kept on the monitor and at
    /// least 10 px; the point stays on the monitor.
    #[test]
    fn arrow_keys() {
        assert_eq!((key_step(false, false), key_step(true, false), key_step(false, true)), (10.0, 50.0, 1.0));
        let bounds = (1280.0, 720.0);
        let sel = first_area(bounds);
        assert_eq!(sel, (320.0, 180.0, 640.0, 360.0), "the middle quarter");
        assert_eq!(nudge_area(sel, (10.0, 0.0), false, bounds), (330.0, 180.0, 640.0, 360.0));
        assert_eq!(nudge_area(sel, (0.0, 5000.0), false, bounds), (320.0, 360.0, 640.0, 360.0), "kept on the monitor");
        assert_eq!(nudge_area(sel, (50.0, -10.0), true, bounds), (320.0, 180.0, 690.0, 350.0), "Ctrl: the size");
        assert_eq!(nudge_area(sel, (-5000.0, 5000.0), true, bounds), (320.0, 180.0, 10.0, 540.0), "at least 10 px, at most to the edge");
        assert_eq!(nudge_point((640.0, 360.0), (-1.0, 10.0), bounds), (639.0, 370.0));
        assert_eq!(nudge_point((5.0, 715.0), (-50.0, 50.0), bounds), (0.0, 719.0), "kept on the monitor");
        assert_eq!(keyboard_said((330, 180), Some((640, 360))), "Area at 330, 180: 640 × 360");
        assert_eq!(keyboard_said((639, 370), None), "Point at 639, 370");
    }

    /// The toolbar has every button, each with its key on the
    /// accessibility bus; Window cannot be chosen while recording.
    #[test]
    fn toolbar() {
        let mut data = vec![0u8; 1280 * 720 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 720, 1.0);
        let st = ShotState::new(true, true);
        let (placed, node) = paint_toolbar(&mut canvas, None, (1280, 720), &st);
        let tools: Vec<Tool> = placed.iter().map(|(_, t)| *t).collect();
        assert_eq!(tools, [Tool::Mode(ShotMode::Area), Tool::Mode(ShotMode::Screen), Tool::Record(false), Tool::Record(true), Tool::Pointer, Tool::Capture, Tool::Close]);
        assert_eq!(node.children.len(), 8);
        assert!(node.children.iter().all(|c| c.description.starts_with("Key: ")));
        assert!(placed.iter().all(|(r, _)| r.y + r.h <= 720 && r.x >= 0 && r.x + r.w <= 1280));
    }
}
