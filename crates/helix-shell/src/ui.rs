//! The shell UI: the top panel and the application launcher, both
//! wlr-layer-shell surfaces of the compositor.
//!
//! - Panel: an "Apps" button, the clock, and OSD level bars.
//! - Launcher: a full-screen overlay with a search field and a list of
//!   installed apps. Opened by the Apps button or by `ShowApplications`
//!   (which the compositor calls when Super is pressed alone).

use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use tokio::sync::mpsc::UnboundedSender;

use crate::launcher::{self, Launcher};
use crate::text::{Canvas, Style, Text};
use crate::{font, Osd, UiEvent, UiRequest};
use helix_protocols::client::helix_keybinding_v1::{self, HelixKeybindingV1};
use helix_protocols::client::helix_keybindings_manager_v1::{self, HelixKeybindingsManagerV1};
use smithay_client_toolkit::reexports::client::{Dispatch, WEnum};
use std::collections::HashMap;

pub const PANEL_HEIGHT: u32 = 32;
const OSD_DURATION: Duration = Duration::from_secs(2);
const BTN_LEFT: u32 = 0x110;

const PANEL_BG: u32 = 0xF01E1E24;
const FG: u32 = 0xFFEDEDEF;
const DIM_FG: u32 = 0xFF8A8A93;
const TRACK: u32 = 0xFF44444C;
const ACCENT: u32 = 0xFF3584E4;
const SCRIM: u32 = 0xB00C0C10;
const BOX_BG: u32 = 0xF81E1E24;
const FIELD_BG: u32 = 0xFF2C2C33;
const HOVER_BG: u32 = 0xFF2C2C33;

const APPS_LABEL: &str = "Apps";
const APPS_BUTTON_W: i32 = 72;
const ROW_H: i32 = 40;
const MAX_ROWS: usize = 10;

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x + self.w)
            && y < f64::from(self.y + self.h)
    }
}

struct LauncherView {
    layer: LayerSurface,
    model: Launcher,
    width: u32,
    height: u32,
    configured: bool,
    /// The search box and result list, for "click outside closes".
    content: Rect,
    /// Visible rows and the result index each shows.
    rows: Vec<(Rect, usize)>,
}

struct Ui {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    compositor: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<Ui>,

    panel: LayerSurface,
    panel_width: u32,
    panel_configured: bool,
    launcher: Option<LauncherView>,
    text: Option<Text>,
    osd: Option<(Osd, Instant)>,
    shown_minute: Option<String>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Reports to the D-Bus side (launcher open/close, key presses).
    requests: UnboundedSender<UiRequest>,
    /// Global key bindings; `None` unless the compositor launched us.
    keybindings: Option<HelixKeybindingsManagerV1>,
    bindings: HashMap<u32, HelixKeybindingV1>,
    exit: bool,
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

/// Baseline that vertically centers text of size `px` in a box.
fn baseline(text: Option<&Text>, top: i32, height: i32, px: f32) -> f32 {
    let cap = text.map_or(px * 0.7, |t| t.ascent(px) * 0.72);
    top as f32 + (height as f32 + cap) / 2.0
}

#[allow(clippy::too_many_arguments)]
fn label(text: Option<&Text>, canvas: &mut Canvas, s: &str, x: f32, base: f32, px: f32, color: u32, max_w: f32) {
    if let Some(text) = text {
        text.draw(canvas, s, x, base, Style { px, color, max_width: max_w });
    }
}

fn draw_osd(text: Option<&Text>, canvas: &mut Canvas, osd: &Osd, w: usize, h: usize) {
    let fraction = osd
        .level
        .map(|l| (l / osd.max_level.max(f64::EPSILON)).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let percent = format!("{}%", (fraction * 100.0).round() as u32);
    let bar_w = 200.min(w.saturating_sub(160)) as i32;
    let x0 = (w as i32 - bar_w - 56) / 2;
    let bar_y = h as i32 / 2 - 3;
    canvas.rect(x0, bar_y, bar_w, 6, TRACK);
    canvas.rect(x0, bar_y, (fraction * f64::from(bar_w)) as i32, 6, ACCENT);
    if text.is_some() {
        let base = baseline(text, 0, h as i32, 14.0);
        label(text, canvas, &percent, (x0 + bar_w + 12) as f32, base, 14.0, FG, 48.0);
    } else {
        let tx = (x0 + bar_w + 12) as usize;
        let ty = (h - font::GLYPH_H * 2) / 2;
        font::render(&percent, 2, |x, y| canvas.blend((tx + x) as i32, (ty + y) as i32, FG, 1.0));
    }
}

impl Ui {

    // ----- Panel -----------------------------------------------------------

    fn draw_panel(&mut self) {
        if !self.panel_configured || self.panel_width == 0 {
            return;
        }
        let (w, h) = (self.panel_width as usize, PANEL_HEIGHT as usize);
        let Ok((buffer, data)) =
            self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate panel buffer");
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(PANEL_BG);

        if self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            self.osd = None;
        }

        // "Apps" button, highlighted while the launcher is open.
        if self.launcher.is_some() {
            canvas.rect(4, 4, APPS_BUTTON_W - 8, h as i32 - 8, HOVER_BG);
        }
        let px = 14.0;
        let base = baseline(self.text.as_ref(), 0, h as i32, px);
        if let Some(text) = &self.text {
            let lw = text.width(APPS_LABEL, px);
            text.draw(&mut canvas, APPS_LABEL, (APPS_BUTTON_W as f32 - lw) / 2.0, base, Style { px, color: FG, max_width: 64.0 });
        }

        match &self.osd {
            Some((osd, _)) => draw_osd(self.text.as_ref(), &mut canvas, osd, w, h),
            None => {
                let clock = clock_text();
                match &self.text {
                    Some(text) => {
                        let cw = text.width(&clock, px);
                        text.draw(&mut canvas, &clock, (w as f32 - cw) / 2.0, base, Style { px, color: FG, max_width: 100.0 });
                    }
                    None => {
                        let scale = 2;
                        let tx = w.saturating_sub(font::text_width(&clock, scale)) / 2;
                        let ty = (h - font::GLYPH_H * scale) / 2;
                        font::render(&clock, scale, |x, y| canvas.blend((tx + x) as i32, (ty + y) as i32, FG, 1.0));
                    }
                }
                self.shown_minute = Some(clock);
            }
        }

        let surface = self.panel.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            self.panel.commit();
        }
    }

