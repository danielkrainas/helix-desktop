//! Nested backend: the compositor runs in a window of the host session.

use std::time::Duration;

use helix_comp::display_config::{Mode as MonitorMode, Monitor};
use helix_comp::Event;
use smithay::backend::renderer::{ImportDma, ImportMemWl};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::input::pointer::CursorImageStatus;
use smithay::desktop::layer_map_for_output;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::{Physical, Rectangle, Size, Transform};

use super::Helix;

const REFRESH_MHZ: i32 = 60_000;

/// How the nested output looks to DisplayConfig clients.
pub fn describe(output: &Output) -> Vec<Monitor> {
    let size = output.current_mode().map(|m| m.size).unwrap_or_default();
    describe_size(size, output.current_scale().fractional_scale())
}

fn describe_size(size: Size<i32, Physical>, scale: f64) -> Vec<Monitor> {
    vec![Monitor {
        connector: "WINIT-1".into(),
        vendor: "Helix".into(),
        product: "Nested".into(),
        serial: "0".into(),
        display_name: "Helix nested window".into(),
        builtin: false,
        modes: vec![MonitorMode {
            width: size.w,
            height: size.h,
            refresh_mhz: REFRESH_MHZ,
            preferred: true,
            current: true,
        }],
        x: 0,
        y: 0,
        scale,
        transform: 0,
        primary: true,
        // The window can't be rotated.
        transforms: vec![0],
    }]
}

