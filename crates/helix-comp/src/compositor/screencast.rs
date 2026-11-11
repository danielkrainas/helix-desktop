//! Recording monitors for screen casts (helix-desktop-spec
//! `org.gnome.Mutter.ScreenCast.md`): frames of a monitor, rendered
//! off-screen at most `MAX_FPS` times a second while a consumer streams,
//! go to the stream's PipeWire node.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use helix_comp::screencast::{CastRequest, CastSource, CursorMode};
use helix_comp::Event;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen, TextureMapping};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};

use super::pipewire::{Command, CursorMeta, Notice, PipeWire, MAX_FPS};
use super::Helix;

const FRAME: Duration = Duration::from_millis(1000 / MAX_FPS as u64);

struct Cast {
    source: CastSource,
    cursor: CursorMode,
    /// The pointer as last sent in metadata.
    sent_cursor: Option<CursorMeta>,
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
    /// Virtual monitors, by the stream they were made for.
    virtuals: HashMap<u32, (smithay::output::Output, smithay::reexports::wayland_server::backend::GlobalId)>,
    next_virtual: u32,
    /// Helix's arrow as cursor metadata (BGRA).
    arrow: Option<std::sync::Arc<(Vec<u8>, u32, u32)>>,
}

/// Size of virtual monitors.
const VIRTUAL_SIZE: (i32, i32) = (1920, 1080);

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
            CastSource::Area { connector, width, height, .. } => {
                let output = self.output_named(connector).ok_or_else(|| format!("no monitor {connector}"))?;
                let scale = output.current_scale().fractional_scale();
                Ok(Size::<i32, smithay::utils::Logical>::from((*width, *height)).to_f64().to_physical(scale).to_i32_round())
            }
            CastSource::Virtual(name) => {
                let output = self.output_named(name).ok_or_else(|| format!("no virtual monitor {name}"))?;
                let geometry = self.space.output_geometry(&output).ok_or("virtual monitor is off")?;
                Ok((geometry.size.w, geometry.size.h).into())
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
                    pw.send(Command::Create {
                        id: stream,
                        width: size.w as u32,
                        height: size.h as u32,
                        cursor_meta: cursor == CursorMode::Metadata,
                        reply: node_tx,
                    })
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
                self.screencast.casts.insert(
                    stream,
                    Cast { source, cursor, sent_cursor: None, size, streaming: false, last: None, target: None, locked: false },
                );
                self.start_cast_timer();
            }
            CastRequest::Stop { stream } => {
                if self.screencast.casts.remove(&stream).is_some() {
                    if let Some(pw) = &self.screencast.pipewire {
                        pw.send(Command::Destroy { id: stream });
                    }
                }
                self.remove_virtual(stream);
            }
            CastRequest::CreateVirtual { stream, reply } => {
                let _ = reply.send(Ok(self.create_virtual(stream)));
            }
            CastRequest::Input(input) => self.remote_input(input),
            CastRequest::ConnectEis { session, devices, reply } => {
                let _ = reply.send(self.connect_eis(session, devices));
            }
            CastRequest::EndRemote { session } => self.end_remote(session),
            CastRequest::LockKeys { reply } => {
                let state = self.seat.get_keyboard().map(|k| k.modifier_state());
                let _ = reply.send(state.map_or((false, false), |m| (m.caps_lock, m.num_lock)));
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

    /// A virtual monitor for `stream`, right of the others: name, position
    /// and size.
    fn create_virtual(&mut self, stream: u32) -> (String, (i32, i32), (i32, i32)) {
        use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
        self.screencast.next_virtual += 1;
        let name = format!("Virtual-{}", self.screencast.next_virtual);
        let x = self.space.outputs().filter_map(|o| self.space.output_geometry(o)).map(|g| g.loc.x + g.size.w).max().unwrap_or(0);
        let output = Output::new(
            name.clone(),
            PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "Helix".into(), model: "Virtual".into() },
        );
        let mode = Mode { size: VIRTUAL_SIZE.into(), refresh: 60_000 };
        output.change_current_state(Some(mode), Some(smithay::utils::Transform::Normal), Some(Scale::Integer(1)), Some((x, 0).into()));
        output.set_preferred(mode);
        let global = output.create_global::<Helix>(&self.display_handle);
        self.space.map_output(&output, (x, 0));
        tracing::info!(name, x, "virtual monitor added");
        self.screencast.virtuals.insert(stream, (output, global));
        (name, (x, 0), VIRTUAL_SIZE)
    }

    /// Where stream `stream`'s logical coordinates start (layout
    /// coordinates).
    pub(super) fn stream_origin(&self, stream: u32) -> Option<smithay::utils::Point<f64, smithay::utils::Logical>> {
        match &self.screencast.casts.get(&stream)?.source {
            CastSource::Monitor(name) | CastSource::Virtual(name) => {
                let output = self.output_named(name)?;
                self.space.output_geometry(&output).map(|g| g.loc.to_f64())
            }
            CastSource::Area { x, y, .. } => Some((f64::from(*x), f64::from(*y)).into()),
            CastSource::Window(id) => {
                let window = self.window_by_list_id(*id)?;
                self.space.element_location(&window).map(|l| l.to_f64())
            }
        }
    }

    /// Windows on a virtual monitor only get frame callbacks when its cast
    /// renders (no screen shows it).
    fn virtual_frame_callbacks(&self, output: &smithay::output::Output) {
        let time = self.start_time.elapsed();
        for window in self.space.elements_for_output(output) {
            window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
    }

    fn remove_virtual(&mut self, stream: u32) {
        let Some((output, global)) = self.screencast.virtuals.remove(&stream) else { return };
        tracing::info!(name = output.name(), "virtual monitor removed");
        self.space.unmap_output(&output);
        self.display_handle.remove_global::<Helix>(global);
        self.rescue_windows();
    }

    /// The pointer for cursor metadata, relative to a source at `origin`
    /// (logical) of `size` (logical) drawn at `scale`.
    fn cursor_meta(&mut self, origin: smithay::utils::Point<f64, smithay::utils::Logical>, size: Size<i32, smithay::utils::Logical>, scale: f64) -> CursorMeta {
        let bitmap = self
            .screencast
            .arrow
            .get_or_insert_with(|| {
                let (pixels, w, h) = super::cursor::arrow();
                std::sync::Arc::new((pixels, w as u32, h as u32))
            })
            .clone();
        let pos = self.seat.get_pointer().unwrap().current_location() - origin;
        let inside = pos.x >= 0.0 && pos.y >= 0.0 && pos.x < f64::from(size.w) && pos.y < f64::from(size.h);
        let hidden = matches!(self.cursor_status, smithay::input::pointer::CursorImageStatus::Hidden);
        let at = pos.to_physical(scale);
        CursorMeta { visible: inside && !hidden, x: at.x.round() as i32, y: at.y.round() as i32, hotspot: (0, 0), bitmap }
    }

    fn end_cast(&mut self, id: u32) {
        self.remove_virtual(id);
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
            let mode = cast.cursor;
            let (source, cursor) = (cast.source.clone(), (mode == CursorMode::Embedded).then_some(named_cursor));
            // Areas render their monitor and read back only the area.
            let mut crop: Option<Rectangle<i32, smithay::utils::Logical>> = None;
            // Where the source is (logical), for cursor metadata.
            let mut origin = smithay::utils::Point::<f64, smithay::utils::Logical>::default();
            // What to draw (nothing while locked: the target is just cleared).
            let scene = match &source {
                CastSource::Monitor(connector) | CastSource::Virtual(connector) => match self.output_named(connector) {
                    Some(output) => {
                        origin = self.space.output_geometry(&output).map(|g| g.loc.to_f64()).unwrap_or_default();
                        let scene = self.output_elements(renderer, &output, cursor).ok();
                        if matches!(source, CastSource::Virtual(_)) {
                            self.virtual_frame_callbacks(&output);
                        }
                        scene
                    }
                    None => {
                        self.end_cast(id);
                        continue;
                    }
                },
                CastSource::Area { connector, x, y, width, height } => match self.output_named(connector) {
                    Some(output) => {
                        let geo = self.space.output_geometry(&output).unwrap_or_default();
                        origin = (f64::from(*x), f64::from(*y)).into();
                        crop = Some(Rectangle::new((x - geo.loc.x, y - geo.loc.y).into(), (*width, *height).into()));
                        self.output_elements(renderer, &output, cursor).ok()
                    }
                    None => {
                        self.end_cast(id);
                        continue;
                    }
                },
                CastSource::Window(window_id) => match self.window_by_list_id(*window_id) {
                    Some(window) => self.window_elements(renderer, &window, cursor, false).map(|(r, scale, e)| {
                        origin = r.loc.to_f64();
                        (r.size, scale, e)
                    }),
                    None => {
                        self.end_cast(id);
                        continue;
                    }
                },
            };
            // Hidden window (or monitor briefly unmapped): no frame.
            let Some((size, scale, elements)) = scene else { continue };
            let source_size = crop.map_or(size, |c| c.size);
            let pointer = (mode == CursorMode::Metadata).then(|| self.cursor_meta(origin, source_size, scale));
            let crop_px = crop.map(|c| Rectangle::new(c.loc.to_f64().to_physical(scale).to_i32_round(), c.size.to_f64().to_physical(scale).to_i32_round()));
            let physical: Size<i32, Physical> = crop_px.map_or_else(|| size.to_f64().to_physical(scale).to_i32_round(), |c| c.size);
            let target_size: Size<i32, Physical> = size.to_f64().to_physical(scale).to_i32_round();
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
                    match FrameTarget::new(renderer, target_size, scale, crop_px) {
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
                // Only the pointer moved: send it with the last frame.
                let moved = pointer.as_ref().filter(|p| cast.sent_cursor.as_ref() != Some(p)).cloned();
                if pointer.is_some() {
                    cast.sent_cursor = pointer.clone();
                }
                (finished, moved)
            };
            let (frame, moved) = frame;
            if let Some(pw) = &self.screencast.pipewire {
                match (frame, moved) {
                    (Some(bgrx), _) => {
                        pw.send(Command::Frame { id, bgrx, cursor: pointer });
                    }
                    (None, Some(cursor)) => {
                        pw.send(Command::Cursor { id, cursor });
                    }
                    (None, None) => {}
                }
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
    /// The part read back (all of it unless an area is recorded).
    region: Rectangle<i32, Physical>,
    /// Read-back started last tick.
    pending: Option<GlesMapping>,
    /// Draw everything next time (new target, lock change, new consumer).
    fresh: bool,
}

impl FrameTarget {
    fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>, scale: f64, crop: Option<Rectangle<i32, Physical>>) -> Result<Self, String> {
        let buffer: Size<i32, Buffer> = (size.w, size.h).into();
        let (texture, format) = match renderer.create_buffer(Fourcc::Argb8888, buffer) {
            Ok(texture) => (texture, Fourcc::Argb8888),
            Err(_) => (renderer.create_buffer(Fourcc::Abgr8888, buffer).map_err(|e| format!("{e:?}"))?, Fourcc::Abgr8888),
        };
        Ok(Self {
            texture,
            tracker: OutputDamageTracker::new(size, scale, Transform::Normal),
            format,
            region: crop.unwrap_or_else(|| Rectangle::from_size(size)),
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
            let region = Rectangle::new((self.region.loc.x, self.region.loc.y).into(), (self.region.size.w, self.region.size.h).into());
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
        let (w, h) = (self.region.size.w as usize, self.region.size.h as usize);
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
