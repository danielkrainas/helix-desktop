//! Recording monitors for screen casts (helix-desktop-spec
//! `org.gnome.Mutter.ScreenCast.md`): frames of a monitor, rendered
//! off-screen at most `MAX_FPS` times a second while a consumer streams,
//! go to the stream's PipeWire node.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use helix_comp::screencast::{CastRequest, CastSource};
use helix_comp::Event;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, TextureMapping};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};

use super::pipewire::{Command, Notice, PipeWire, MAX_FPS};
use super::Helix;

const FRAME: Duration = Duration::from_millis(1000 / MAX_FPS as u64);

struct Cast {
    source: CastSource,
    cursor: bool,
    /// Rendering state between frames.
    target: Option<FrameTarget>,
    /// The session was locked at the last frame.
    locked: bool,
    size: Size<i32, Physical>,
    streaming: bool,
    last: Option<Instant>,
}

#[derive(Default)]
pub struct CastState {
    pipewire: Option<PipeWire>,
    casts: HashMap<u32, Cast>,
    notices: Option<smithay::reexports::calloop::channel::Sender<Notice>>,
    timer: bool,
}

/// RGBA → BGRx, in place (when the GPU can't read back BGRA).
pub fn to_bgrx(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
}

impl Helix {
    fn output_named(&self, connector: &str) -> Option<smithay::output::Output> {
        self.space.outputs().find(|o| o.name() == connector).cloned()
    }

    fn pipewire(&mut self) -> Result<&PipeWire, String> {
        if self.screencast.pipewire.is_none() {
            let (tx, rx) = smithay::reexports::calloop::channel::channel::<Notice>();
            self.loop_handle
                .insert_source(rx, |event, _, state| {
                    if let smithay::reexports::calloop::channel::Event::Msg(notice) = event {
                        state.cast_notice(notice);
                    }
                })
                .map_err(|e| e.to_string())?;
            self.screencast.notices = Some(tx.clone());
            self.screencast.pipewire = Some(PipeWire::start(tx)?);
        }
        Ok(self.screencast.pipewire.as_ref().expect("just started"))
    }

    /// The pixel size frames of `source` have now, if it can be shown.
    fn source_size(&self, source: &CastSource) -> Result<Size<i32, Physical>, String> {
        match source {
            CastSource::Monitor(connector) => {
                let output = self.output_named(connector).ok_or_else(|| format!("no monitor {connector}"))?;
                let geometry = self.space.output_geometry(&output).ok_or_else(|| format!("monitor {connector} is off"))?;
                let scale = output.current_scale().fractional_scale();
                Ok(geometry.size.to_f64().to_physical(scale).to_i32_round())
            }
            CastSource::Window(id) => {
                let window = self.window_by_list_id(*id).ok_or_else(|| format!("no window {id}"))?;
                let scale = self
                    .space
                    .outputs_for_element(&window)
                    .iter()
                    .map(|o| o.current_scale().fractional_scale())
                    .fold(0.0, f64::max);
                let scale = if scale > 0.0 { scale } else { 1.0 };
                Ok(window.geometry().size.to_f64().to_physical(scale).to_i32_round())
            }
        }
    }

