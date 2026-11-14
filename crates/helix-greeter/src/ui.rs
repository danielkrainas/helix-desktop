//! The greeter on screen: a wlr-layer-shell overlay on every monitor (the
//! background), with the login screen on the primary one. greetd and the
//! system bus are used from worker threads; their answers come back
//! through calloop channels.

use std::sync::mpsc;
use std::time::Duration;

use helix_draw::{physical, Canvas, Text};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};

use crate::app::{Effect, Greeter, Key};
use crate::greetd::{Greetd, Request, Response};
use crate::paint::{self, Hits};
use crate::power::{Offered, PowerAction};
use crate::users::User;

const BTN_LEFT: u32 = 0x110;

/// Answers from the worker threads.
enum Event {
    Greetd(std::io::Result<Response>),
    Users(Vec<User>),
    Power(Offered),
}

/// One monitor's surface.
struct Screen {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    scale: f64,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
    }
}

/// How the run ended.
pub enum Outcome {
    /// greetd will start the session.
    Started,
    Failed(String),
}

struct Ui {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    cursor_shapes: Option<CursorShapeManager>,
    qh: QueueHandle<Ui>,
    /// In the order monitors appeared: the first is the primary one.
    screens: Vec<Screen>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor: Option<WpCursorShapeDeviceV1>,
    shift: bool,
    text: Option<Text>,
    greeter: Greeter,
    hits: Hits,
    greetd: mpsc::Sender<Request>,
    system: tokio::sync::mpsc::UnboundedSender<PowerAction>,
    shown_clock: String,
    demo: bool,
    outcome: Option<Outcome>,
}

/// greetd's thread: one request at a time, in order.
fn greetd_worker(mut greetd: Box<dyn Greetd>, requests: mpsc::Receiver<Request>, events: Sender<Event>) {
    std::thread::Builder::new()
        .name("greetd".into())
        .spawn(move || {
            for request in requests {
                tracing::debug!(?request, "to greetd");
                let reply = greetd.request(&request);
                let failed = reply.is_err();
                if events.send(Event::Greetd(reply)).is_err() || failed {
                    break;
                }
            }
        })
        .expect("cannot start the greetd thread");
}

/// The system bus's thread: users, power buttons, power actions.
fn system_worker(load_users: bool, demo: bool, mut actions: tokio::sync::mpsc::UnboundedReceiver<PowerAction>, events: Sender<Event>) {
    std::thread::Builder::new()
        .name("system-bus".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!(%err, "no async runtime; listing /etc/passwd users");
                    let _ = events.send(Event::Users(crate::users::from_passwd()));
                    return;
                }
            };
            rt.block_on(async move {
                let system = match zbus::Connection::system().await {
                    Ok(c) => Some(c),
                    Err(err) => {
                        tracing::warn!(%err, "no system bus");
                        None
                    }
                };
                let users = if load_users { crate::users::load(system.as_ref()).await } else { Vec::new() };
                let _ = events.send(Event::Users(users));
                if let Some(system) = &system {
                    let _ = events.send(Event::Power(crate::power::offered(system).await));
                }
                while let Some(action) = actions.recv().await {
                    if demo {
                        println!("power {action:?} (demo: not done)");
                        continue;
                    }
                    let Some(system) = &system else { continue };
                    if let Err(err) = crate::power::act(system, action).await {
                        tracing::warn!(%err, ?action, "logind refused");
                    }
                }
            });
        })
        .expect("cannot start the system bus thread");
}

