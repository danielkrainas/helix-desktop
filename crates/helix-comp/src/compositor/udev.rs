//! Bare-hardware backend: DRM/KMS outputs, GBM buffers, libinput input and
//! a libseat session (so it runs unprivileged from a VT and supports VT
//! switching).
//!
//! Single GPU for now: the seat's primary GPU drives every connector it has.
//! Structure follows Smithay's `anvil` example (MIT), simplified.

use std::collections::HashMap;
use std::time::Duration;

use helix_comp::display_config::{Mode as MonitorMode, Monitor, MonitorsConfig, ALL_TRANSFORMS};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportMemWl};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::desktop::layer_map_for_output;
use smithay::desktop::space::{space_render_elements, SpaceRenderElements};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as _, Mode as DrmMode, ModeTypeFlags};
use smithay::reexports::drm::Device as _;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{DeviceFd, Scale, Transform};
use smithay::wayland::compositor::with_states;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use super::Helix;

const CLEAR_COLOR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];
/// Framebuffer formats to try, best first.
const COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Xrgb8888];

smithay::backend::renderer::element::render_elements! {
    pub OutputElements<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor = MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface = WaylandSurfaceRenderElement<GlesRenderer>,
}

type Manager = DrmOutputManager<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;
type Output_ = DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

struct Surface {
    output: Output,
    drm_output: Output_,
    _global: GlobalId,
    /// Connector modes (for DisplayConfig; their `current` flags are
    /// ignored) and the same modes for DRM.
    modes: Vec<MonitorMode>,
    drm_modes: Vec<DrmMode>,
    /// Index of the mode shown; `None` when the monitor is turned off.
    current: Option<usize>,
    /// A frame is queued; wait for its vblank before rendering again.
    frame_pending: bool,
    /// A retry timer is armed (nothing to draw last time).
    retry_armed: bool,
}

pub struct UdevData {
    pub session: LibSeatSession,
    renderer: GlesRenderer,
    manager: Manager,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    cursor: MemoryRenderBuffer,
}

impl UdevData {
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    fn crtc_of(&self, connector: &str) -> Option<crtc::Handle> {
        self.surfaces.iter().find(|(_, s)| s.output.name() == connector && s.current.is_some()).map(|(c, _)| *c)
    }

    /// Gamma ramp length of the CRTC showing `connector`.
    pub fn gamma_size(&self, connector: &str) -> Option<usize> {
        let crtc = self.crtc_of(connector)?;
        let info = self.manager.device().get_crtc(crtc).ok()?;
        Some(info.gamma_length() as usize)
    }

    pub fn set_gamma(&mut self, connector: &str, ramps: &helix_comp::display_config::Ramps) -> Result<(), String> {
        let crtc = self.crtc_of(connector).ok_or("monitor is off")?;
        self.manager
            .device()
            .set_gamma(crtc, &ramps.0, &ramps.1, &ramps.2)
            .map_err(|e| e.to_string())
    }

    /// The image drawn for named cursors.
    pub fn cursor_buffer(&self) -> &MemoryRenderBuffer {
        &self.cursor
    }
}

