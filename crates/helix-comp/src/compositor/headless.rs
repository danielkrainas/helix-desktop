//! Headless backend, for tests: one virtual monitor (two with
//! `HELIX_HEADLESS_OUTPUTS=2`), no window, no GPU (unless
//! `HELIX_HEADLESS_GPU`, below) and no input devices. Clients get frame
//! callbacks at 60 Hz and shared-memory buffers only.
//!
//! `HELIX_HEADLESS_SIZE` (`WIDTHxHEIGHT`, default 1280x720) sets the
//! monitor's size. With `HELIX_HEADLESS_SNAPSHOT=file.png` the compositor
//! writes what the monitor would show (shared-memory surfaces only) to that
//! file every second (the second monitor's as `file-2.png`), and logs
//! every window's placement. With
//! `HELIX_HEADLESS_INPUT=socket` it takes test input on that socket
//! (`headless_input`).
//!
//! With `HELIX_HEADLESS_GPU=/dev/dri/renderD…` it also draws every frame
//! with that GPU (off-screen, like a monitor would show it), offers GPU
//! buffers (`zwp_linux_dmabuf_v1`, explicit sync where the GPU has it) and
//! takes captures and snapshots from what it drew, so apps that draw with
//! the GPU (browsers) run as they would on hardware.

use std::time::Duration;

use helix_comp::display_config::{Mode as MonitorMode, Monitor, MonitorsConfig};
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size, Transform};
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::shell::wlr_layer::Layer;

use super::Helix;

const REFRESH_MHZ: i32 = 60_000;
const CONNECTOR: &str = "HEADLESS-1";
/// The second monitor's (`HELIX_HEADLESS_OUTPUTS=2`), right of the first.
const CONNECTOR_2: &str = "HEADLESS-2";

/// `HELIX_HEADLESS_OUTPUTS`: one monitor (default) or two side by side,
/// the same size (tests of what the shell shows on every monitor).
fn outputs_from_env() -> usize {
    match std::env::var("HELIX_HEADLESS_OUTPUTS").ok().as_deref().map(str::trim) {
        Some("2") => 2,
        _ => 1,
    }
}