pub fn run(greeter: Greeter, greetd: Box<dyn Greetd>, load_users: bool, demo: bool) -> Outcome {
    match run_inner(greeter, greetd, load_users, demo) {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

fn run_inner(greeter: Greeter, greetd: Box<dyn Greetd>, load_users: bool, demo: bool) -> Result<Outcome, Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<Ui> = event_queue.handle();

    let (events_tx, events_rx) = channel::channel();
    let (greetd_tx, greetd_rx) = mpsc::channel();
    greetd_worker(greetd, greetd_rx, events_tx.clone());
    let (system_tx, system_rx) = tokio::sync::mpsc::unbounded_channel();
    system_worker(load_users, demo, system_rx, events_tx);

    let shm = Shm::bind(&globals, &qh)?;
    let text = Text::load();
    if text.is_none() {
        tracing::warn!("no system font found; text will be missing");
    }
    let mut ui = Ui {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh)?,
        layer_shell: LayerShell::bind(&globals, &qh)?,
        pool: SlotPool::new(1920 * 1080 * 4, &shm)?,
        shm,
        viewporter: globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok(),
        fractional: globals.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ()).ok(),
        cursor_shapes: CursorShapeManager::bind(&globals, &qh).ok(),
        qh: qh.clone(),
        screens: Vec::new(),
        keyboard: None,
        pointer: None,
        cursor: None,
        shift: false,
        text,
        greeter,
        hits: Hits::new(),
        greetd: greetd_tx,
        system: system_tx,
        shown_clock: String::new(),
        demo,
        outcome: None,
    };

    let mut event_loop: EventLoop<Ui> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue).insert(handle.clone()).map_err(|e| format!("cannot watch the Wayland connection: {e}"))?;
    handle
        .insert_source(events_rx, |event, _, ui| {
            if let channel::Event::Msg(event) = event {
                ui.event(event);
            }
        })
        .map_err(|e| format!("cannot watch the workers: {e}"))?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, ui| {
            if crate::clock::time(crate::clock::now()) != ui.shown_clock {
                ui.draw();
            }
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("cannot start the clock: {e}"))?;

    tracing::info!(demo, "greeter running");
    while ui.outcome.is_none() {
        if let Err(err) = event_loop.dispatch(None, &mut ui) {
            return Ok(Outcome::Failed(format!("lost the compositor: {err}")));
        }
    }
    Ok(ui.outcome.take().unwrap_or(Outcome::Failed("no outcome".into())))
}

impl Ui {
    fn event(&mut self, event: Event) {
        let effects = match event {
            Event::Greetd(Ok(response)) => {
                tracing::debug!(?response, "from greetd");
                self.greeter.greetd_reply(response)
            }
            Event::Greetd(Err(err)) => {
                self.outcome = Some(Outcome::Failed(format!("lost greetd: {err}")));
                return;
            }
            Event::Users(users) => {
                self.greeter.set_users(users);
                Vec::new()
            }
            Event::Power(offered) => {
                self.greeter.set_power(offered);
                Vec::new()
            }
        };
        self.apply(effects);
        self.draw();
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Greetd(request) => {
                    if self.greetd.send(request).is_err() {
                        self.outcome = Some(Outcome::Failed("greetd thread stopped".into()));
                    }
                }
                Effect::Power(action) => {
                    let _ = self.system.send(action);
                }
                Effect::Finished(state) => {
                    if self.demo {
                        println!("would remember {state:?}");
                    } else if let Err(err) = state.save() {
                        tracing::warn!(%err, "cannot save the greeter's state");
                    }
                    self.outcome = Some(Outcome::Started);
                }
            }
        }
    }

    fn add_screen(&mut self, output: wl_output::WlOutput) {
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("helix-greeter"), Some(&output));
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(layer.wl_surface(), &self.qh, ()));
        // Fractional scales need the viewport to show the buffer.
        let fractional = viewport.as_ref().and(self.fractional.as_ref()).map(|m| m.get_fractional_scale(layer.wl_surface(), &self.qh, ScaleOf(layer.wl_surface().clone())));
        layer.commit();
        self.screens.push(Screen { output, layer, width: 0, height: 0, configured: false, scale: 1.0, viewport, fractional });
    }

    fn draw(&mut self) {
        let view = self.greeter.view(crate::clock::now());
        self.shown_clock = view.time.clone();
        for index in 0..self.screens.len() {
            let screen = &self.screens[index];
            if !screen.configured || screen.width == 0 || screen.height == 0 {
                continue;
            }
            let (w, h, scale) = (screen.width as usize, screen.height as usize, screen.scale);
            let (pw, ph) = (physical(w, scale), physical(h, scale));
            let Ok((buffer, data)) = self.pool.create_buffer(pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Argb8888) else {
                tracing::warn!("cannot allocate a buffer");
                continue;
            };
            let mut canvas = Canvas::scaled(data, w, h, scale);
            if index == 0 {
                self.hits = paint::paint(&mut canvas, self.text.as_ref(), &view, w as i32, h as i32);
            } else {
                paint::paint_background(&mut canvas);
            }
            let screen = &self.screens[index];
            let surface = screen.layer.wl_surface();
            if let Some(viewport) = &screen.viewport {
                viewport.set_destination(w as i32, h as i32);
            }
            surface.damage_buffer(0, 0, pw as i32, ph as i32);
            if buffer.attach_to(surface).is_ok() {
                surface.commit();
            }
        }
    }

    fn key(&mut self, event: &KeyEvent) {
        let key = match event.keysym {
            Keysym::Up | Keysym::KP_Up => Key::Up,
            Keysym::Down | Keysym::KP_Down => Key::Down,
            Keysym::Return | Keysym::KP_Enter => Key::Return,
            Keysym::Escape => Key::Escape,
            Keysym::ISO_Left_Tab => Key::BackTab,
            Keysym::Tab if self.shift => Key::BackTab,
            Keysym::Tab => Key::Tab,
            Keysym::BackSpace => Key::Backspace,
            _ => match event.utf8.as_deref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) {
                Some(text) => Key::Text(text.to_owned()),
                None => return,
            },
        };
        let effects = self.greeter.key(key);
        self.apply(effects);
        self.draw();
    }

    fn on_primary(&self, surface: &wl_surface::WlSurface) -> bool {
        self.screens.first().is_some_and(|s| s.layer.wl_surface() == surface)
    }
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_screen(output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let was_primary = self.screens.first().is_some_and(|s| s.output == output);
        self.screens.retain(|s| s.output != output);
        if was_primary {
            // The next monitor takes the login screen.
            self.draw();
        }
    }
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Its monitor went away.
        self.screens.retain(|s| &s.layer != layer);
        self.draw();
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        if let Some(screen) = self.screens.iter_mut().find(|s| &s.layer == layer) {
            (screen.width, screen.height) = configure.new_size;
            screen.configured = true;
        }
        self.draw();
    }
}