/// Bring up the session, GPU, outputs and input. Returns the initial
/// monitor description for DisplayConfig.
pub fn init(
    event_loop: &mut EventLoop<'static, Helix>,
    state: &mut Helix,
) -> Result<Vec<Monitor>, Box<dyn std::error::Error>> {
    let (session, session_notifier) =
        LibSeatSession::new().map_err(|e| format!("cannot start a seat session (run from a VT?): {e}"))?;
    let seat = session.seat();
    tracing::info!(seat, "seat session started");

    let gpu_path = primary_gpu(&seat)?
        .or_else(|| all_gpus(&seat).ok()?.into_iter().next())
        .ok_or("no GPU found")?;
    let node = DrmNode::from_path(&gpu_path)?;
    tracing::info!(gpu = %gpu_path.display(), "using GPU");

    let fd = session.clone().open(
        &gpu_path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;

    // SAFETY: the GBM device (and its fd) outlive the EGL display, which
    // lives in the renderer stored next to them in UdevData.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new_with_priority(&egl_display, ContextPriority::High)?;
    // SAFETY: the context is current only on this thread.
    let renderer = unsafe { GlesRenderer::new(context)? };

    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let import_node = node.node_with_type(NodeType::Render).and_then(Result::ok);
    let manager = DrmOutputManager::new(
        drm,
        GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT),
        GbmFramebufferExporter::new(gbm.clone(), import_node),
        Some(gbm),
        COLOR_FORMATS,
        render_formats,
    );

    let (pixels, w, h) = super::cursor::arrow();
    let cursor = MemoryRenderBuffer::from_slice(&pixels, Fourcc::Argb8888, (w as i32, h as i32), 1, Transform::Normal, None);

    let handle = event_loop.handle();
    handle.insert_source(drm_notifier, |event, _, state| match event {
        DrmEvent::VBlank(crtc) => state.frame_finished(crtc),
        DrmEvent::Error(err) => tracing::warn!(%err, "DRM error"),
    })?;

    // Clients may share GPU buffers (dmabuf) and use any shm format the
    // renderer can import.
    state.shm_state.update_formats(renderer.shm_formats());
    let dmabuf_formats = renderer.dmabuf_formats();
    state
        .dmabuf_state
        .create_global::<Helix>(&state.display_handle, dmabuf_formats);

    // Input.
    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| "cannot assign libinput to the seat")?;
    handle.insert_source(LibinputInputBackend::new(libinput.clone()), |event, _, state| {
        match &event {
            smithay::backend::input::InputEvent::DeviceAdded { device } => state.input_device_added(device),
            smithay::backend::input::InputEvent::DeviceRemoved { device } => state.input_device_removed(device),
            _ => {}
        }
        state.process_input_event(event);
    })?;

    // VT switches pause and resume the session.
    handle.insert_source(session_notifier, move |event, _, state| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused (VT switched away)");
            libinput.suspend();
            if let Some(udev) = state.udev.as_mut() {
                udev.manager.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            if let Err(err) = libinput.resume() {
                tracing::warn!(?err, "cannot resume libinput");
            }
            if let Some(udev) = state.udev.as_mut() {
                if let Err(err) = udev.manager.activate(false) {
                    tracing::warn!(%err, "cannot reactivate DRM device");
                }
                for surface in udev.surfaces.values_mut() {
                    surface.frame_pending = false;
                }
            }
            state.reapply_gamma();
            state.render_all();
        }
    })?;

    // Monitor hotplug arrives as udev "changed" events on the GPU.
    let udev = UdevBackend::new(&seat)?;
    handle.insert_source(udev, move |event, _, state| {
        if let UdevEvent::Changed { device_id } = event {
            if device_id == node.dev_id() {
                state.scan_connectors();
            }
        }
    })?;

    state.udev = Some(UdevData {
        session,
        renderer,
        manager,
        scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        cursor,
    });
    state.scan_connectors();
    if state.space.outputs().next().is_none() {
        return Err("no connected display".into());
    }
    Ok(state.describe_outputs())
}

fn is_builtin(name: &str) -> bool {
    ["eDP", "LVDS", "DSI"].iter().any(|p| name.starts_with(p))
}

impl Helix {
    /// Match outputs to the GPU's connected connectors.
    pub fn scan_connectors(&mut self) {
        let Some(udev) = self.udev.as_mut() else { return };
        let events = match udev.scanner.scan_connectors(udev.manager.device()) {
            Ok(result) => result.into_iter().collect::<Vec<_>>(),
            Err(err) => {
                tracing::warn!(%err, "cannot scan connectors");
                return;
            }
        };
        for event in events {
            match event {
                DrmScanEvent::Connected { connector, crtc: Some(crtc) } => self.connector_connected(connector, crtc),
                DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => self.connector_disconnected(crtc),
                _ => {}
            }
        }
        let monitors = self.describe_outputs();
        self.send(helix_comp::Event::Monitors(monitors));
    }