/// Open the host window and drive rendering and input from it. Returns the
/// initial monitor description.
pub fn init(
    event_loop: &mut EventLoop<'static, Helix>,
    state: &mut Helix,
) -> Result<Vec<Monitor>, Box<dyn std::error::Error>> {
    let attributes = smithay::reexports::winit::window::Window::default_attributes()
        .with_inner_size(smithay::reexports::winit::dpi::LogicalSize::new(1280.0, 800.0))
        .with_title("Helix")
        .with_visible(true);
    let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)?;
    let size = backend.window_size();
    let mode = Mode {
        size,
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(
        "WINIT-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Helix".into(),
            model: "Nested".into(),
        },
    );
    output.user_data().insert_if_missing(|| helix_comp::edid::Identity { vendor: "Helix".into(), product: "Nested".into(), serial: "0".into() });
    let _global = output.create_global::<Helix>(&state.display_handle);
    // GL's origin is bottom-left; the winit backend needs a flip.
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // GPU buffers from clients, with feedback naming the host's GPU when it
    // can be found (helix-desktop-spec contracts/wayland/README.md, "GPU
    // buffers").
    state.shm_state.update_formats(backend.renderer().shm_formats());
    let formats: Vec<_> = backend.renderer().dmabuf_formats().into_iter().collect();
    let render_node = smithay::backend::egl::EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten());
    let feedback = render_node.and_then(|node| {
        smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(node.dev_id(), formats.clone()).build().ok()
    });
    // Explicit sync through the host GPU's render node.
    if let Some(path) = render_node.and_then(|n| n.dev_path()) {
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => state.enable_syncobj(smithay::backend::drm::DrmDeviceFd::new(std::os::fd::OwnedFd::from(file).into())),
            Err(err) => tracing::info!(%err, path = %path.display(), "cannot open the render node"),
        }
    }
    match feedback {
        Some(feedback) => {
            tracing::info!(?render_node, "dmabuf with feedback");
            state.dmabuf_state.create_global_with_default_feedback::<Helix>(&state.display_handle, &feedback);
        }
        None => {
            tracing::info!("dmabuf without feedback (no render node for the host GPU)");
            state.dmabuf_state.create_global::<Helix>(&state.display_handle, formats);
        }
    }

    // Shared with the timer that looks again when a frame had nothing new.
    let backend = std::rc::Rc::new(std::cell::RefCell::new(backend));
    let idle_check = backend.clone();
    let (wake_tx, wake_rx) = smithay::reexports::calloop::ping::make_ping()?;
    event_loop.handle().insert_source(wake_rx, move |_, _, _| {
        idle_check.borrow().window().request_redraw();
    })?;
    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: REFRESH_MHZ,
                    }),
                    None,
                    None,
                    None,
                );
                layer_map_for_output(&output).arrange();
                state.send(Event::Monitors(describe(&output)));
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let mut backend = backend.borrow_mut();
                // Named cursors are the host's; Helix draws an app's image
                // itself, with the host's pointer hidden.
                let host = match &state.cursor_status {
                    CursorImageStatus::Named(icon) => Some(*icon),
                    _ => None,
                };
                if state.host_cursor != Some(host) {
                    state.host_cursor = Some(host);
                    match host {
                        Some(icon) => {
                            backend.window().set_cursor(icon);
                            backend.window().set_cursor_visible(true);
                        }
                        None => backend.window().set_cursor_visible(false),
                    }
                }
                for (dmabuf, notifier) in std::mem::take(&mut state.pending_dmabufs) {
                    if backend.renderer().import_dmabuf(&dmabuf, None).is_ok() {
                        let _ = notifier.successful::<Helix>();
                    } else {
                        notifier.failed();
                    }
                }
                let rendered: Option<Vec<Rectangle<i32, smithay::utils::Physical>>>;
                {
                    // The elements first (the magnifier's color effects draw
                    // offscreen, which must happen before the window is bound).
                    let renderer = backend.renderer();
                    let scale = output.current_scale().fractional_scale();
                    let origin = state.space.output_geometry(&output).map(|g| g.loc.to_f64()).unwrap_or_default();
                    let geo = state.space.output_geometry(&output).unwrap_or_default();
                    let pointer = state.seat.get_pointer().unwrap().current_location();
                    let locked = state.lock.locked;
                    let elements: Vec<super::udev::OutputElements> = if locked {
                        // Locked: only the lock surface (if any), on black.
                        let mut elements: Vec<super::udev::OutputElements> =
                            state.cursor_surface_elements(renderer, origin, scale).into_iter().map(super::udev::OutputElements::CursorSurface).collect();
                        if let Some(s) = state.lock.surface_for(&output) {
                            let lock: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                                render_elements_from_surface_tree(renderer, s.wl_surface(), (0, 0), scale, 1.0, Kind::Unspecified);
                            elements.extend(lock.into_iter().map(super::udev::OutputElements::CursorSurface));
                        }
                        elements
                    } else {
                        let focused = state.focused_window();
                        // An app's own pointer image, in front.
                        let mut elements: Vec<super::udev::OutputElements> =
                            state.cursor_surface_elements(renderer, origin, scale).into_iter().map(super::udev::OutputElements::CursorSurface).collect();
                        elements.extend(
                            super::frames::scene(renderer, &state.space, &state.frames, &output, focused.as_ref())
                                .into_iter()
                                .map(super::udev::OutputElements::Scene),
                        );
                        elements
                    };
                    let elements = super::zoom::wrap(&mut state.zoom, renderer, &output, geo, pointer, elements);
                    // Monitors off: the window shows black.
                    let elements = if state.screens_off { Vec::new() } else { elements };
                    let clear = if locked || state.screens_off { [0.0, 0.0, 0.0, 1.0] } else { [0.12, 0.12, 0.14, 1.0] };
                    // Only what changed since this buffer was last drawn.
                    let age = {
                        if let Err(err) = backend.bind() {
                            tracing::warn!(%err, "cannot bind renderer");
                            return;
                        }
                        backend.buffer_age().unwrap_or(0)
                    };
                    let (renderer, mut framebuffer) = match backend.bind() {
                        Ok(bound) => bound,
                        Err(err) => {
                            tracing::warn!(%err, "cannot bind renderer");
                            return;
                        }
                    };
                    rendered = match damage_tracker.render_output(renderer, &mut framebuffer, age, &elements, clear) {
                        Ok(result) => result.damage.cloned(),
                        Err(err) => {
                            tracing::warn!(?err, "render failed");
                            None
                        }
                    };
                }
                if let Some(damage) = rendered.as_ref() {
                    if let Err(err) = backend.submit(Some(damage)) {
                        tracing::warn!(%err, "submit failed");
                    }
                }
                if rendered.is_some() {
                    // The host shows it at its next frame; we can't know
                    // when exactly, so now it is.
                    let mut feedback = super::timing::presentation_feedback(&state.space, &output);
                    feedback.presented::<Duration, smithay::utils::Monotonic>(
                        state.clock.now().into(),
                        smithay::wayland::presentation::Refresh::fixed(super::timing::refresh_of(&output)),
                        0,
                        smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(),
                    );
                    state.frame_shown(&output);
                } else {
                    // Nothing new to show: what waited for this frame may go on.
                    state.frame_shown(&output);
                }
                // The host draws named cursors, so they can't be captured.
                state.run_captures(backend.renderer(), false);
                state.cast_frames(backend.renderer(), false);
                state.send_cursor_frame(&output, state.start_time.elapsed());
                if state.lock.locked {
                    state.lock_frame_shown();
                    state.lock_send_frames(&output);
                    let _ = state.display_handle.flush_clients();
                    next_frame(&backend, rendered.is_some(), &state.loop_handle, &wake_tx);
                    return;
                }

                for window in state.space.elements() {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
                for layer in layer_map_for_output(&output).layers() {
                    layer.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
                state.space.refresh();
                state.popups.cleanup();
                let _ = state.display_handle.flush_clients();
                next_frame(&backend, rendered.is_some(), &state.loop_handle, &wake_tx);
            }
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })?;

    Ok(describe_size(size, 1.0))
}

/// After a frame: draw again at once when something was drawn (the swap
/// paces it), else look again in about a frame instead of spinning.
fn next_frame(
    backend: &WinitGraphicsBackend<GlesRenderer>,
    rendered: bool,
    handle: &smithay::reexports::calloop::LoopHandle<'static, Helix>,
    wake: &smithay::reexports::calloop::ping::Ping,
) {
    if rendered {
        backend.window().request_redraw();
        return;
    }
    let wake = wake.clone();
    let frame = Duration::from_micros(1_000_000_000 / REFRESH_MHZ.max(1) as u64);
    let _ = handle.insert_source(smithay::reexports::calloop::timer::Timer::from_duration(frame), move |_, _, _| {
        wake.ping();
        smithay::reexports::calloop::timer::TimeoutAction::Drop
    });
}