    // ----- Launcher --------------------------------------------------------

    fn open_launcher(&mut self, notify: bool) {
        if self.launcher.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("helix-launcher"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        let apps = launcher::installed();
        tracing::info!(apps = apps.len(), "launcher opened");
        self.launcher = Some(LauncherView {
            layer,
            model: Launcher::new(apps),
            width: 0,
            height: 0,
            configured: false,
            content: Rect { x: 0, y: 0, w: 0, h: 0 },
            rows: Vec::new(),
        });
        if notify {
            let _ = self.requests.send(UiRequest::Overview(true));
        }
        self.draw_panel();
    }

    fn close_launcher(&mut self, notify: bool) {
        if self.launcher.take().is_some() {
            if notify {
                let _ = self.requests.send(UiRequest::Overview(false));
            }
            self.draw_panel();
        }
    }

    fn launch_selected(&mut self) {
        let Some(view) = &self.launcher else { return };
        if let Some(app) = view.model.selected_app() {
            tracing::info!(app = %app.id, "launching");
            if let Err(err) = launcher::launch(&app.id) {
                tracing::warn!(%err, app = %app.id, "launch failed");
            }
        }
        self.close_launcher(true);
    }

    fn draw_launcher(&mut self) {
        let Some(mut view) = self.launcher.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_launcher(&mut view);
        }
        self.launcher = Some(view);
    }

    fn paint_launcher(&mut self, view: &mut LauncherView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let Ok((buffer, data)) =
            self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate launcher buffer");
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(SCRIM);

        let bw = 600.min(w as i32 - 48);
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 / 6).max(PANEL_HEIGHT as i32 + 24);

        // Search field.
        let field = Rect { x: bx, y: by, w: bw, h: 48 };
        canvas.rect(field.x, field.y, field.w, field.h, FIELD_BG);
        let (q_text, q_color) = if view.model.query.is_empty() {
            ("Type to search apps…", DIM_FG)
        } else {
            (view.model.query.as_str(), FG)
        };
        let base = baseline(self.text.as_ref(), field.y, field.h, 17.0);
        // Leave room for the caret before the placeholder.
        let text_x = if view.model.query.is_empty() { bx + 22 } else { bx + 16 };
        label(self.text.as_ref(), &mut canvas, q_text, text_x as f32, base, 17.0, q_color, (bw - 40) as f32);
        if let Some(text) = &self.text {
            let caret_x = bx + 16 + text.width(&view.model.query, 17.0) as i32 + 1;
            canvas.rect(caret_x, field.y + 13, 2, field.h - 26, ACCENT);
        }

