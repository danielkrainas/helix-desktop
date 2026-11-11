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
use crate::screenshot::{CaptureError, Job, PickMode, Picked, Rect as Area, Target};

const BTN_RIGHT: u32 = 0x111;
/// Drags smaller than this (logical pixels) are taken as clicks.
const MIN_DRAG: f64 = 2.0;
const FLASH_STEPS: u32 = 6;
const FLASH_STEP: Duration = Duration::from_millis(40);
const PICK_SCRIM: u32 = 0x80000000;
const HINT_BG: u32 = 0xE01E1E24;

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
}

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
        PickMode::Area => "Drag to select an area · Esc to cancel",
        PickMode::Point => "Click to pick a color · Esc to cancel",
        PickMode::Screenshot => "Drag to capture an area · Enter for the whole screen · Esc to cancel",
    }
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
        match target {
            Target::Screen => manager.capture_screen(cursor, fd.as_fd(), qh, data),
            Target::Window { frame: true } if manager.version() >= 3 => manager.capture_window_with_frame(cursor, fd.as_fd(), qh, data),
            Target::Window { .. } => manager.capture_window(cursor, fd.as_fd(), qh, data),
            Target::Area(a) => manager.capture_area(a.x, a.y, a.width, a.height, cursor, fd.as_fd(), qh, data),
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
        self.picker = Some(Picker { mode, reply: Some(reply), views, drag: None });
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
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0x00000000);
        // Dim everything but the selection.
        match picker.drag.as_ref().filter(|d| d.view == index) {
            Some(drag) => {
                let (x, y, sw, sh) = span(drag.start, drag.now);
                let (x, y, sw, sh) = (x as i32, y as i32, sw.round() as i32, sh.round() as i32);
                canvas.rect(0, 0, w as i32, y, PICK_SCRIM);
                canvas.rect(0, y + sh, w as i32, h as i32 - y - sh, PICK_SCRIM);
                canvas.rect(0, y, x, sh, PICK_SCRIM);
                canvas.rect(x + sw, y, w as i32 - x - sw, sh, PICK_SCRIM);
                for (rx, ry, rw, rh) in [(x - 1, y - 1, sw + 2, 1), (x - 1, y + sh, sw + 2, 1), (x - 1, y, 1, sh), (x + sw, y, 1, sh)] {
                    canvas.rect(rx, ry, rw, rh, ACCENT);
                }
            }
            None => canvas.rect(0, 0, w as i32, h as i32, PICK_SCRIM),
        }
        if index == 0 {
            let text = self.text.as_ref();
            let message = hint(picker.mode);
            let tw = text.map_or(0.0, |t| t.width(message, 14.0)) as i32;
            let (bw, bh) = (tw + 32, 36);
            let bx = (w as i32 - bw) / 2;
            canvas.rect(bx, 24, bw, bh, HINT_BG);
            label(text, &mut canvas, message, (bx + 16) as f32, baseline(text, 24, bh, 14.0), 14.0, FG, tw as f32 + 1.0);
        }
        let surface = view.overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.overlay.layer.commit();
        }
    }

    /// Pointer input on the picker; true if the event was for it.
    pub(super) fn picker_pointer(&mut self, event: &PointerEvent) -> bool {
        let Some(picker) = self.picker.as_mut() else { return false };
        let Some(index) = picker.views.iter().position(|v| v.overlay.layer.wl_surface() == &event.surface) else {
            return false;
        };
        let origin = picker.views[index].origin;
        let mode = picker.mode;
        match event.kind {
            PointerEventKind::Press { button: BTN_RIGHT, .. } => self.finish_pick(Picked::Cancelled),
            PointerEventKind::Press { button: BTN_LEFT, .. } => {
                if mode == PickMode::Point {
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
        match event.keysym {
            Keysym::Escape => self.finish_pick(Picked::Cancelled),
            Keysym::Return | Keysym::KP_Enter | Keysym::space if picker.mode == PickMode::Screenshot => {
                self.finish_pick(Picked::Screen)
            }
            _ => {}
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

    fn draw_flash(&mut self, index: usize) {
        let Some(flash) = self.flashes.get(index) else { return };
        let (w, h) = (flash.overlay.width as usize, flash.overlay.height as usize);
        if !flash.overlay.configured || w == 0 || h == 0 {
            return;
        }
        let scale = self.scales.of(flash.overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
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