    pub fn cast_request(&mut self, request: CastRequest) {
        match request {
            CastRequest::WindowSize { window, reply } => {
                let size = self.window_by_list_id(window).map(|w| {
                    let s = w.geometry().size;
                    (s.w, s.h)
                });
                let _ = reply.send(size);
            }
            CastRequest::Start { stream, source, cursor, reply } => {
                let size = match self.source_size(&source) {
                    Ok(size) if size.w > 0 && size.h > 0 => size,
                    Ok(_) => {
                        let _ = reply.send(Err("nothing to show yet".into()));
                        return;
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        return;
                    }
                };
                let (node_tx, node_rx) = std::sync::mpsc::channel();
                let created = self.pipewire().map(|pw| {
                    pw.send(Command::Create { id: stream, width: size.w as u32, height: size.h as u32, reply: node_tx })
                });
                match created {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = reply.send(Err("PipeWire thread gone".into()));
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "screen casting unavailable");
                        let _ = reply.send(Err(err));
                        return;
                    }
                }
                // The node id arrives once PipeWire set the stream up.
                std::thread::spawn(move || {
                    let node = node_rx
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| Err("PipeWire did not set up the stream".into()));
                    let _ = reply.send(node);
                });
                self.screencast.casts.insert(stream, Cast { source, cursor, size, streaming: false, last: None, target: None, locked: false });
                self.start_cast_timer();
            }
            CastRequest::Stop { stream } => {
                if self.screencast.casts.remove(&stream).is_some() {
                    if let Some(pw) = &self.screencast.pipewire {
                        pw.send(Command::Destroy { id: stream });
                    }
                }
            }
        }
    }

    fn cast_notice(&mut self, notice: Notice) {
        match notice {
            Notice::Streaming { id, on } => {
                if let Some(cast) = self.screencast.casts.get_mut(&id) {
                    tracing::info!(stream = id, on, "screen cast consumer");
                    cast.streaming = on;
                    // A new consumer needs a whole frame.
                    if let Some(target) = cast.target.as_mut() {
                        target.fresh = true;
                    }
                }
            }
            Notice::Ended { id } => self.end_cast(id),
        }
    }

    fn end_cast(&mut self, id: u32) {
        if self.screencast.casts.remove(&id).is_some() {
            if let Some(pw) = &self.screencast.pipewire {
                pw.send(Command::Destroy { id });
            }
            self.send(Event::ScreenCastEnded(id));
        }
    }

    /// On bare hardware, frames are made on a timer (nested: every redraw).
    fn start_cast_timer(&mut self) {
        if self.udev.is_none() || self.screencast.timer {
            return;
        }
        self.screencast.timer = true;
        let _ = self.loop_handle.insert_source(Timer::from_duration(FRAME), |_, _, state| {
            if state.screencast.casts.is_empty() {
                state.screencast.timer = false;
                return TimeoutAction::Drop;
            }
            if let Some(mut udev) = state.udev.take() {
                let cursor = udev.cursor_buffer().clone();
                state.cast_frames(udev.renderer(), Some(&cursor));
                state.udev = Some(udev);
            }
            TimeoutAction::ToDuration(FRAME)
        });
    }

    /// Render every streaming cast that is due a frame, into its own
    /// target with damage tracking: nothing is read back when nothing
    /// changed. Read-back finishes on the next tick, so the GPU is not
    /// waited on.
    pub fn cast_frames(&mut self, renderer: &mut GlesRenderer, named_cursor: Option<&MemoryRenderBuffer>) {
        let now = Instant::now();
        let locked = self.lock.locked;
        let due: Vec<u32> = self
            .screencast
            .casts
            .iter()
            .filter(|(_, c)| c.streaming && c.last.is_none_or(|t| now.duration_since(t) >= FRAME))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some(cast) = self.screencast.casts.get(&id) else { continue };
            let (source, cursor) = (cast.source.clone(), cast.cursor.then_some(named_cursor));
            // What to draw (nothing while locked: the target is just cleared).
            let scene = match &source {
                CastSource::Monitor(connector) => match self.output_named(connector) {
                    Some(output) => self.output_elements(renderer, &output, cursor).ok(),
                    None => {
                        self.end_cast(id);
                        continue;
                    }
                },
                CastSource::Window(window_id) => match self.window_by_list_id(*window_id) {
                    Some(window) => self.window_elements(renderer, &window, cursor, false).map(|(r, scale, e)| (r.size, scale, e)),
                    None => {
                        self.end_cast(id);
                        continue;
                    }
                },
            };
            // Hidden window (or monitor briefly unmapped): no frame.
            let Some((size, scale, elements)) = scene else { continue };
            let physical: Size<i32, Physical> = size.to_f64().to_physical(scale).to_i32_round();
            if physical.w <= 0 || physical.h <= 0 {
                continue;
            }
            let frame = {
                let cast = self.screencast.casts.get_mut(&id).expect("listed above");
                cast.last = Some(now);
                if cast.size != physical {
                    tracing::info!(stream = id, size = ?physical, "screen cast size changed");
                    cast.size = physical;
                    cast.target = None;
                    if let Some(pw) = &self.screencast.pipewire {
                        pw.send(Command::Resize { id, width: physical.w as u32, height: physical.h as u32 });
                    }
                }
                if cast.locked != locked {
                    cast.locked = locked;
                    if let Some(target) = cast.target.as_mut() {
                        target.fresh = true;
                    }
                }
                if cast.target.is_none() {
                    match FrameTarget::new(renderer, physical, scale) {
                        Ok(target) => cast.target = Some(target),
                        Err(err) => {
                            tracing::warn!(err, "cannot render screen cast frames");
                            continue;
                        }
                    }
                }
                let target = cast.target.as_mut().expect("just made");
                let finished = target.finish(renderer);
                let elements: &[super::capture::CaptureElement] = if locked { &[] } else { &elements };
                let clear = if locked { [0.0, 0.0, 0.0, 1.0] } else { super::capture::CLEAR };
                if let Err(err) = target.render(renderer, elements, clear) {
                    tracing::debug!(err, "screen cast frame failed");
                }
                finished
            };
            if let (Some(bgrx), Some(pw)) = (frame, &self.screencast.pipewire) {
                pw.send(Command::Frame { id, bgrx });
            }
        }
    }
}