        // Results.
        let list_top = by + 56;
        let room = ((h as i32 - list_top - 24) / ROW_H).max(1) as usize;
        let visible = MAX_ROWS.min(room);
        let first = view.model.selected.saturating_sub(visible - 1);
        let shown: Vec<usize> = (first..view.model.results.len()).take(visible).collect();
        let list_h = (shown.len().max(1) as i32) * ROW_H + 8;
        canvas.rect(bx, list_top, bw, list_h, BOX_BG);
        view.rows.clear();
        if shown.is_empty() {
            let base = baseline(self.text.as_ref(), list_top + 4, ROW_H, 15.0);
            label(self.text.as_ref(), &mut canvas, "No matching apps", (bx + 16) as f32, base, 15.0, DIM_FG, (bw - 32) as f32);
        }
        for (row, &result) in shown.iter().enumerate() {
            let rect = Rect { x: bx + 4, y: list_top + 4 + row as i32 * ROW_H, w: bw - 8, h: ROW_H };
            if result == view.model.selected {
                canvas.rect(rect.x, rect.y, rect.w, rect.h, ACCENT);
            }
            let app = &view.model.apps[view.model.results[result]];
            let base = baseline(self.text.as_ref(), rect.y, rect.h, 15.0);
            label(self.text.as_ref(), &mut canvas, &app.name, (rect.x + 12) as f32, base, 15.0, FG, (rect.w - 24) as f32);
            view.rows.push((rect, result));
        }
        view.content = Rect { x: bx, y: by, w: bw, h: list_top + list_h - by };

        let surface = view.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    fn launcher_key(&mut self, event: &KeyEvent) {
        let Some(view) = self.launcher.as_mut() else { return };
        match event.keysym {
            Keysym::Escape => {
                self.close_launcher(true);
                return;
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.launch_selected();
                return;
            }
            Keysym::Up => view.model.move_selection(-1),
            Keysym::Down => view.model.move_selection(1),
            Keysym::Page_Up => view.model.move_selection(-(MAX_ROWS as isize)),
            Keysym::Page_Down => view.model.move_selection(MAX_ROWS as isize),
            Keysym::BackSpace => view.model.backspace(),
            _ => {
                if let Some(text) = &event.utf8 {
                    view.model.type_text(text);
                }
            }
        }
        self.draw_launcher();
    }

    fn handle(&mut self, event: UiEvent) {
        match event {
            UiEvent::Osd(osd) => {
                self.osd = Some((osd, Instant::now() + OSD_DURATION));
                self.draw_panel();
            }
            UiEvent::Overview(true) => self.open_launcher(false),
            UiEvent::Overview(false) => self.close_launcher(false),
            // Monitor labels need per-output surfaces; not drawn yet.
            UiEvent::MonitorLabels(_) | UiEvent::HideMonitorLabels => {}
            UiEvent::Bind { action, accelerator } => self.bind(action, &accelerator),
            UiEvent::Unbind { action } => {
                if let Some(binding) = self.bindings.remove(&action) {
                    binding.destroy();
                }
            }
        }
    }

    fn bind(&mut self, action: u32, accelerator: &crate::accel::Accelerator) {
        let (Some(manager), Some((mods, key))) = (&self.keybindings, accelerator.to_wire()) else {
            if self.keybindings.is_none() {
                tracing::warn!(%accelerator, "global key bindings unavailable (shell not started by the compositor)");
            } else {
                tracing::warn!(%accelerator, "accelerator uses modifiers the compositor cannot bind");
            }
            let _ = self.requests.send(UiRequest::GrabFailed { action });
            return;
        };
        let modifiers = helix_keybindings_manager_v1::Modifier::from_bits_truncate(mods);
        let binding = manager.bind(modifiers, key, &self.qh, action);
        self.bindings.insert(action, binding);
    }

    /// Redraw when the minute changes or an OSD expires.
    fn tick(&mut self) {
        let osd_expired = self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until);
        let minute_changed =
            self.osd.is_none() && self.shown_minute.as_deref() != Some(&clock_text());
        if osd_expired || minute_changed {
            self.draw_panel();
        }
    }
}