fn size_from_env() -> Size<i32, Logical> {
    std::env::var("HELIX_HEADLESS_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .filter(|&(w, h): &(i32, i32)| w > 0 && h > 0)
        .unwrap_or((1280, 720))
        .into()
}

fn describe(size: Size<i32, Logical>, count: usize) -> Vec<Monitor> {
    let mut monitors = vec![Monitor {
        connector: CONNECTOR.into(),
        vendor: "Helix".into(),
        product: "Headless".into(),
        serial: "0".into(),
        display_name: "Helix headless monitor".into(),
        builtin: false,
        modes: vec![MonitorMode { width: size.w, height: size.h, refresh_mhz: REFRESH_MHZ, preferred: true, current: true }],
        x: 0,
        y: 0,
        scale: 1.0,
        transform: 0,
        primary: true,
        transforms: vec![0],
        hdr: false,
        color_mode: 0,
        underscan: None,
    }];
    if count > 1 {
        let mut second = monitors[0].clone();
        second.connector = CONNECTOR_2.into();
        second.serial = "1".into();
        second.display_name = "Helix headless monitor 2".into();
        second.x = size.w;
        second.primary = false;
        monitors.push(second);
    }
    monitors
}

/// A virtual monitor at `x` (layout coordinates), mapped and advertised.
fn add_output(state: &mut Helix, connector: &str, serial: &str, mode: Mode, x: i32) -> Output {
    let output = Output::new(
        connector.to_string(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "Helix".into(), model: "Headless".into() },
    );
    output.user_data().insert_if_missing(|| helix_comp::edid::Identity { vendor: "Helix".into(), product: "Headless".into(), serial: serial.into() });
    let _global = output.create_global::<Helix>(&state.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((x, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (x, 0));
    output
}

/// Set up the virtual monitor and the frame clock. Returns the monitor
/// description.
pub fn init(event_loop: &mut EventLoop<'static, Helix>, state: &mut Helix) -> Result<Vec<Monitor>, Box<dyn std::error::Error>> {
    let size = size_from_env();
    let mode = Mode { size: (size.w, size.h).into(), refresh: REFRESH_MHZ };
    let count = outputs_from_env();
    let output = add_output(state, CONNECTOR, "0", mode, 0);
    let mut frames_outputs = vec![output.clone()];
    if count > 1 {
        frames_outputs.push(add_output(state, CONNECTOR_2, "1", mode, size.w));
    }
    tracing::info!(width = size.w, height = size.h, count, "headless monitor");
    let snap_outputs = frames_outputs.clone();

    let frame = Duration::from_micros(1_000_000_000 / REFRESH_MHZ as u64);
    event_loop.handle().insert_source(Timer::from_duration(frame), move |_, _, state| {
        for output in &frames_outputs {
            state.headless_frame(output);
        }
        TimeoutAction::ToDuration(frame)
    })?;

    // Test input (keys, pointer), for this backend only.
    if let Some(path) = std::env::var_os("HELIX_HEADLESS_INPUT") {
        super::headless_input::listen(event_loop, std::path::Path::new(&path))?;
    }

    if let Some(path) = std::env::var_os("HELIX_HEADLESS_SNAPSHOT") {
        let path = std::path::PathBuf::from(path);
        // The second monitor's next to it, as `NAME-2.png`.
        let second = snap_outputs.get(1).cloned().map(|o| (o, path.with_file_name(format!("{}-2.png", path.file_stem().unwrap_or_default().to_string_lossy()))));
        event_loop.handle().insert_source(Timer::from_duration(Duration::from_secs(1)), move |_, _, state| {
            state.headless_snapshot(&output, &path);
            if let Some((output, path)) = &second {
                state.headless_snapshot(output, path);
            }
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })?;
    }
    state.headless = true;
    if let Some(path) = std::env::var_os("HELIX_HEADLESS_GPU") {
        match HeadlessGpu::new(state, std::path::Path::new(&path)) {
            Ok(gpu) => {
                tracing::info!(gpu = %std::path::Path::new(&path).display(), "headless monitor drawn with the GPU");
                state.headless_gpu = Some(Box::new(gpu));
            }
            Err(err) => tracing::warn!(%err, "cannot use the GPU for the headless monitor"),
        }
    }
    Ok(describe(size, count))
}

impl Helix {
    /// Apply a monitor configuration (checked beforehand): each virtual
    /// monitor moves and scales; one mode, no rotation, none can be
    /// turned off.
    pub(super) fn apply_headless(&mut self, config: &MonitorsConfig) -> Result<Vec<Monitor>, String> {
        let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
        outputs.sort_by_key(|o| o.name());
        let mut placed = Vec::new();
        for output in &outputs {
            let name = output.name();
            let logical = config
                .logical
                .iter()
                .find(|l| l.monitors.iter().any(|c| c.connector == name))
                .ok_or_else(|| format!("the headless monitor {name} cannot be turned off"))?;
            placed.push(logical);
        }
        for (output, l) in outputs.iter().zip(&placed) {
            output.change_current_state(None, None, Some(super::monitors::to_scale(l.scale)), Some((l.x, l.y).into()));
            self.space.map_output(output, (l.x, l.y));
        }
        let size = outputs.first().and_then(|o| o.current_mode()).map(|m| m.size).unwrap_or_default();
        let mut monitors = describe((size.w, size.h).into(), outputs.len());
        for (m, l) in monitors.iter_mut().zip(&placed) {
            (m.x, m.y, m.scale, m.primary) = (l.x, l.y, l.scale, l.primary);
            tracing::info!(connector = %m.connector, x = l.x, y = l.y, scale = l.scale, primary = l.primary, "headless monitor configured");
        }
        Ok(monitors)
    }

    /// A frame went by: what waited for it may go on.
    fn headless_frame(&mut self, output: &Output) {
        let now = self.start_time.elapsed();
        if let Some(mut gpu) = self.headless_gpu.take() {
            if gpu.draw(self, output) {
                let mut feedback = super::timing::presentation_feedback(&self.space, output);
                feedback.presented::<Duration, smithay::utils::Monotonic>(
                    self.clock.now().into(),
                    smithay::wayland::presentation::Refresh::fixed(super::timing::refresh_of(output)),
                    0,
                    smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                );
            }
            self.headless_gpu = Some(gpu);
        }
        self.frame_shown(output);
        self.send_cursor_frame(output, now);
        if self.lock.locked {
            self.lock_frame_shown();
            self.lock_send_frames(output);
        } else {
            for window in self.space.elements() {
                window.send_frame(output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
            }
            for layer in layer_map_for_output(output).layers() {
                layer.send_frame(output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
            }
        }
        self.space.refresh();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Write what `output` would show to `path` (shared-memory surfaces,
    /// at scale 1) and log where each window is.
    fn headless_snapshot(&mut self, output: &Output, path: &std::path::Path) {
        let Some(mut image) = self.headless_image(output) else { return };
        if let Some(pixels) = self.headless_gpu.as_mut().and_then(|gpu| gpu.read(output, (image.width, image.height))) {
            image.pixels = pixels;
        }
        if let Err(err) = image.save(path) {
            tracing::warn!(%err, path = %path.display(), "cannot write the headless snapshot");
        }
    }

    /// What `output` shows (shared-memory surfaces, at scale 1); logs
    /// where each window is.
    pub(super) fn headless_image(&self, output: &Output) -> Option<Image> {
        self.headless_image_masked(output, None)
    }

    /// [`Self::headless_image`], with what `mask` hides opaque black.
    pub(super) fn headless_image_masked(&self, output: &Output, mask: Option<&super::frames::Mask<'_>>) -> Option<Image> {
        let geo = self.space.output_geometry(output)?;
        let mut image = Image::filled(geo.size.w, geo.size.h);
        let fullscreen = self.in_fullscreen(output);
        // (Each with the rectangle it is blacked out as, when masked.)
        type Black = Option<smithay::utils::Rectangle<i32, Logical>>;
        let layers: Vec<(Layer, WlSurface, Point<i32, Logical>, Black)> = {
            let map = layer_map_for_output(output);
            map.layers()
                .filter(|l| !(fullscreen && super::wm::hidden_in_fullscreen(l.layer(), l.namespace())))
                .filter_map(|l| {
                    let lg = map.layer_geometry(l)?;
                    let black = mask.filter(|m| (m.layer)(l)).map(|_| {
                        let mut rect = l.bbox_with_popups();
                        rect.loc += lg.loc;
                        let mut rect = rect.merge(lg);
                        rect.loc += geo.loc;
                        rect
                    });
                    Some((l.layer(), l.wl_surface().clone(), lg.loc + geo.loc, black))
                })
                .collect()
        };
        let draw_layers = |image: &mut Image, kinds: &[Layer]| {
            for (kind, surface, at, black) in &layers {
                if kinds.contains(kind) {
                    match black {
                        Some(rect) => image.fill_rect(rect.loc - geo.loc, rect.size, 0xFF000000),
                        None => image.draw_tree(surface, *at - geo.loc),
                    }
                }
            }
        };
        draw_layers(&mut image, &[Layer::Background, Layer::Bottom]);
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in &windows {
            let (Some(location), Some(surface)) = (self.space.element_location(window), super::wm::surface_of(window)) else { continue };
            let geometry = window.geometry();
            if mask.is_some_and(|m| (m.window)(window)) {
                if let Some(rect) = super::frames::mask_rect(&self.space, window) {
                    image.fill_rect(rect.loc - geo.loc, rect.size, 0xFF000000);
                }
                continue;
            }
            image.draw_tree(&surface, location - geometry.loc - geo.loc);
            let toplevel = window.toplevel().map(|t| {
                let current = t.current_state();
                let title = smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().ok().map(|d| (d.app_id.clone(), d.title.clone())))
                });
                (format!("{:?}", current.states), current.size, title)
            });
            tracing::info!(
                ?location,
                geometry = ?geometry,
                bbox = ?window.bbox(),
                toplevel = ?toplevel,
                "headless window"
            );
        }
        draw_layers(&mut image, &[Layer::Top, Layer::Overlay]);
        Some(image)
    }
}

/// An ARGB image the snapshot is drawn into.
pub(super) struct Image {
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) pixels: Vec<u32>,
}

impl Image {
    /// One of `width` × `height` in the screen's background.
    pub(super) fn filled(width: i32, height: i32) -> Self {
        Self { width, height, pixels: vec![0xFF1E1E24; (width.max(0) * height.max(0)) as usize] }
    }

    /// As the captures' straight-alpha RGBA (opaque).
    pub(super) fn to_rgba(&self) -> super::capture::Image {
        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
        for px in &self.pixels {
            rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8, 0xFF]);
        }
        super::capture::Image { width: self.width as u32, height: self.height as u32, rgba }
    }

    /// The pixel at (`x`, `y`), ARGB.
    #[cfg(test)]
    pub(super) fn at(&self, x: i32, y: i32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }
}

impl Image {
    /// Draw `surface` and its subsurfaces with the surface's origin at `at`.
    /// `size` at `at` in one opaque `argb`.
    pub(super) fn fill_rect(&mut self, at: Point<i32, Logical>, size: Size<i32, Logical>, argb: u32) {
        for y in at.y.max(0)..(at.y + size.h).min(self.height) {
            for x in at.x.max(0)..(at.x + size.w).min(self.width) {
                self.pixels[(y * self.width + x) as usize] = argb;
            }
        }
    }

    pub(super) fn draw_tree(&mut self, surface: &WlSurface, at: Point<i32, Logical>) {
        with_surface_tree_downward(
            surface,
            at,
            |_, states, loc| {
                let view = states.data_map.get::<RendererSurfaceStateUserData>().and_then(|d| d.lock().ok()?.view());
                match view {
                    Some(view) => TraversalAction::DoChildren(*loc + view.offset),
                    None => TraversalAction::SkipChildren,
                }
            },
            |_, states, loc| {
                let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else { return };
                let Ok(data) = data.lock() else { return };
                let (Some(buffer), Some(view)) = (data.buffer(), data.view()) else { return };
                let at = *loc + view.offset;
                let _ = smithay::wayland::shm::with_buffer_contents(buffer, |ptr, len, spec| {
                    let opaque = spec.format == wl_shm::Format::Xrgb8888;
                    if !opaque && spec.format != wl_shm::Format::Argb8888 {
                        return;
                    }
                    for y in 0..spec.height {
                        for x in 0..spec.width {
                            let offset = (spec.offset + y * spec.stride + x * 4) as usize;
                            if offset + 4 > len {
                                return;
                            }
                            // SAFETY: inside the pool, checked above.
                            let px = unsafe { std::ptr::read_unaligned(ptr.add(offset) as *const u32) };
                            self.blend(at.x + x, at.y + y, if opaque { px | 0xFF00_0000 } else { px });
                        }
                    }
                });
            },
            |_, _, _| true,
        );
    }

    /// Premultiplied ARGB over the image.
    fn blend(&mut self, x: i32, y: i32, src: u32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let dst = &mut self.pixels[(y * self.width + x) as usize];
        let a = src >> 24;
        if a == 0xFF {
            *dst = src;
            return;
        }
        let mix = |shift: u32| {
            let s = (src >> shift) & 0xFF;
            let d = (*dst >> shift) & 0xFF;
            (s + d * (255 - a) / 255).min(255) << shift
        };
        *dst = 0xFF00_0000 | mix(16) | mix(8) | mix(0);
    }

    fn save(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let rgba = self.to_rgba().rgba;
        let tmp = path.with_extension("tmp");
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut encoder = png::Encoder::new(file, self.width as u32, self.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}


/// The headless backend's GPU: what each monitor shows, drawn off-screen.
pub struct HeadlessGpu {
    pub renderer: smithay::backend::renderer::gles::GlesRenderer,
    /// Per monitor (by name): the picture and its damage tracker.
    targets: Vec<(String, smithay::backend::renderer::gles::GlesTexture, smithay::backend::renderer::damage::OutputDamageTracker, bool)>,
    _gbm: smithay::backend::allocator::gbm::GbmDevice<smithay::backend::drm::DrmDeviceFd>,
}

impl HeadlessGpu {
    fn new(state: &mut Helix, path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        use smithay::backend::allocator::gbm::GbmDevice;
        use smithay::backend::drm::{DrmDeviceFd, DrmNode};
        use smithay::backend::egl::{EGLContext, EGLDisplay};
        use smithay::backend::renderer::gles::GlesRenderer;
        use smithay::backend::renderer::ImportDma;
        use smithay::utils::DeviceFd;
        let node = DrmNode::from_path(path)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)));
        state.enable_syncobj(fd.clone());
        let gbm = GbmDevice::new(fd)?;
        // SAFETY: the GBM device outlives the display (both kept here).
        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&display)?;
        // SAFETY: the context is current only on this thread.
        let renderer = unsafe { GlesRenderer::new(context)? };
        state.shm_state.update_formats(smithay::backend::renderer::ImportMemWl::shm_formats(&renderer));
        let formats = renderer.dmabuf_formats();
        let feedback = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(node.dev_id(), formats).build()?;
        state.dmabuf_state.create_global_with_default_feedback::<Helix>(&state.display_handle, &feedback);
        Ok(Self { renderer, targets: Vec::new(), _gbm: gbm })
    }

    /// Draw what `output` shows now; whether anything changed.
    fn draw(&mut self, state: &mut Helix, output: &Output) -> bool {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::{Bind, Offscreen, Renderer};
        let Some(mode) = output.current_mode() else { return false };
        let renderer = &mut self.renderer;
        state.animations.context = Some(renderer.context_id());
        state.animations.prune();
        let focused = state.focused_window();
        let elements: Vec<super::frames::SceneElement> = if state.lock.locked {
            Vec::new()
        } else {
            super::frames::scene(renderer, &state.space, &state.frames, output, focused.as_ref(), Some((&state.animations, state.wm_settings.motion)), state.in_fullscreen(output), None)
        };
        let name = output.name();
        if !self.targets.iter().any(|(n, _, _, _)| *n == name) {
            let Ok(texture) = Offscreen::<smithay::backend::renderer::gles::GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, (mode.size.w, mode.size.h).into()) else { return false };
            self.targets.push((name.clone(), texture, OutputDamageTracker::from_output(output), false));
        }
        let Some((_, texture, tracker, drawn)) = self.targets.iter_mut().find(|(n, _, _, _)| *n == name) else { return false };
        let Ok(mut framebuffer) = renderer.bind(texture) else { return false };
        let age = if *drawn { 1 } else { 0 };
        match tracker.render_output(renderer, &mut framebuffer, age, &elements, [0.12, 0.12, 0.14, 1.0]) {
            Ok(result) => {
                *drawn = true;
                result.damage.is_some()
            }
            Err(err) => {
                tracing::warn!(?err, "headless GPU drawing failed");
                false
            }
        }
    }

    /// What was last drawn for `output`, as ARGB pixels of `size`.
    fn read(&mut self, output: &Output, (w, h): (i32, i32)) -> Option<Vec<u32>> {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, TextureMapping as _};
        let name = output.name();
        let renderer = &mut self.renderer;
        let (_, texture, _, drawn) = self.targets.iter_mut().find(|(n, _, _, _)| *n == name)?;
        if !*drawn {
            return None;
        }
        let framebuffer = renderer.bind(texture).ok()?;
        let mapping = renderer.copy_framebuffer(&framebuffer, smithay::utils::Rectangle::from_size((w, h).into()), Fourcc::Abgr8888).ok()?;
        drop(framebuffer);
        let bottom_up = !mapping.flipped();
        let data = renderer.map_texture(&mapping).ok()?;
        let (w, h) = (w as usize, h as usize);
        let mut out = Vec::with_capacity(w * h);
        for row in 0..h {
            let src = if bottom_up { h - 1 - row } else { row };
            for px in data[src * w * 4..(src + 1) * w * 4].chunks_exact(4) {
                out.push(0xFF00_0000 | (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]));
            }
        }
        Some(out)
    }
}