impl SeatHandler for Ui {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(err) => tracing::warn!(%err, "no keyboard"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => {
                    self.cursor = self.cursor_shapes.as_ref().map(|m| m.get_shape_device(&p, qh));
                    self.pointer = Some(p);
                }
                Err(err) => tracing::warn!(%err, "no pointer"),
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(k) = self.keyboard.take() {
                k.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(c) = self.cursor.take() {
                c.destroy();
            }
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Ui {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key(&event);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key(&event);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: RawModifiers, _: u32) {
        self.shift = modifiers.shift;
    }
}

impl PointerHandler for Ui {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        let mut redraw = false;
        for event in events {
            let on_primary = self.on_primary(&event.surface);
            let hit = if on_primary { paint::hit_at(&self.hits, event.position.0, event.position.1) } else { None };
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(cursor) = &self.cursor {
                        cursor.set_shape(serial, Shape::Default);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    redraw |= self.greeter.hover.take().is_some();
                }
                PointerEventKind::Motion { .. } => {
                    if self.greeter.hover != hit {
                        self.greeter.hover = hit;
                        redraw = true;
                    }
                }
                PointerEventKind::Press { button: BTN_LEFT, .. } => {
                    let effects = self.greeter.click(hit);
                    self.apply(effects);
                    redraw = true;
                }
                _ => {}
            }
        }
        if redraw {
            self.draw();
        }
    }
}

impl ShmHandler for Ui {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Ui);

impl ProvidesRegistryState for Ui {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Ui);

/// Which surface a fractional-scale object belongs to.
struct ScaleOf(wl_surface::WlSurface);

impl Dispatch<WpFractionalScaleV1, ScaleOf> for Ui {
    fn event(ui: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, ScaleOf(surface): &ScaleOf, _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // In 120ths.
            let scale = f64::from(scale.max(1)) / 120.0;
            if let Some(screen) = ui.screens.iter_mut().find(|s| s.layer.wl_surface() == surface) {
                if screen.scale != scale {
                    screen.scale = scale;
                    ui.draw();
                }
            }
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for Ui {
    fn event(_: &mut Self, _: &WpFractionalScaleManagerV1, _: <WpFractionalScaleManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewporter, ()> for Ui {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewport, ()> for Ui {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
