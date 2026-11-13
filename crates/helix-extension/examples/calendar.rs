//! A panel item with a popover (API 1.1): a click shows this month in a
//! layer surface the extension draws itself (here with
//! smithay-client-toolkit and helix-draw), where the shell said; Escape
//! or a click elsewhere closes it.

use std::time::Duration;

use helix_extension::{Event, Extension, PanelItemState, POPOVER_NAMESPACE};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};

const W: u32 = 300;
const H: u32 = 268;

/// Today: (year, month 1–12, day, weekday of the 1st, 0 = Sunday).
fn today() -> (i32, u32, u32, u32) {
    // SAFETY: localtime_r with valid pointers.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    let day = tm.tm_mday as u32;
    let first = ((tm.tm_wday - (tm.tm_mday - 1) % 7) + 7) % 7;
    (tm.tm_year + 1900, tm.tm_mon as u32 + 1, day, first as u32)
}

fn days_in(year: i32, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// What the extension's main task asks of the popover thread.
enum Ask {
    Toggle { x: i32, y: i32, output: String },
}

struct Popover {
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    text: Option<helix_draw::Text>,
}

impl Popover {
    fn toggle(&mut self, qh: &QueueHandle<Self>, x: i32, y: i32, output: &str) {
        if self.layer.take().is_some() {
            return;
        }
        let out = self.outputs.outputs().find(|o| self.outputs.info(o).and_then(|i| i.name).as_deref() == Some(output));
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some(POPOVER_NAMESPACE), out.as_ref());
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        // Margins from the monitor's edges, whatever the panel takes.
        layer.set_exclusive_zone(-1);
        layer.set_margin(y, 0, 0, x);
        layer.set_size(W, H);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        self.layer = Some(layer);
    }

    fn draw(&mut self) {
        let Some(layer) = &self.layer else { return };
        let Ok((buffer, data)) = self.pool.create_buffer(W as i32, H as i32, W as i32 * 4, wl_shm::Format::Argb8888) else { return };
        let mut canvas = helix_draw::Canvas::scaled(data, W as usize, H as usize, 1.0);
        canvas.fill(0xFF4A_4A55);
        canvas.rect(1, 1, W as i32 - 2, H as i32 - 2, 0xFF2E_2E36);
        let (year, month, day, first) = today();
        if let Some(text) = &self.text {
            let style = |px: f32, color: u32| helix_draw::Style { px, color, max_width: W as f32 };
            text.draw(&mut canvas, &format!("{} {year}", MONTHS[month as usize - 1]), 16.0, 30.0, style(17.0, 0xFFFF_FFFF));
            for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
                text.draw(&mut canvas, d, 22.0 + i as f32 * 38.0, 62.0, style(12.0, 0xFFA0_A0A8));
            }
            for n in 1..=days_in(year, month) {
                let cell = first + n - 1;
                let (cx, cy) = (16 + (cell % 7) as i32 * 38, 72 + (cell / 7) as i32 * 30);
                if n == day {
                    canvas.rect(cx, cy, 30, 26, 0xFF35_84E4);
                }
                let s = n.to_string();
                let tw = text.width(&s, 13.0);
                text.draw(&mut canvas, &s, cx as f32 + (30.0 - tw) / 2.0, cy as f32 + 18.0, style(13.0, 0xFFFF_FFFF));
            }
        }
        let surface = layer.wl_surface();
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, W as i32, H as i32);
            layer.commit();
        }
    }
}

impl CompositorHandler for Popover {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Popover {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Popover {
    /// Closed by the compositor (a click elsewhere).
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.layer.as_ref() == Some(layer) {
            self.layer = None;
        }
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.draw();
    }
}

impl SeatHandler for Popover {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Popover {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    /// Escape closes it (the extension's part).
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if event.keysym == Keysym::Escape {
            self.layer = None;
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl ShmHandler for Popover {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Popover);
smithay_client_toolkit::delegate_dispatch2!(Popover);

impl ProvidesRegistryState for Popover {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

/// The popover's own Wayland connection and loop, on a thread.
fn run_popover(asks: channel::Channel<Ask>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, queue) = registry_queue_init::<Popover>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let pool = SlotPool::new((W * H * 4) as usize, &shm).map_err(|e| e.to_string())?;
    let mut state = Popover {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?,
        layer_shell: LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?,
        shm,
        pool,
        layer: None,
        keyboard: None,
        text: helix_draw::Text::load(),
    };
    let mut event_loop: EventLoop<Popover> = EventLoop::try_new().map_err(|e| e.to_string())?;
    WaylandSource::new(conn, queue).insert(event_loop.handle()).map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(asks, move |event, _, state: &mut Popover| {
            if let channel::Event::Msg(Ask::Toggle { x, y, output }) = event {
                state.toggle(&qh, x, y, &output);
            }
        })
        .map_err(|e| e.to_string())?;
    loop {
        event_loop.dispatch(Duration::from_millis(500), &mut state).map_err(|e| e.to_string())?;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let mut ext = Extension::connect().await?;
    let (year, month, day, _) = today();
    let label = format!("{day} {}", &MONTHS[month as usize - 1][..3]);
    ext.panel_item("calendar", PanelItemState { icon: "x-office-calendar-symbolic".into(), label, tooltip: format!("{} {year}", MONTHS[month as usize - 1]), ..Default::default() }).await?;
    ext.start().await?;
    let (asks, asked) = channel::channel::<Ask>();
    std::thread::spawn(move || {
        if let Err(err) = run_popover(asked) {
            eprintln!("calendar popover: {err}");
        }
    });
    while let Some(event) = ext.next_event().await {
        if let Event::TogglePopover { x, y, output, .. } = event {
            let _ = asks.send(Ask::Toggle { x, y, output });
        }
    }
    Ok(())
}
