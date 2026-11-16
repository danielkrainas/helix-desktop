//! Headless backend, for tests: one virtual monitor (two with
//! `HELIX_HEADLESS_OUTPUTS=2`), no window, no GPU and no input devices. Clients get frame callbacks at 60 Hz and shared-memory
//! buffers only.
//!
//! `HELIX_HEADLESS_SIZE` (`WIDTHxHEIGHT`, default 1280x720) sets the
//! monitor's size. With `HELIX_HEADLESS_SNAPSHOT=file.png` the compositor
//! writes what the monitor would show (shared-memory surfaces only) to that
//! file every second, and logs every window's placement. With
//! `HELIX_HEADLESS_INPUT=socket` it takes test input on that socket
//! (`headless_input`).

use std::time::Duration;

use helix_comp::display_config::{Mode as MonitorMode, Monitor};
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
        event_loop.handle().insert_source(Timer::from_duration(Duration::from_secs(1)), move |_, _, state| {
            state.headless_snapshot(&output, &path);
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })?;
    }
    Ok(describe(size, count))
}

impl Helix {
    /// A frame went by: what waited for it may go on.
    fn headless_frame(&mut self, output: &Output) {
        let now = self.start_time.elapsed();
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
        let Some(image) = self.headless_image(output) else { return };
        if let Err(err) = image.save(path) {
            tracing::warn!(%err, path = %path.display(), "cannot write the headless snapshot");
        }
    }

    /// What `output` shows (shared-memory surfaces, at scale 1); logs
    /// where each window is.
    pub(super) fn headless_image(&self, output: &Output) -> Option<Image> {
        let geo = self.space.output_geometry(output)?;
        let mut image = Image { width: geo.size.w, height: geo.size.h, pixels: vec![0xFF1E1E24; (geo.size.w * geo.size.h) as usize] };
        let fullscreen = self.in_fullscreen(output);
        let layers: Vec<(Layer, WlSurface, Point<i32, Logical>)> = {
            let map = layer_map_for_output(output);
            map.layers()
                .filter(|l| !(fullscreen && super::wm::hidden_in_fullscreen(l.layer(), l.namespace())))
                .filter_map(|l| Some((l.layer(), l.wl_surface().clone(), map.layer_geometry(l)?.loc + geo.loc)))
                .collect()
        };
        let draw_layers = |image: &mut Image, kinds: &[Layer]| {
            for (kind, surface, at) in &layers {
                if kinds.contains(kind) {
                    image.draw_tree(surface, *at - geo.loc);
                }
            }
        };
        draw_layers(&mut image, &[Layer::Background, Layer::Bottom]);
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in &windows {
            let (Some(location), Some(surface)) = (self.space.element_location(window), super::wm::surface_of(window)) else { continue };
            let geometry = window.geometry();
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
    /// The pixel at (`x`, `y`), ARGB.
    #[cfg(test)]
    pub(super) fn at(&self, x: i32, y: i32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }
}

impl Image {
    /// Draw `surface` and its subsurfaces with the surface's origin at `at`.
    fn draw_tree(&mut self, surface: &WlSurface, at: Point<i32, Logical>) {
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
        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
        for px in &self.pixels {
            rgba.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8, 0xFF]);
        }
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