    fn connector_connected(&mut self, connector: connector::Info, crtc: crtc::Handle) {
        let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
        let Some(mode_index) = connector
            .modes()
            .iter()
            .position(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or(if connector.modes().is_empty() { None } else { Some(0) })
        else {
            tracing::warn!(name, "connector has no modes");
            return;
        };
        let drm_mode = connector.modes()[mode_index];
        let wl_mode = WlMode::from(drm_mode);
        let (mm_w, mm_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (mm_w as i32, mm_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make: "Unknown".into(),
                model: "Unknown".into(),
            },
        );
        let global = output.create_global::<Helix>(&self.display_handle);
        let x = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|g| g.loc.x + g.size.w)
            .max()
            .unwrap_or(0);
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some((x, 0).into()));
        self.space.map_output(&output, (x, 0));

        let Some(udev) = self.udev.as_mut() else { return };
        let mut planes = udev.manager.device().planes(&crtc).ok();
        let is_nvidia = udev
            .manager
            .device()
            .get_driver()
            .is_ok_and(|d| d.name().to_string_lossy().to_lowercase().contains("nvidia"));
        if is_nvidia {
            // Overlay planes are unreliable on the NVIDIA driver (as in anvil).
            if let Some(planes) = planes.as_mut() {
                planes.overlay.clear();
            }
        }
        let drm_output = match udev.manager.initialize_output::<_, OutputElements>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            planes,
            &mut udev.renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(o) => o,
            Err(err) => {
                tracing::warn!(name, %err, "cannot set up output");
                self.space.unmap_output(&output);
                return;
            }
        };
        let modes = connector
            .modes()
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let wl = WlMode::from(*m);
                MonitorMode {
                    width: wl.size.w,
                    height: wl.size.h,
                    refresh_mhz: wl.refresh,
                    preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
                    current: i == mode_index,
                }
            })
            .collect();
        let drm_modes = connector.modes().to_vec();
        tracing::info!(name, mode = ?wl_mode, "output connected");
        udev.surfaces.insert(
            crtc,
            Surface {
                output,
                drm_output,
                _global: global,
                modes,
                drm_modes,
                current: Some(mode_index),
                frame_pending: false,
                retry_armed: false,
            },
        );
        layer_map_for_output(&udev.surfaces[&crtc].output).arrange();
        self.render_crtc(crtc);
    }

    fn connector_disconnected(&mut self, crtc: crtc::Handle) {
        let Some(udev) = self.udev.as_mut() else { return };
        if let Some(surface) = udev.surfaces.remove(&crtc) {
            tracing::info!(name = surface.output.name(), "output disconnected");
            self.space.unmap_output(&surface.output);
            self.display_handle.remove_global::<Helix>(surface._global);
        }
    }

    /// DisplayConfig's view of the connected outputs.
    pub fn describe_outputs(&self) -> Vec<Monitor> {
        let Some(udev) = self.udev.as_ref() else { return Vec::new() };
        let mut monitors: Vec<Monitor> = udev
            .surfaces
            .values()
            .map(|s| {
                let geo = self.space.output_geometry(&s.output).unwrap_or_default();
                let name = s.output.name();
                let mut modes = s.modes.clone();
                for (i, mode) in modes.iter_mut().enumerate() {
                    mode.current = s.current == Some(i);
                }
                Monitor {
                    builtin: is_builtin(&name),
                    primary: self.primary_output.as_deref() == Some(name.as_str()) && s.current.is_some(),
                    connector: name.clone(),
                    vendor: "Unknown".into(),
                    product: "Unknown".into(),
                    serial: "0".into(),
                    display_name: name,
                    modes,
                    x: geo.loc.x,
                    y: geo.loc.y,
                    scale: s.output.current_scale().fractional_scale(),
                    transform: super::monitors::from_transform(s.output.current_transform()),
                    transforms: ALL_TRANSFORMS.to_vec(),
                }
            })
            .collect();
        monitors.sort_by_key(|m| (m.x, m.y));
        // Without a configured primary that is on, the leftmost one is.
        if !monitors.iter().any(|m| m.primary) {
            if let Some(first) = monitors.iter_mut().find(|m| m.modes.iter().any(|mode| mode.current)) {
                first.primary = true;
            }
        }
        monitors
    }

    /// A queued frame reached the screen; draw the next one.
    pub fn frame_finished(&mut self, crtc: crtc::Handle) {
        let Some(surface) = self.udev.as_mut().and_then(|u| u.surfaces.get_mut(&crtc)) else {
            return;
        };
        if let Err(err) = surface.drm_output.frame_submitted() {
            tracing::warn!(%err, "frame submission failed");
        }
        surface.frame_pending = false;
        self.render_crtc(crtc);
    }

    pub fn render_all(&mut self) {
        let crtcs: Vec<_> = self
            .udev
            .as_ref()
            .map(|u| u.surfaces.keys().copied().collect())
            .unwrap_or_default();
        for crtc in crtcs {
            self.render_crtc(crtc);
        }
    }

    /// Render one output if it is due, then send frame callbacks.
    pub fn render_crtc(&mut self, crtc: crtc::Handle) {
        let pointer_location = self.seat.get_pointer().unwrap().current_location();
        let Helix {
            udev,
            space,
            cursor_status,
            start_time,
            lock,
            ..
        } = self;
        let Some(udev) = udev.as_mut() else { return };
        if !udev.session.is_active() {
            return;
        }
        let Some(surface) = udev.surfaces.get_mut(&crtc) else { return };
        if surface.frame_pending || surface.current.is_none() {
            return;
        }
        let output = surface.output.clone();
        let Some(geo) = space.output_geometry(&output) else { return };
        let renderer = &mut udev.renderer;
        let scale = output.current_scale().fractional_scale();

        let mut elements: Vec<OutputElements> = Vec::new();
        if geo.to_f64().contains(pointer_location) {
            let local = pointer_location - geo.loc.to_f64();
            match cursor_status {
                CursorImageStatus::Hidden => {}
                CursorImageStatus::Surface(cursor_surface) => {
                    let hotspot = with_states(cursor_surface, |states| {
                        states
                            .data_map
                            .get::<CursorImageSurfaceData>()
                            .map(|d| d.lock().unwrap().hotspot)
                            .unwrap_or_default()
                    });
                    let at = (local - hotspot.to_f64()).to_physical(scale).to_i32_round();
                    elements.extend(
                        render_elements_from_surface_tree(renderer, cursor_surface, at, Scale::from(scale), 1.0, Kind::Cursor)
                            .into_iter()
                            .map(OutputElements::CursorSurface),
                    );
                }
                CursorImageStatus::Named(_) => {
                    match MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        local.to_physical(scale),
                        &udev.cursor,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    ) {
                        Ok(e) => elements.push(OutputElements::Cursor(e)),
                        Err(err) => tracing::warn!(?err, "cannot draw cursor"),
                    }
                }
            }
        }
        if lock.locked {
            // Locked: only the lock surface (if any); nothing of the session.
            if let Some(lock_surface) = lock.surface_for(&output) {
                elements.extend(
                    render_elements_from_surface_tree(renderer, lock_surface.wl_surface(), (0, 0), Scale::from(scale), 1.0, Kind::Unspecified)
                        .into_iter()
                        .map(OutputElements::CursorSurface),
                );
            }
        } else {
            match space_render_elements(renderer, [&*space], &output, 1.0) {
                Ok(space_elements) => elements.extend(space_elements.into_iter().map(OutputElements::Space)),
                Err(err) => tracing::warn!(?err, "output has no mode"),
            }
        }

        let clear = if lock.locked { [0.0, 0.0, 0.0, 1.0] } else { CLEAR_COLOR };
        let rendered = match surface.drm_output.render_frame(renderer, &elements, clear, FrameFlags::DEFAULT) {
            Ok(result) => !result.is_empty,
            Err(err) => {
                tracing::warn!(?err, "render failed");
                false
            }
        };
        if rendered {
            match surface.drm_output.queue_frame(()) {
                Ok(()) => surface.frame_pending = true,
                Err(err) => tracing::warn!(%err, "cannot queue frame"),
            }
        }

        let locked = lock.locked;
        // Frame callbacks let clients draw their next frame.
        let elapsed = start_time.elapsed();
        for window in space.elements().filter(|_| !locked) {
            window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
        for layer in layer_map_for_output(&output).layers().filter(|_| !locked) {
            layer.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }

        // Nothing changed: check again in about a frame instead of spinning.
        if !rendered && !surface.retry_armed {
            surface.retry_armed = true;
            let refresh_mhz = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1);
            let frame = Duration::from_micros(1_000_000_000 / refresh_mhz as u64);
            let _ = self.loop_handle.insert_source(Timer::from_duration(frame), move |_, _, state| {
                if let Some(s) = state.udev.as_mut().and_then(|u| u.surfaces.get_mut(&crtc)) {
                    s.retry_armed = false;
                }
                state.render_crtc(crtc);
                TimeoutAction::Drop
            });
        }

        if locked {
            if rendered {
                self.lock_frame_shown();
            }
            self.lock_send_frames(&output);
        }
        self.space.refresh();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Apply a checked monitor configuration to the DRM outputs.
    pub fn apply_drm(&mut self, config: &MonitorsConfig) -> Result<Vec<Monitor>, String> {
        let Helix { udev, space, .. } = self;
        let udev = udev.as_mut().ok_or("not on DRM")?;
        let find = |name: &str| {
            config
                .logical
                .iter()
                .find_map(|l| l.monitors.iter().find(|c| c.connector == name).map(|c| (l, c)))
        };

        // Modes first: the step that can fail. This also turns monitors
        // that were off back on (their `current` was `None`).
        for surface in udev.surfaces.values_mut() {
            let name = surface.output.name();
            let Some((_, choice)) = find(&name) else { continue };
            let index = surface
                .modes
                .iter()
                .position(|m| (m.width, m.height, m.refresh_mhz) == (choice.width, choice.height, choice.refresh_mhz))
                .ok_or_else(|| format!("{name} has no such mode"))?;
            if surface.current == Some(index) {
                continue;
            }
            let drm_mode = surface.drm_modes[index];
            surface
                .drm_output
                .use_mode(drm_mode, &mut udev.renderer, &DrmOutputRenderElements::<GlesRenderer, OutputElements>::default())
                .map_err(|e| format!("{name} cannot use {}x{}: {e:?}", choice.width, choice.height))?;
            surface.output.change_current_state(Some(WlMode::from(drm_mode)), None, None, None);
            surface.output.set_preferred(WlMode::from(drm_mode));
            surface.current = Some(index);
            tracing::info!(name, width = choice.width, height = choice.height, "output mode changed");
        }

        // Then placement, scale and transform; outputs not mentioned go off.
        for surface in udev.surfaces.values_mut() {
            let name = surface.output.name();
            match find(&name) {
                Some((logical, _)) => {
                    surface.output.change_current_state(
                        None,
                        Some(super::monitors::to_transform(logical.transform)),
                        Some(super::monitors::to_scale(logical.scale)),
                        Some((logical.x, logical.y).into()),
                    );
                    space.map_output(&surface.output, (logical.x, logical.y));
                }
                None if surface.current.is_some() => {
                    tracing::info!(name, "output turned off");
                    space.unmap_output(&surface.output);
                    surface.current = None;
                    surface.frame_pending = false;
                    if let Err(err) = surface.drm_output.with_compositor(|c| c.clear()) {
                        tracing::warn!(name, %err, "cannot turn the output off");
                    }
                }
                None => {}
            }
        }
        self.render_all();
        Ok(self.describe_outputs())
    }

    /// Switch to another VT (Ctrl+Alt+F1..F12).
    pub fn switch_vt(&mut self, vt: i32) {
        if let Some(udev) = self.udev.as_mut() {
            if let Err(err) = udev.session.change_vt(vt) {
                tracing::warn!(%err, vt, "cannot switch VT");
            }
        }
    }
}