/// Run the UI until the compositor goes away. Blocks the calling thread.
pub fn run(
    events: Channel<UiEvent>,
    requests: UnboundedSender<UiRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<Ui> = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    // Only advertised to the shell the compositor launched itself.
    let keybindings = globals.bind::<HelixKeybindingsManagerV1, _, _>(&qh, 1..=1, ()).ok();
    if keybindings.is_none() {
        tracing::warn!("compositor offers no key bindings to this process; accelerators will not fire");
    }

    let surface = compositor.create_surface(&qh);
    let panel = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("helix-panel"), None);
    panel.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    panel.set_size(0, PANEL_HEIGHT);
    panel.set_exclusive_zone(PANEL_HEIGHT as i32);
    panel.set_keyboard_interactivity(KeyboardInteractivity::None);
    panel.commit();

    let text = Text::load();
    if text.is_none() {
        tracing::warn!("no system font found; text is limited to the clock");
    }

    let pool = SlotPool::new((1920 * PANEL_HEIGHT * 4) as usize, &shm)?;
    let mut ui = Ui {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        qh: qh.clone(),
        panel,
        panel_width: 0,
        panel_configured: false,
        launcher: None,
        text,
        osd: None,
        shown_minute: None,
        keyboard: None,
        pointer: None,
        requests,
        keybindings,
        bindings: HashMap::new(),
        exit: false,
    };

    let mut event_loop: EventLoop<Ui> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| format!("cannot watch the Wayland connection: {e}"))?;
    handle
        .insert_source(events, |event, _, ui| {
            if let channel::Event::Msg(event) = event {
                ui.handle(event);
            }
        })
        .map_err(|e| format!("cannot watch UI events: {e}"))?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, ui| {
            ui.tick();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("cannot start UI timer: {e}"))?;

    tracing::info!("shell UI running");
    while !ui.exit {
        if let Err(err) = event_loop.dispatch(None, &mut ui) {
            // The compositor went away (it restarts or stops the shell);
            // exit rather than linger holding org.gnome.Shell.
            tracing::warn!(%err, "lost the compositor connection; shell UI stopping");
            break;
        }
    }
    Ok(())
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer == &self.panel {
            self.exit = true;
        } else {
            self.close_launcher(true);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if layer == &self.panel {
            if configure.new_size.0 > 0 {
                self.panel_width = configure.new_size.0;
            }
            self.panel_configured = true;
            self.draw_panel();
        } else if let Some(view) = self.launcher.as_mut().filter(|v| &v.layer == layer) {
            view.width = configure.new_size.0;
            view.height = configure.new_size.1;
            view.configured = true;
            self.draw_launcher();
        }
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
                Ok(p) => self.pointer = Some(p),
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
        self.launcher_key(&event);
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.launcher_key(&event);
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: RawModifiers, _: u32) {}
}

impl PointerHandler for Ui {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind else {
                continue;
            };
            let (x, y) = event.position;
            if &event.surface == self.panel.wl_surface() {
                if x < f64::from(APPS_BUTTON_W) {
                    if self.launcher.is_some() {
                        self.close_launcher(true);
                    } else {
                        self.open_launcher(true);
                    }
                }
            } else if let Some(view) = self.launcher.as_mut().filter(|v| v.layer.wl_surface() == &event.surface) {
                if let Some(&(_, result)) = view.rows.iter().find(|(r, _)| r.contains(x, y)) {
                    view.model.selected = result;
                    self.launch_selected();
                } else if !view.content.contains(x, y) {
                    self.close_launcher(true);
                }
            }
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

impl Dispatch<HelixKeybindingsManagerV1, ()> for Ui {
    fn event(
        _: &mut Self,
        _: &HelixKeybindingsManagerV1,
        _: helix_keybindings_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

/// User data: the org.gnome.Shell action id the binding belongs to.
impl Dispatch<HelixKeybindingV1, u32> for Ui {
    fn event(
        ui: &mut Self,
        binding: &HelixKeybindingV1,
        event: helix_keybinding_v1::Event,
        action: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let action = *action;
        match event {
            helix_keybinding_v1::Event::Bound => tracing::debug!(action, "accelerator bound"),
            helix_keybinding_v1::Event::Failed { reason } => {
                let reason = match reason {
                    WEnum::Value(helix_keybinding_v1::ErrorReason::Taken) => "taken",
                    WEnum::Value(helix_keybinding_v1::ErrorReason::UnknownKey) => "unknown key",
                    _ => "unknown reason",
                };
                tracing::warn!(action, reason, "compositor refused accelerator");
                if ui.bindings.remove(&action).is_some() {
                    binding.destroy();
                }
                let _ = ui.requests.send(UiRequest::GrabFailed { action });
            }
            helix_keybinding_v1::Event::Pressed { time } => {
                let _ = ui.requests.send(UiRequest::Accelerator { action, pressed: true, time });
            }
            helix_keybinding_v1::Event::Released { time } => {
                let _ = ui.requests.send(UiRequest::Accelerator { action, pressed: false, time });
            }
            _ => {}
        }
    }
}