/// A stream's render target: kept between frames so damage tracking can
/// tell when nothing changed.
pub struct FrameTarget {
    texture: GlesTexture,
    tracker: OutputDamageTracker,
    /// BGRA when the GPU can read it back so (no conversion), else RGBA.
    format: Fourcc,
    size: Size<i32, Physical>,
    /// Read-back started last tick.
    pending: Option<GlesMapping>,
    /// Draw everything next time (new target, lock change, new consumer).
    fresh: bool,
}

impl FrameTarget {
    fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>, scale: f64) -> Result<Self, String> {
        let buffer: Size<i32, Buffer> = (size.w, size.h).into();
        let (texture, format) = match renderer.create_buffer(Fourcc::Argb8888, buffer) {
            Ok(texture) => (texture, Fourcc::Argb8888),
            Err(_) => (renderer.create_buffer(Fourcc::Abgr8888, buffer).map_err(|e| format!("{e:?}"))?, Fourcc::Abgr8888),
        };
        Ok(Self {
            texture,
            tracker: OutputDamageTracker::new(size, scale, Transform::Normal),
            format,
            size,
            pending: None,
            fresh: true,
        })
    }

    /// Draw `elements`; start reading back if anything changed.
    fn render(&mut self, renderer: &mut GlesRenderer, elements: &[super::capture::CaptureElement], clear: [f32; 4]) -> Result<(), String> {
        let age = if self.fresh { 0 } else { 1 };
        let mut framebuffer = renderer.bind(&mut self.texture).map_err(|e| format!("{e:?}"))?;
        let result = self
            .tracker
            .render_output(renderer, &mut framebuffer, age, elements, clear)
            .map_err(|e| format!("{e:?}"))?;
        let changed = self.fresh || result.damage.is_some_and(|d| !d.is_empty());
        if changed {
            let region = Rectangle::from_size((self.size.w, self.size.h).into());
            let mapping = match renderer.copy_framebuffer(&framebuffer, region, self.format) {
                Ok(m) => m,
                Err(_) if self.format == Fourcc::Argb8888 => {
                    // This GPU can't read BGRA back: read RGBA and convert.
                    self.format = Fourcc::Abgr8888;
                    renderer.copy_framebuffer(&framebuffer, region, self.format).map_err(|e| format!("{e:?}"))?
                }
                Err(err) => return Err(format!("{err:?}")),
            };
            self.pending = Some(mapping);
            self.fresh = false;
        }
        Ok(())
    }

    /// The frame read back last tick, as BGRx top to bottom.
    fn finish(&mut self, renderer: &mut GlesRenderer) -> Option<Vec<u8>> {
        let mapping = self.pending.take()?;
        let bottom_up = !mapping.flipped();
        let data = renderer.map_texture(&mapping).ok()?;
        let (w, h) = (self.size.w as usize, self.size.h as usize);
        let stride = w * 4;
        if data.len() < stride * h {
            return None;
        }
        let mut out = vec![0u8; stride * h];
        for row in 0..h {
            let src = if bottom_up { h - 1 - row } else { row };
            out[row * stride..(row + 1) * stride].copy_from_slice(&data[src * stride..(src + 1) * stride]);
        }
        if self.format == Fourcc::Abgr8888 {
            to_bgrx(&mut out);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bgrx() {
        let mut px = [1, 2, 3, 4, 5, 6, 7, 8];
        super::to_bgrx(&mut px);
        assert_eq!(px, [3, 2, 1, 255, 7, 6, 5, 255]);
    }
}
