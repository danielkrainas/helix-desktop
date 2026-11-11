//! The top panel: a wlr-layer-shell surface showing the clock, and OSD
//! levels when requested.

use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};

use crate::{font, Osd, UiEvent};

pub const HEIGHT: u32 = 32;
const OSD_DURATION: Duration = Duration::from_secs(2);
const BACKGROUND: u32 = 0xF01E1E24;
const FOREGROUND: u32 = 0xFFEDEDEF;
const TRACK: u32 = 0xFF44444C;
const ACCENT: u32 = 0xFF3584E4;
const OVERVIEW_DOT: u32 = 0xFF3584E4;

struct Panel {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    width: u32,
    configured: bool,
    exit: bool,
    osd: Option<(Osd, Instant)>,
    overview: bool,
    shown_minute: Option<String>,
}

/// Local time as "HH:MM".
fn clock_text() -> String {
    // SAFETY: time/localtime_r write only into the provided struct.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "--:--".into();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

impl Panel {
    fn draw(&mut self) {
        if !self.configured || self.width == 0 {
            return;
        }
        let (w, h) = (self.width as usize, HEIGHT as usize);
        let (buffer, canvas) = match self.pool.create_buffer(
            w as i32,
            h as i32,
            (w * 4) as i32,
            wl_shm::Format::Argb8888,
        ) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(%err, "cannot allocate panel buffer");
                return;
            }
        };
        let mut put = |x: usize, y: usize, color: u32| {
            if x < w && y < h {
                let i = (y * w + x) * 4;
                canvas[i..i + 4].copy_from_slice(&color.to_le_bytes());
            }
        };
        for y in 0..h {
            for x in 0..w {
                put(x, y, BACKGROUND);
            }
        }

        if self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            self.osd = None;
        }
        let scale = 2;
        let text_y = (h - font::GLYPH_H * scale) / 2;
        match &self.osd {
            Some((osd, _)) => {
                // Level bar with a percentage, centered.
                let fraction = osd
                    .level
                    .map(|l| (l / osd.max_level.max(f64::EPSILON)).clamp(0.0, 1.0))
                    .unwrap_or(0.0);
                let percent = format!("{}%", (fraction * 100.0).round() as u32);
                let bar_w = 200.min(w.saturating_sub(120));
                let text_w = font::text_width(&percent, scale);
                let total = bar_w + 12 + text_w;
                let x0 = w.saturating_sub(total) / 2;
                let bar_y = h / 2 - 3;
                for y in bar_y..bar_y + 6 {
                    for x in 0..bar_w {
                        let filled = (x as f64) < fraction * bar_w as f64;
                        put(x0 + x, y, if filled { ACCENT } else { TRACK });
                    }
                }
                let tx = x0 + bar_w + 12;
                font::render(&percent, scale, |x, y| put(tx + x, text_y + y, FOREGROUND));
            }
            None => {
                let text = clock_text();
                let tx = w.saturating_sub(font::text_width(&text, scale)) / 2;
                font::render(&text, scale, |x, y| put(tx + x, text_y + y, FOREGROUND));
                self.shown_minute = Some(text);
            }
        }
        if self.overview {
            for y in h / 2 - 4..h / 2 + 4 {
                for x in 12..20 {
                    put(x, y, OVERVIEW_DOT);
                }
            }
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(err) = buffer.attach_to(surface) {
            tracing::warn!(%err, "cannot attach panel buffer");
            return;
        }
        self.layer.commit();
    }

    fn handle(&mut self, event: UiEvent) {
        match event {
            UiEvent::Osd(osd) => self.osd = Some((osd, Instant::now() + OSD_DURATION)),
            UiEvent::Overview(active) => self.overview = active,
            // Monitor labels need per-output surfaces; not drawn yet.
            UiEvent::MonitorLabels(_) | UiEvent::HideMonitorLabels => return,
        }
        self.draw();
    }

    /// Redraw when the minute changes or an OSD expires.
    fn tick(&mut self) {
        let osd_expired = self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until);
        let minute_changed = self.osd.is_none() && self.shown_minute.as_deref() != Some(&clock_text());
        if osd_expired || minute_changed {
            self.draw();
        }
    }
}

/// Run the panel until the compositor goes away. Blocks the calling thread.
pub fn run(ui: Channel<UiEvent>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<Panel> = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("helix-panel"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, HEIGHT);
    layer.set_exclusive_zone(HEIGHT as i32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let pool = SlotPool::new((1920 * HEIGHT * 4) as usize, &shm)?;
    let mut panel = Panel {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        width: 0,
        configured: false,
        exit: false,
        osd: None,
        overview: false,
        shown_minute: None,
    };

    let mut event_loop: EventLoop<Panel> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| format!("cannot watch the Wayland connection: {e}"))?;
    handle
        .insert_source(ui, |event, _, panel| {
            if let channel::Event::Msg(event) = event {
                panel.handle(event);
            }
        })
        .map_err(|e| format!("cannot watch UI events: {e}"))?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, panel| {
            panel.tick();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("cannot start panel timer: {e}"))?;

    tracing::info!("panel running");
    while !panel.exit {
        event_loop.dispatch(None, &mut panel)?;
    }
    Ok(())
}

impl CompositorHandler for Panel {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Panel {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Panel {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        self.configured = true;
        self.draw();
    }
}

impl ShmHandler for Panel {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Panel);

impl ProvidesRegistryState for Panel {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(Panel);
