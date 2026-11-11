//! Nested backend: the compositor runs in a window of the host session.

use std::time::Duration;

use helix_comp::display_config::{Mode as MonitorMode, Monitor};
use helix_comp::Event;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::desktop::layer_map_for_output;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::{Physical, Rectangle, Size, Transform};

use super::Helix;

const REFRESH_MHZ: i32 = 60_000;

/// How the nested output looks to DisplayConfig clients.
pub fn describe(size: Size<i32, Physical>) -> Vec<Monitor> {
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
        scale: 1.0,
        transform: 0,
        primary: true,
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
    let _global = output.create_global::<Helix>(&state.display_handle);
    // GL's origin is bottom-left; the winit backend needs a flip.
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

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
                state.send(Event::Monitors(describe(size)));
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                if let Some(icon) = state.cursor.take() {
                    backend.window().set_cursor(icon);
                }
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);
                {
                    let (renderer, mut framebuffer) = match backend.bind() {
                        Ok(bound) => bound,
                        Err(err) => {
                            tracing::warn!(%err, "cannot bind renderer");
                            return;
                        }
                    };
                    if let Err(err) = smithay::desktop::space::render_output::<
                        _,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [&state.space],
                        &[],
                        &mut damage_tracker,
                        [0.12, 0.12, 0.14, 1.0],
                    ) {
                        tracing::warn!(?err, "render failed");
                    }
                }
                if let Err(err) = backend.submit(Some(&[damage])) {
                    tracing::warn!(%err, "submit failed");
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
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })?;

    Ok(describe(size))
}
