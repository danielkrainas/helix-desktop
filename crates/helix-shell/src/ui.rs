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
use crate::auth::{AuthError, Authenticator, Secret};
use crate::end_dialog::{Choice, EndDialog, REBOOT, SHUTDOWN};
use crate::{font, Osd, PowerAction, UiEvent, UiRequest};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
};
use std::sync::Arc;
use helix_protocols::client::helix_keybinding_v1::{self, HelixKeybindingV1};
use helix_protocols::client::helix_keybindings_manager_v1::{self, HelixKeybindingsManagerV1};
use helix_protocols::client::helix_window_list_v1::{self, HelixWindowListV1};
use smithay_client_toolkit::reexports::client::{Dispatch, Proxy, WEnum};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use helix_protocols::client::helix_screenshot_manager_v1::HelixScreenshotManagerV1;
use crate::windows::{SwitchEntry, SwitchMode, Switcher};
use std::collections::HashMap;

mod capture;

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
/// Screen sharing indicator.
const SHARING: u32 = 0xFFE01B24;

const APPS_LABEL: &str = "Apps";
const APPS_BUTTON_W: i32 = 72;
/// Size of a workspace dot in the panel.
const WORKSPACE_DOT: i32 = 6;
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
    /// The shell's own key bindings (ids from `INTERNAL_ACTIONS` up).
    internal: HashMap<u32, Internal>,
    /// Open windows, from the compositor (privileged shell only).
    window_list: Option<HelixWindowListV1>,
    windows: crate::windows::Windows,
    /// The window or input source switcher, while open.
    switcher: Option<Overlay<SwitcherState>>,
    /// What the open window switcher was opened for.
    switcher_mode: Option<SwitchMode>,
    /// A `cycle-windows` / `cycle-group` run in progress.
    cycle: Option<Cycle>,
    /// The panel control with keyboard focus (`switch-panels`).
    panel_focus: Option<usize>,
    /// Screen captures (privileged shell only).
    screenshots: Option<HelixScreenshotManagerV1>,
    /// Picking an area, a point or what to capture, while open.
    picker: Option<capture::Picker>,
    flashes: Vec<capture::Flash>,
    loop_handle: Option<LoopHandle<'static, Ui>>,
    bindings: HashMap<u32, HelixKeybindingV1>,
    lock: LockScreen,
    banner: Option<BannerView>,
    /// Full-screen overlays: the end-session dialog and the power menu.
    dialog: Option<Overlay<DialogState>>,
    power_menu: Option<Overlay<()>>,
    /// Numbers identifying monitors (`ShowMonitorLabels`), one per output.
    monitor_labels: Vec<Overlay<i32>>,
    /// Which power actions logind allows (entries shown in the menu).
    power: crate::logind::Availability,
    /// The screen is being shared (panel indicator).
    sharing: bool,
    exit: bool,
}

/// A full-output overlay surface with clickable regions.
struct Overlay<T> {
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    hits: Vec<(Rect, OverlayHit)>,
    state: T,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OverlayHit {
    Confirm,
    Cancel,
    /// A window in the switcher.
    Window(u32),
    Power(PowerAction),
    /// A monitor in the share dialog, by index.
    Monitor(usize),
    Outside,
}

struct DialogState {
    kind: DialogKind,
    remaining: u32,
}

/// What the centered dialog asks.
enum DialogKind {
    /// Confirm ending the session (`EndSessionDialog`).
    End(EndDialog),
    /// Keep a display configuration change, or go back
    /// (`org.helix.DisplayChangeDialog`).
    DisplayChange,
    /// Which monitor to share with an app (`org.helix.ScreenCastChooser`).
    ShareScreen {
        app: String,
        /// (what, label, detail)
        sources: Vec<(crate::screencast_chooser::Shared, String, String)>,
        remember: bool,
        reply: Option<tokio::sync::oneshot::Sender<Option<crate::screencast_chooser::Shared>>>,
    },
}

const POWER_LABEL: &str = "Power";
const POWER_BUTTON_W: i32 = 80;

/// What a click on the banner hit.
#[derive(Debug, Clone, PartialEq)]
enum BannerHit {
    Body,
    Close,
    Button(String),
}

struct BannerView {
    data: crate::notifications::Banner,
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    hits: Vec<(Rect, BannerHit)>,
}

const BANNER_W: u32 = 440;
const BANNER_BG: u32 = 0xFF2E2E36;
const MONITOR_LABEL_SIZE: u32 = 72;
const BANNER_BORDER: u32 = 0xFF4A4A55;
const BANNER_PAD: i32 = 14;

/// Banner height for its content.
fn banner_height(text: Option<&Text>, banner: &crate::notifications::Banner) -> u32 {
    let body_lines = match (text, banner.body.is_empty()) {
        (_, true) => 0,
        (Some(t), false) => t.wrap(&banner.body, 14.0, (BANNER_W as i32 - 2 * BANNER_PAD) as f32, 2).len(),
        (None, false) => 1,
    } as i32;
    let buttons = if banner.buttons.is_empty() { 0 } else { 44 };
    (BANNER_PAD + 16 + 26 + body_lines * 20 + buttons + BANNER_PAD) as u32
}

/// The lock screen: one surface per output while the session is locked.
struct LockScreen {
    state: SessionLockState,
    session: Option<SessionLock>,
    surfaces: Vec<LockView>,
    password: String,
    message: Option<String>,
    checking: bool,
    auth: Arc<dyn Authenticator>,
    user: String,
    /// Results of password checks, from the worker thread.
    results: channel::Sender<Result<(), AuthError>>,
}

struct LockView {
    surface: SessionLockSurface,
    width: u32,
    height: u32,
}

/// Clear a password buffer, overwriting its bytes first.
fn wipe(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8, and the string is cleared after.
    unsafe { s.as_mut_vec().iter_mut().for_each(|b| std::ptr::write_volatile(b, 0)) };
    s.clear();
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

/// Words for the OSD icons the shell uses itself.
fn osd_caption(osd: &Osd) -> Option<String> {
    if let Some(label) = &osd.label {
        return Some(label.clone());
    }
    let caption = match osd.icon.as_deref()? {
        "audio-volume-muted-symbolic" => "Muted",
        i if i.starts_with("audio-volume-") => "Volume",
        "display-brightness-symbolic" => "Brightness",
        "microphone-sensitivity-muted-symbolic" => "Microphone off",
        i if i.starts_with("microphone-sensitivity-") => "Microphone on",
        _ => return None,
    };
    Some(caption.to_owned())
}

fn draw_osd(text: Option<&Text>, canvas: &mut Canvas, osd: &Osd, w: usize, h: usize) {
    let caption = osd_caption(osd);
    let Some(level) = osd.level else {
        // Just words (e.g. "Microphone off").
        if let (Some(t), Some(caption)) = (text, &caption) {
            let cw = t.width(caption, 14.0);
            let base = baseline(text, 0, h as i32, 14.0);
            label(text, canvas, caption, (w as f32 - cw) / 2.0, base, 14.0, FG, cw + 1.0);
        }
        return;
    };
    let fraction = (level / osd.max_level.max(f64::EPSILON)).clamp(0.0, 1.0);
    let percent = format!("{}%", (level * 100.0).round() as u32);
    let bar_w = 200.min(w.saturating_sub(160)) as i32;
    let caption_w = match (text, &caption) {
        (Some(t), Some(c)) => t.width(c, 14.0) as i32 + 12,
        _ => 0,
    };
    let x0 = (w as i32 - bar_w - 56 + caption_w) / 2;
    if let (Some(caption), true) = (&caption, caption_w > 0) {
        let base = baseline(text, 0, h as i32, 14.0);
        label(text, canvas, caption, (x0 - caption_w) as f32, base, 14.0, FG, caption_w as f32);
    }
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
        // Workspace dots, the active one highlighted.
        if self.windows.count > 1 {
            let y = h as i32 / 2 - WORKSPACE_DOT / 2;
            for i in 0..self.windows.count.min(36) as i32 {
                let x = APPS_BUTTON_W + 6 + i * (WORKSPACE_DOT + 6);
                let active = i as u32 == self.windows.active;
                let (w, color) = if active { (WORKSPACE_DOT * 2, FG) } else { (WORKSPACE_DOT, DIM_FG) };
                let x = if i as u32 > self.windows.active { x + WORKSPACE_DOT } else { x };
                canvas.rect(x, y, w, WORKSPACE_DOT, color);
            }
        }

        if self.sharing {
            // Left of the Power button: a red dot and "Sharing".
            let x = w as i32 - POWER_BUTTON_W - 96;
            canvas.rect(x, h as i32 / 2 - 4, 8, 8, SHARING);
            label(self.text.as_ref(), &mut canvas, "Sharing", (x + 14) as f32, base, px, SHARING, 80.0);
        }
        if let Some(text) = &self.text {
            let pw = text.width(POWER_LABEL, px);
            if self.power_menu.is_some() {
                canvas.rect(w as i32 - POWER_BUTTON_W + 4, 4, POWER_BUTTON_W - 8, h as i32 - 8, HOVER_BG);
            }
            text.draw(&mut canvas, POWER_LABEL, w as f32 - (POWER_BUTTON_W as f32 + pw) / 2.0, base, Style { px, color: FG, max_width: 72.0 });
        }
        // Keyboard focus ring (switch-panels).
        if let Some(control) = self.panel_focus {
            let (x, bw) = if control == 0 { (4, APPS_BUTTON_W - 8) } else { (w as i32 - POWER_BUTTON_W + 4, POWER_BUTTON_W - 8) };
            let (y, bh) = (3, h as i32 - 6);
            for (rx, ry, rw, rh) in [(x, y, bw, 2), (x, y + bh - 2, bw, 2), (x, y, 2, bh), (x + bw - 2, y, 2, bh)] {
                canvas.rect(rx, ry, rw, rh, ACCENT);
            }
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
            model: Launcher::with_windows(apps, self.windows.mru().to_vec()),
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
        match view.model.selected() {
            Some(launcher::Hit::App(i)) => {
                let app = &view.model.apps[i];
                tracing::info!(app = %app.id, "launching");
                if let Err(err) = launcher::launch(&app.id) {
                    tracing::warn!(%err, app = %app.id, "launch failed");
                }
            }
            Some(launcher::Hit::Window(i)) => {
                let id = view.model.windows[i].id;
                self.activate_window(id);
            }
            None => {}
        }
        self.close_launcher(true);
    }

    fn activate_window(&self, id: u32) {
        if let Some(list) = &self.window_list {
            tracing::debug!(id, "activating window");
            list.activate(id);
        }
    }

    // ----- Window switcher ---------------------------------------------------

    /// Move the open switcher's selection; false if none is open.
    fn step_switcher(&mut self, backward: bool) -> bool {
        let Some(open) = self.switcher.as_mut() else { return false };
        open.state.switcher.step(backward);
        self.draw_switcher();
        true
    }

    fn switch_windows(&mut self, backward: bool, hold: u32, mode: SwitchMode) {
        if self.step_switcher(backward) {
            return;
        }
        let names = if mode == SwitchMode::Apps { crate::windows::app_names() } else { HashMap::new() };
        let name = |app_id: &str| names.get(&app_id.to_lowercase()).cloned();
        let (apps_only_here, windows_only_here) = crate::windows::current_workspace_only();
        let only_here = if mode == SwitchMode::Apps { apps_only_here } else { windows_only_here };
        let only = only_here.then_some(self.windows.active);
        let Some(switcher) = Switcher::open(&self.windows, mode, backward, only, name) else { return };
        let kind = SwitchKind::Windows { apps: None };
        self.switcher_mode = Some(mode);
        self.open_switcher(switcher, hold, kind);
    }

    /// Into the selected application's windows (Down in the application
    /// switcher), or back (Up).
    fn switcher_drill(&mut self, into: bool) -> bool {
        if self.switcher_mode != Some(SwitchMode::Apps) {
            return false;
        }
        let only = crate::windows::current_workspace_only().1.then_some(self.windows.active);
        let Some(open) = self.switcher.as_mut() else { return false };
        let SwitchKind::Windows { apps } = &mut open.state.kind else { return false };
        match (into, apps.is_some()) {
            (true, false) => {
                let Some(windows) = Switcher::app_windows(&self.windows, open.state.switcher.selected_id(), only) else { return true };
                *apps = Some(std::mem::replace(&mut open.state.switcher, windows));
            }
            (false, true) => {
                open.state.switcher = apps.take().expect("checked");
            }
            _ => return false,
        }
        self.draw_switcher();
        true
    }

    /// `cycle-windows` / `cycle-group`: activate the next window directly.
    fn cycle_windows(&mut self, group: bool, backward: bool) {
        let now = Instant::now();
        let continuing = self
            .cycle
            .as_ref()
            .is_some_and(|c| c.group == group && now.duration_since(c.last) < CYCLE_CONTINUE);
        if !continuing {
            let only = crate::windows::current_workspace_only().1.then_some(self.windows.active);
            let order = crate::windows::cycle_order(&self.windows, group, only);
            if order.len() < 2 {
                self.cycle = None;
                return;
            }
            self.cycle = Some(Cycle { order, index: 0, group, last: now });
        }
        let Some(cycle) = self.cycle.as_mut() else { return };
        let n = cycle.order.len();
        cycle.index = if backward { (cycle.index + n - 1) % n } else { (cycle.index + 1) % n };
        cycle.last = now;
        let id = cycle.order[cycle.index];
        self.activate_window(id);
    }

    /// `toggle-on-all-workspaces` for the focused window.
    fn toggle_on_all_workspaces(&mut self) {
        let focused = self.windows.focused();
        let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 3) else { return };
        if focused == 0 {
            return;
        }
        let on_all = self.windows.mru().iter().any(|e| e.id == focused && e.workspace == crate::windows::ALL_WORKSPACES);
        list.set_on_all_workspaces(focused, u32::from(!on_all));
    }

    // ----- Panel keyboard focus (switch-panels) -------------------------------

    fn focus_panel(&mut self, backward: bool) {
        let next = match self.panel_focus {
            None if backward => PANEL_CONTROLS - 1,
            None => 0,
            Some(i) if backward => (i + PANEL_CONTROLS - 1) % PANEL_CONTROLS,
            Some(i) => (i + 1) % PANEL_CONTROLS,
        };
        if self.panel_focus.is_none() {
            self.close_launcher(true);
            self.power_menu = None;
            self.panel.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            self.panel.commit();
        }
        self.panel_focus = Some(next);
        self.draw_panel();
    }

    fn leave_panel(&mut self) {
        if self.panel_focus.take().is_some() {
            self.panel.set_keyboard_interactivity(KeyboardInteractivity::None);
            self.panel.commit();
            self.draw_panel();
        }
    }

    fn panel_key(&mut self, event: &KeyEvent) {
        match event.keysym {
            Keysym::Escape => self.leave_panel(),
            Keysym::Right | Keysym::Tab => self.focus_panel(false),
            Keysym::Left | Keysym::ISO_Left_Tab => self.focus_panel(true),
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                let control = self.panel_focus;
                self.leave_panel();
                match control {
                    Some(0) => self.open_launcher(true),
                    Some(_) => {
                        self.toggle_power_menu();
                        self.draw_panel();
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn switch_input_source(&mut self, backward: bool, hold: u32) {
        if self.step_switcher(backward) {
            return;
        }
        let sources = crate::input_sources::current();
        let entries = sources
            .iter()
            .enumerate()
            .map(|(i, s)| SwitchEntry {
                id: i as u32,
                label: crate::input_sources::description(&s.1).unwrap_or_else(|| s.1.clone()),
                detail: s.1.clone(),
            })
            .collect();
        let Some(switcher) = Switcher::new(entries, true, backward) else { return };
        self.open_switcher(switcher, hold, SwitchKind::InputSources(sources));
    }

    fn open_switcher(&mut self, switcher: Switcher, hold: u32, kind: SwitchKind) {
        if hold == 0 {
            // Nothing to let go of: switch at once.
            self.commit_switch(&kind, switcher.selected_id());
            return;
        }
        self.close_launcher(true);
        self.power_menu = None;
        let overlay = self.new_overlay("helix-switcher", KeyboardInteractivity::Exclusive, SwitcherState { switcher, hold, kind });
        self.switcher = Some(overlay);
    }

    fn commit_switch(&mut self, kind: &SwitchKind, id: u32) {
        match kind {
            SwitchKind::Windows { .. } => self.activate_window(id),
            SwitchKind::InputSources(sources) => {
                if let Some(source) = sources.get(id as usize) {
                    crate::input_sources::activate(source);
                }
            }
        }
    }

    /// Activate the selected entry (or just close, `commit` false).
    fn finish_switching(&mut self, commit: bool) {
        let Some(overlay) = self.switcher.take() else { return };
        if commit {
            self.commit_switch(&overlay.state.kind, overlay.state.switcher.selected_id());
        }
    }

    fn switcher_key(&mut self, event: &KeyEvent) {
        // The key above Tab (evdev KEY_GRAVE) or Down: an application's
        // windows; Up: back to the applications.
        const KEY_ABOVE_TAB: u32 = 41;
        if (event.keysym == Keysym::Down || event.raw_code == KEY_ABOVE_TAB) && self.switcher_drill(true) {
            return;
        }
        if event.keysym == Keysym::Up && self.switcher_drill(false) {
            return;
        }
        match event.keysym {
            Keysym::Escape => self.finish_switching(false),
            Keysym::Return | Keysym::KP_Enter => self.finish_switching(true),
            Keysym::Right | Keysym::Down | Keysym::Tab => {
                self.step_switcher(false);
            }
            Keysym::Left | Keysym::Up | Keysym::ISO_Left_Tab => {
                self.step_switcher(true);
            }
            _ => {}
        }
    }

    /// Modifiers changed while the switcher has the keyboard: letting go of
    /// the binding's modifier picks the selected window.
    fn switcher_modifiers(&mut self, modifiers: &Modifiers) {
        let Some(overlay) = &self.switcher else { return };
        let hold = overlay.state.hold;
        let held = (hold & 4 != 0 && modifiers.alt) || (hold & 8 != 0 && modifiers.logo) || (hold & 2 != 0 && modifiers.ctrl);
        if !held {
            self.finish_switching(true);
        }
    }

    fn draw_switcher(&mut self) {
        let Some(mut overlay) = self.switcher.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_switcher(&mut overlay);
        }
        self.switcher = Some(overlay);
    }

    fn paint_switcher(&mut self, overlay: &mut Overlay<SwitcherState>) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let Ok((buffer, data)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(0x00000000);
        let text = self.text.as_ref();
        let switcher = &overlay.state.switcher;
        let shown = switcher.entries.len().clamp(1, SWITCHER_ROWS);
        let first = switcher.selected.saturating_sub(shown - 1);
        let bw = 480.min(w as i32 - 40);
        let bh = shown as i32 * ROW_H + 16;
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 2;
        canvas.rect(bx - 1, by - 1, bw + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        overlay.hits.clear();
        for (row, index) in (first..switcher.entries.len()).take(shown).enumerate() {
            let entry = &switcher.entries[index];
            let r = Rect { x: bx + 8, y: by + 8 + row as i32 * ROW_H, w: bw - 16, h: ROW_H };
            if index == switcher.selected {
                canvas.rect(r.x, r.y, r.w, r.h, ACCENT);
            }
            let base = baseline(text, r.y, r.h, 15.0);
            let detail_w = text.map_or(0.0, |t| t.width(&entry.detail, 13.0).min(160.0));
            label(text, &mut canvas, &entry.label, (r.x + 12) as f32, base, 15.0, FG, r.w as f32 - 36.0 - detail_w);
            let detail_color = if index == switcher.selected { FG } else { DIM_FG };
            label(text, &mut canvas, &entry.detail, r.x as f32 + r.w as f32 - 12.0 - detail_w, base, 13.0, detail_color, 160.0);
            overlay.hits.push((r, OverlayHit::Window(entry.id)));
        }
        let surface = overlay.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
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
            ("Type to search…", DIM_FG)
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
            let (name, is_window) = view.model.row_label(view.model.results[result]);
            let base = baseline(self.text.as_ref(), rect.y, rect.h, 15.0);
            let tag_w = if is_window { 96.0 } else { 0.0 };
            label(self.text.as_ref(), &mut canvas, name, (rect.x + 12) as f32, base, 15.0, FG, rect.w as f32 - 24.0 - tag_w);
            if is_window {
                let tag = "Open window";
                let tw = self.text.as_ref().map_or(0.0, |t| t.width(tag, 13.0));
                let color = if result == view.model.selected { FG } else { DIM_FG };
                label(self.text.as_ref(), &mut canvas, tag, rect.x as f32 + rect.w as f32 - 12.0 - tw, base, 13.0, color, tag_w);
            }
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

    // ----- Lock screen -------------------------------------------------------

    fn start_lock(&mut self) {
        if self.lock.session.is_some() {
            return;
        }
        self.finish_pick(crate::screenshot::Picked::Cancelled);
        self.finish_switching(false);
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::ShareScreen { .. })) {
            self.answer_dialog(Choice::Canceled);
        }
        self.close_launcher(true);
        match self.lock.state.lock(&self.qh) {
            Ok(session) => self.lock.session = Some(session),
            Err(err) => {
                tracing::error!(%err, "compositor does not let this process lock the session");
                let _ = self.requests.send(UiRequest::LockGone);
            }
        }
    }

    fn lock_key(&mut self, event: &KeyEvent) {
        if self.lock.checking {
            return;
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit_password(),
            Keysym::Escape => {
                wipe(&mut self.lock.password);
                self.lock.message = None;
            }
            Keysym::BackSpace => {
                self.lock.password.pop();
            }
            _ => {
                if let Some(text) = &event.utf8 {
                    self.lock.password.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
        }
        self.draw_lock();
    }

    fn submit_password(&mut self) {
        if self.lock.password.is_empty() {
            return;
        }
        let secret = Secret::new(std::mem::take(&mut self.lock.password));
        self.lock.checking = true;
        self.lock.message = Some("Checking…".into());
        let auth = self.lock.auth.clone();
        let user = self.lock.user.clone();
        let results = self.lock.results.clone();
        std::thread::spawn(move || {
            let result = auth.authenticate(&user, &secret);
            drop(secret);
            let _ = results.send(result);
        });
    }

    fn password_checked(&mut self, result: Result<(), AuthError>) {
        self.lock.checking = false;
        match result {
            Ok(()) => {
                tracing::info!("unlocking");
                if let Some(session) = self.lock.session.take() {
                    session.unlock();
                }
                self.lock.surfaces.clear();
                self.lock.message = None;
                let _ = self.requests.send(UiRequest::Unlocked);
            }
            Err(AuthError::Denied) => {
                self.lock.message = Some("Incorrect password. Try again.".into());
                self.draw_lock();
            }
            Err(AuthError::Unavailable(err)) => {
                tracing::error!(%err, "cannot check passwords");
                self.lock.message = Some(format!("Cannot check password: {err}"));
                self.draw_lock();
            }
        }
    }

    fn draw_lock(&mut self) {
        let mut views = std::mem::take(&mut self.lock.surfaces);
        for view in &mut views {
            self.paint_lock(view);
        }
        self.lock.surfaces = views;
    }

    fn paint_lock(&mut self, view: &mut LockView) {
        let (w, h) = (view.width as usize, view.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        let Ok((buffer, data)) =
            self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate lock screen buffer");
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(0xFF101014);
        let text = self.text.as_ref();
        let center = |s: &str, px: f32| -> f32 { (w as f32 - text.map_or(0.0, |t| t.width(s, px))) / 2.0 };

        let clock = clock_text();
        let top = (h as i32) / 4;
        label(text, &mut canvas, &clock, center(&clock, 64.0), top as f32, 64.0, FG, w as f32);
        label(text, &mut canvas, &self.lock.user, center(&self.lock.user, 18.0), (top + 70) as f32, 18.0, DIM_FG, w as f32);

        let fw = 360.min(w as i32 - 40);
        let fx = (w as i32 - fw) / 2;
        let fy = top + 100;
        canvas.rect(fx, fy, fw, 44, FIELD_BG);
        let (shown, color) = if self.lock.password.is_empty() {
            ("Password".to_owned(), DIM_FG)
        } else {
            ("●".repeat(self.lock.password.chars().count()), FG)
        };
        let base = baseline(text, fy, 44, 17.0);
        label(text, &mut canvas, &shown, (fx + 14) as f32, base, 17.0, color, (fw - 28) as f32);
        if let Some(message) = &self.lock.message {
            label(text, &mut canvas, message, center(message, 15.0), (fy + 76) as f32, 15.0, DIM_FG, w as f32);
        }

        let surface = view.surface.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
        }
    }

    // ----- Notification banner ----------------------------------------------

    fn show_banner(&mut self, banner: Option<crate::notifications::Banner>) {
        let Some(data) = banner else {
            self.banner = None;
            return;
        };
        let height = banner_height(self.text.as_ref(), &data);
        if let Some(view) = self.banner.as_mut().filter(|v| v.data.id == data.id) {
            view.data = data;
            if view.height != height {
                view.layer.set_size(BANNER_W, height);
                view.layer.commit();
            } else {
                self.draw_banner();
            }
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Top, Some("helix-notification"), None);
        layer.set_anchor(Anchor::TOP);
        layer.set_margin(8, 0, 0, 0);
        layer.set_size(BANNER_W, height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.banner = Some(BannerView { data, layer, width: 0, height: 0, configured: false, hits: Vec::new() });
    }

    fn draw_banner(&mut self) {
        let Some(mut view) = self.banner.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_banner(&mut view);
        }
        self.banner = Some(view);
    }

    fn paint_banner(&mut self, view: &mut BannerView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let Ok((buffer, data)) =
            self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate banner buffer");
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        // Lighter than the desktop, with a thin border, so it stands out.
        canvas.fill(BANNER_BORDER);
        canvas.rect(1, 1, w as i32 - 2, h as i32 - 2, BANNER_BG);
        let text = self.text.as_ref();
        let inner = (w as i32 - 2 * BANNER_PAD) as f32;
        view.hits.clear();

        let mut y = BANNER_PAD;
        label(text, &mut canvas, &view.data.app_name, BANNER_PAD as f32, (y + 12) as f32, 12.0, DIM_FG, inner - 24.0);
        // Close button, top right.
        let close = Rect { x: w as i32 - BANNER_PAD - 18, y: y - 4, w: 22, h: 22 };
        label(text, &mut canvas, "×", (close.x + 5) as f32, (close.y + 17) as f32, 18.0, DIM_FG, 20.0);
        view.hits.push((close, BannerHit::Close));
        y += 16;
        label(text, &mut canvas, &view.data.summary, BANNER_PAD as f32, (y + 20) as f32, 16.0, FG, inner);
        y += 26;
        if let Some(t) = text {
            for line in t.wrap(&view.data.body, 14.0, inner, 2) {
                t.draw(&mut canvas, &line, BANNER_PAD as f32, (y + 15) as f32, Style { px: 14.0, color: DIM_FG, max_width: inner });
                y += 20;
            }
        }
        if !view.data.buttons.is_empty() {
            y += 8;
            let n = view.data.buttons.len() as i32;
            let gap = 8;
            let bw = (w as i32 - 2 * BANNER_PAD - gap * (n - 1)) / n;
            for (i, (key, button_label)) in view.data.buttons.iter().enumerate() {
                let r = Rect { x: BANNER_PAD + i as i32 * (bw + gap), y, w: bw, h: 32 };
                canvas.rect(r.x, r.y, r.w, r.h, BANNER_BORDER);
                let lw = text.map_or(0.0, |t| t.width(button_label, 14.0)).min((bw - 12) as f32);
                let base = baseline(text, r.y, r.h, 14.0);
                label(text, &mut canvas, button_label, r.x as f32 + (bw as f32 - lw) / 2.0, base, 14.0, FG, (bw - 12) as f32);
                view.hits.push((r, BannerHit::Button(key.clone())));
            }
        }
        // Anything else on the banner is its body.
        view.hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, BannerHit::Body));

        let surface = view.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    fn banner_click(&mut self, x: f64, y: f64) {
        let Some(view) = &self.banner else { return };
        let Some(hit) = view.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone()) else { return };
        let id = view.data.id;
        let request = match hit {
            BannerHit::Close => UiRequest::NotificationDismissed { id },
            BannerHit::Body => UiRequest::NotificationActivated { id, action: None },
            BannerHit::Button(key) => UiRequest::NotificationActivated { id, action: Some(key) },
        };
        let _ = self.requests.send(request);
    }

    // ----- Overlays: end-session dialog and power menu -----------------------

    fn new_overlay<T>(&mut self, namespace: &str, keyboard: KeyboardInteractivity, state: T) -> Overlay<T> {
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some(namespace.to_owned()), None);
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(keyboard);
        layer.set_size(0, 0);
        layer.commit();
        Overlay { layer, width: 0, height: 0, configured: false, hits: Vec::new(), state }
    }

    fn show_end_dialog(&mut self, dialog: Option<EndDialog>) {
        match dialog {
            Some(data) => {
                let remaining = data.seconds;
                self.show_dialog(DialogState { kind: DialogKind::End(data), remaining });
            }
            None => {
                if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::End(_))) {
                    self.dialog = None;
                }
            }
        }
    }

    fn show_display_change(&mut self, seconds: Option<u32>) {
        match seconds {
            Some(remaining) => self.show_dialog(DialogState { kind: DialogKind::DisplayChange, remaining }),
            None => {
                if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::DisplayChange)) {
                    self.dialog = None;
                }
            }
        }
    }

    fn show_dialog(&mut self, state: DialogState) {
        self.power_menu = None;
        self.close_launcher(true);
        match self.dialog.as_mut() {
            Some(open) => {
                // A question being replaced is answered "no".
                if let DialogKind::ShareScreen { reply, .. } = &mut open.state.kind {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(None);
                    }
                }
                open.state = state;
                self.draw_dialog();
            }
            None => {
                let overlay = self.new_overlay("helix-dialog", KeyboardInteractivity::Exclusive, state);
                self.dialog = Some(overlay);
            }
        }
    }

    fn answer_dialog(&mut self, choice: Choice) {
        let Some(dialog) = self.dialog.take() else { return };
        let request = match dialog.state.kind {
            DialogKind::End(_) => UiRequest::EndDialogAnswer(choice),
            DialogKind::DisplayChange => UiRequest::DisplayChangeAnswer { keep: choice == Choice::Confirmed },
            DialogKind::ShareScreen { sources, reply, .. } => {
                // Confirm (Enter) shares the first entry.
                let chosen = (choice == Choice::Confirmed).then(|| sources.first().map(|s| s.0.clone())).flatten();
                if let Some(reply) = reply {
                    let _ = reply.send(chosen);
                }
                return;
            }
        };
        let _ = self.requests.send(request);
    }

    /// A monitor was picked in the share dialog.
    fn share_monitor(&mut self, index: usize) {
        let Some(dialog) = self.dialog.take() else { return };
        if let DialogKind::ShareScreen { sources, reply: Some(reply), .. } = dialog.state.kind {
            let _ = reply.send(sources.get(index).map(|s| s.0.clone()));
        }
    }

    fn dialog_key(&mut self, event: &KeyEvent) {
        match event.keysym {
            Keysym::Escape => self.answer_dialog(Choice::Canceled),
            Keysym::Return | Keysym::KP_Enter => self.answer_dialog(Choice::Confirmed),
            _ => {}
        }
    }

    /// One second passed: count down. At zero, ending the session goes ahead
    /// (unless something blocks it) and a display change is reverted.
    fn dialog_tick(&mut self) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let on_timeout = match &dialog.state.kind {
            DialogKind::ShareScreen { .. } => return,
            DialogKind::End(data) if !data.blockers.is_empty() => return,
            DialogKind::End(_) => Choice::Confirmed,
            DialogKind::DisplayChange => Choice::Canceled,
        };
        dialog.state.remaining = dialog.state.remaining.saturating_sub(1);
        if dialog.state.remaining == 0 {
            self.answer_dialog(on_timeout);
        } else {
            self.draw_dialog();
        }
    }

    fn draw_dialog(&mut self) {
        let Some(mut overlay) = self.dialog.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_dialog(&mut overlay);
        }
        self.dialog = Some(overlay);
    }

    fn paint_dialog(&mut self, overlay: &mut Overlay<DialogState>) {
        if matches!(overlay.state.kind, DialogKind::ShareScreen { .. }) {
            self.paint_share_dialog(overlay);
            return;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let Ok((buffer, data)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let remaining = overlay.state.remaining;
        let no_blockers = Vec::new();
        let (title, message, cancel_label, confirm_label, blockers) = match &overlay.state.kind {
            DialogKind::End(d) => {
                let (title, verb) = match d.kind {
                    SHUTDOWN => ("Power Off", "Power Off"),
                    REBOOT => ("Restart", "Restart"),
                    _ => ("Log Out", "Log Out"),
                };
                let confirm = if d.blockers.is_empty() { verb.to_owned() } else { format!("{verb} Anyway") };
                let message = format!("The session will end automatically in {remaining} seconds.");
                (title, message, "Cancel", confirm, &d.blockers)
            }
            DialogKind::ShareScreen { .. } => return,
            DialogKind::DisplayChange => (
                "Keep the new display settings?",
                format!("The previous settings come back in {remaining} seconds."),
                "Revert",
                "Keep".to_owned(),
                &no_blockers,
            ),
        };
        let blocked = !blockers.is_empty();
        let bw = 460.min(w as i32 - 40);
        let rows = blockers.len().min(6) as i32;
        let bh = if blocked { 150 + 26 + rows * 40 } else { 160 };
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 3;
        canvas.rect(bx - 1, by - 1, bw + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        let inner = (bw - 48) as f32;
        let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
        label(text, &mut canvas, title, center(title, 20.0), (by + 44) as f32, 20.0, FG, inner);
        let mut y = by + 78;
        if blocked {
            let msg = "Some applications are busy or have unsaved work:";
            label(text, &mut canvas, msg, (bx + 24) as f32, y as f32, 14.0, DIM_FG, inner);
            y += 16;
            for (app, reason) in blockers.iter().take(6) {
                label(text, &mut canvas, app, (bx + 24) as f32, (y + 18) as f32, 15.0, FG, inner);
                label(text, &mut canvas, reason, (bx + 24) as f32, (y + 36) as f32, 13.0, DIM_FG, inner);
                y += 40;
            }
        } else {
            label(text, &mut canvas, &message, center(&message, 14.0), y as f32, 14.0, DIM_FG, inner);
        }
        // Buttons: Cancel and the action.
        let by2 = by + bh - 56;
        let half = (bw - 24 * 2 - 12) / 2;
        let cancel = Rect { x: bx + 24, y: by2, w: half, h: 36 };
        let confirm = Rect { x: bx + 24 + half + 12, y: by2, w: half, h: 36 };
        canvas.rect(cancel.x, cancel.y, cancel.w, cancel.h, BANNER_BORDER);
        canvas.rect(confirm.x, confirm.y, confirm.w, confirm.h, ACCENT);
        for (r, s) in [(cancel, cancel_label), (confirm, confirm_label.as_str())] {
            let lw = text.map_or(0.0, |t| t.width(s, 15.0));
            let base = baseline(text, r.y, r.h, 15.0);
            label(text, &mut canvas, s, r.x as f32 + (r.w as f32 - lw) / 2.0, base, 15.0, FG, r.w as f32);
        }
        overlay.hits = vec![(cancel, OverlayHit::Cancel), (confirm, OverlayHit::Confirm)];
        let surface = overlay.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// "Share your screen?": one row per monitor, and Cancel.
    fn paint_share_dialog(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::ShareScreen { app, sources, remember, .. } = &overlay.state.kind else { return };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let Ok((buffer, data)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let rows = sources.len().min(8) as i32;
        let bw = 460.min(w as i32 - 40);
        let bh = 140 + rows * (ROW_H + 8) + 56 + if *remember { 12 } else { 0 };
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 3;
        canvas.rect(bx - 1, by - 1, bw + 2, bh + 2, BANNER_BORDER);
        canvas.rect(bx, by, bw, bh, BANNER_BG);
        let inner = (bw - 48) as f32;
        let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
        let title = "Share your screen?";
        label(text, &mut canvas, title, center(title, 20.0), (by + 44) as f32, 20.0, FG, inner);
        let message = format!("{app} wants to see your screen. Choose what to share.");
        label(text, &mut canvas, &message, center(&message, 14.0), (by + 78) as f32, 14.0, DIM_FG, inner);
        if *remember {
            let note = "It will be able to share the same screen again without asking.";
            label(text, &mut canvas, note, center(note, 13.0), (by + 96) as f32, 13.0, DIM_FG, inner);
        }
        let mut hits = Vec::new();
        let top = if *remember { by + 112 } else { by + 100 };
        for (i, (_, name, connector)) in sources.iter().take(8).enumerate() {
            let r = Rect { x: bx + 24, y: top + i as i32 * (ROW_H + 8), w: bw - 48, h: ROW_H };
            canvas.rect(r.x, r.y, r.w, r.h, HOVER_BG);
            let base = baseline(text, r.y, r.h, 15.0);
            label(text, &mut canvas, name, (r.x + 12) as f32, base, 15.0, FG, r.w as f32 - 120.0);
            let cw = text.map_or(0.0, |t| t.width(connector, 13.0).min(100.0));
            label(text, &mut canvas, connector, r.x as f32 + r.w as f32 - 12.0 - cw, base, 13.0, DIM_FG, 100.0);
            hits.push((r, OverlayHit::Monitor(i)));
        }
        let cancel = Rect { x: bx + 24, y: by + bh - 56, w: bw - 48, h: 36 };
        canvas.rect(cancel.x, cancel.y, cancel.w, cancel.h, BANNER_BORDER);
        let lw = text.map_or(0.0, |t| t.width("Cancel", 15.0));
        label(text, &mut canvas, "Cancel", cancel.x as f32 + (cancel.w as f32 - lw) / 2.0, baseline(text, cancel.y, cancel.h, 15.0), 15.0, FG, cancel.w as f32);
        hits.push((cancel, OverlayHit::Cancel));
        overlay.hits = hits;
        let surface = overlay.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// One label per output whose connector is in `labels`, top left below
    /// the panel. Labels take no input.
    fn show_monitor_labels(&mut self, labels: &HashMap<String, i32>) {
        self.monitor_labels.clear();
        for output in self.output_state.outputs() {
            let Some(name) = self.output_state.info(&output).and_then(|i| i.name) else { continue };
            let Some(&number) = labels.get(&name) else { continue };
            let surface = self.compositor.create_surface(&self.qh);
            if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
                surface.set_input_region(Some(region.wl_region()));
            }
            let layer = self.layer_shell.create_layer_surface(
                &self.qh,
                surface,
                Layer::Overlay,
                Some("helix-monitor-label"),
                Some(&output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::LEFT);
            layer.set_margin(PANEL_HEIGHT as i32 + 12, 0, 0, 12);
            layer.set_size(MONITOR_LABEL_SIZE, MONITOR_LABEL_SIZE);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            tracing::debug!(name, number, "monitor label");
            self.monitor_labels.push(Overlay {
                layer,
                width: 0,
                height: 0,
                configured: false,
                hits: Vec::new(),
                state: number,
            });
        }
    }

    fn draw_monitor_label(&mut self, index: usize) {
        let Some(overlay) = self.monitor_labels.get(index) else { return };
        if !overlay.configured || overlay.width == 0 {
            return;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let Ok((buffer, data)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas { data, width: w, height: h };
        canvas.fill(BANNER_BORDER);
        canvas.rect(2, 2, w as i32 - 4, h as i32 - 4, BANNER_BG);
        let text = self.text.as_ref();
        let number = overlay.state.to_string();
        let px = 40.0;
        let width = text.map(|t| t.width(&number, px)).unwrap_or(0.0);
        let base = baseline(text, 0, h as i32, px);
        label(text, &mut canvas, &number, (w as f32 - width) / 2.0, base, px, FG, w as f32);
        let surface = overlay.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    fn toggle_power_menu(&mut self) {
        if self.power_menu.take().is_some() {
            return;
        }
        self.close_launcher(true);
        let overlay = self.new_overlay("helix-power-menu", KeyboardInteractivity::OnDemand, ());
        self.power_menu = Some(overlay);
    }

    fn draw_power_menu(&mut self) {
        let Some(mut overlay) = self.power_menu.take() else { return };
        if overlay.configured && overlay.width > 0 {
            let (w, h) = (overlay.width as usize, overlay.height as usize);
            if let Ok((buffer, data)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888) {
                let mut canvas = Canvas { data, width: w, height: h };
                canvas.fill(0x00000000);
                let text = self.text.as_ref();
                let items = power_menu_items(self.power);
                let mw = 200;
                let mx = w as i32 - mw - 6;
                let my = PANEL_HEIGHT as i32 + 4;
                let mh = items.len() as i32 * 36 + 8;
                canvas.rect(mx - 1, my - 1, mw + 2, mh + 2, BANNER_BORDER);
                canvas.rect(mx, my, mw, mh, BANNER_BG);
                overlay.hits.clear();
                for (i, (name, action)) in items.iter().enumerate() {
                    let r = Rect { x: mx + 4, y: my + 4 + i as i32 * 36, w: mw - 8, h: 36 };
                    let base = baseline(text, r.y, r.h, 15.0);
                    label(text, &mut canvas, name, (r.x + 12) as f32, base, 15.0, FG, (r.w - 24) as f32);
                    overlay.hits.push((r, OverlayHit::Power(*action)));
                }
                overlay.hits.push((Rect { x: 0, y: 0, w: w as i32, h: h as i32 }, OverlayHit::Outside));
                let surface = overlay.layer.wl_surface();
                surface.damage_buffer(0, 0, w as i32, h as i32);
                if buffer.attach_to(surface).is_ok() {
                    overlay.layer.commit();
                }
            }
        }
        self.power_menu = Some(overlay);
    }

    fn handle(&mut self, event: UiEvent) {
        match event {
            UiEvent::EndDialog(dialog) => self.show_end_dialog(dialog),
            UiEvent::DisplayChange(seconds) => self.show_display_change(seconds),
            UiEvent::ShareScreen(slot) => {
                if let Some(q) = slot.take() {
                    if self.lock.session.is_some() {
                        let _ = q.reply.send(None);
                    } else {
                        use crate::screencast_chooser::Shared;
                        let mut sources: Vec<(Shared, String, String)> = q
                            .monitors
                            .into_iter()
                            .map(|(connector, name)| (Shared::Monitor(connector.clone()), name, connector))
                            .collect();
                        if q.windows {
                            sources.extend(self.windows.mru().iter().map(|w| (Shared::Window(w.id), w.label().to_owned(), w.app_id.clone())));
                        }
                        if sources.is_empty() {
                            let _ = q.reply.send(None);
                        } else {
                            let kind = DialogKind::ShareScreen { app: q.app, sources, remember: q.remember, reply: Some(q.reply) };
                            self.show_dialog(DialogState { kind, remaining: 0 });
                        }
                    }
                }
            }
            UiEvent::Sharing(on) => {
                self.sharing = on;
                self.draw_panel();
            }
            UiEvent::Screenshot(slot) => {
                if let Some(job) = slot.take() {
                    self.screenshot_job(job);
                }
            }
            UiEvent::PowerAvailability(power) => self.power = power,
            UiEvent::Lock => self.start_lock(),
            UiEvent::Banner(banner) => self.show_banner(banner),
            UiEvent::Osd(osd) => {
                self.osd = Some((osd, Instant::now() + OSD_DURATION));
                self.draw_panel();
            }
            UiEvent::Overview(true) => self.open_launcher(false),
            UiEvent::Overview(false) => self.close_launcher(false),
            UiEvent::MonitorLabels(labels) => self.show_monitor_labels(&labels),
            UiEvent::HideMonitorLabels => self.monitor_labels.clear(),
            UiEvent::Bind { action, accelerator, when_locked } => {
                self.bind(action, &accelerator);
                if when_locked {
                    if let Some(binding) = self.bindings.get(&action).filter(|b| b.version() >= 2) {
                        binding.allow_when_locked();
                    }
                }
            }
            UiEvent::Unbind { action } => {
                if let Some(binding) = self.bindings.remove(&action) {
                    binding.destroy();
                }
            }
        }
    }

    /// Bind the keys the shell handles itself.
    fn bind_internal(&mut self) {
        let mut next = INTERNAL_ACTIONS;
        let parse = |text: &str, what: &str| {
            let parsed = crate::accel::Accelerator::parse(text);
            if parsed.is_none() {
                tracing::warn!(text, what, "cannot parse key");
            }
            parsed
        };
        // Modifiers to hold while switching (Shift only means backward).
        let hold = |a: &crate::accel::Accelerator| a.to_wire().map(|(mods, _)| mods & !1).unwrap_or(0);
        for (text, backward) in crate::input_sources::accelerators() {
            let Some(accelerator) = parse(&text, "input source switching") else { continue };
            self.internal.insert(next, Internal::InputSource { backward, hold: hold(&accelerator) });
            self.bind(next, &accelerator);
            next += 1;
        }
        for (text, action) in crate::shell_keys::accelerators() {
            let Some(accelerator) = parse(&text, "shell key") else { continue };
            self.internal.insert(next, Internal::ShellKey(action));
            self.bind(next, &accelerator);
            if action.works_when_locked() {
                if let Some(binding) = self.bindings.get(&next).filter(|b| b.version() >= 2) {
                    binding.allow_when_locked();
                }
            }
            next += 1;
        }
        if self.window_list.is_none() {
            return;
        }
        for (text, backward, mode) in crate::windows::accelerators() {
            let Some(accelerator) = parse(&text, "window switching") else { continue };
            self.internal.insert(next, Internal::SwitchWindows { backward, hold: hold(&accelerator), mode });
            self.bind(next, &accelerator);
            next += 1;
        }
        for (text, key) in crate::windows::wm_key_accelerators() {
            let Some(accelerator) = parse(&text, "window management") else { continue };
            self.internal.insert(next, Internal::Wm(key));
            self.bind(next, &accelerator);
            next += 1;
        }
        if self.window_list.as_ref().is_some_and(|l| l.version() >= 2) {
            for (text, key) in crate::windows::workspace_accelerators() {
                let Some(accelerator) = parse(&text, "workspace") else { continue };
                self.internal.insert(next, Internal::Workspace(key));
                self.bind(next, &accelerator);
                next += 1;
            }
        }
    }

    fn internal_pressed(&mut self, action: u32) {
        match self.internal.get(&action).copied() {
            Some(Internal::InputSource { backward, hold }) => self.switch_input_source(backward, hold),
            Some(Internal::SwitchWindows { backward, hold, mode }) => self.switch_windows(backward, hold, mode),
            Some(Internal::ShellKey(key)) => {
                let _ = self.requests.send(UiRequest::ShellKey(key));
            }
            Some(Internal::Workspace(key)) => self.workspace_key(key),
            Some(Internal::Wm(crate::windows::WmKey::Cycle { group, backward })) => self.cycle_windows(group, backward),
            Some(Internal::Wm(crate::windows::WmKey::Panels { backward })) => self.focus_panel(backward),
            Some(Internal::Wm(crate::windows::WmKey::ToggleOnAllWorkspaces)) => self.toggle_on_all_workspaces(),
            None => {}
        }
    }

    /// Switch workspace, or move the focused window and switch with it.
    fn workspace_key(&mut self, key: crate::windows::WorkspaceKey) {
        let (Some(list), Some(target)) = (&self.window_list, self.windows.workspace_target(key.target)) else { return };
        if key.move_window {
            let focused = self.windows.focused();
            if focused == 0 {
                return;
            }
            list.move_window(focused, target);
        }
        tracing::debug!(target, moving = key.move_window, "workspace key");
        list.switch_workspace(target);
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
        self.dialog_tick();
        let osd_expired = self.osd.as_ref().is_some_and(|(_, until)| Instant::now() >= *until);
        let minute_changed =
            self.osd.is_none() && self.shown_minute.as_deref() != Some(&clock_text());
        if osd_expired || minute_changed {
            self.draw_panel();
        }
    }
}

/// The top panel (on the primary monitor: no output is named).
fn make_panel(compositor: &CompositorState, layer_shell: &LayerShell, qh: &QueueHandle<Ui>) -> LayerSurface {
    let surface = compositor.create_surface(qh);
    let panel = layer_shell.create_layer_surface(qh, surface, Layer::Top, Some("helix-panel"), None);
    panel.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    panel.set_size(0, PANEL_HEIGHT);
    panel.set_exclusive_zone(PANEL_HEIGHT as i32);
    panel.set_keyboard_interactivity(KeyboardInteractivity::None);
    panel.commit();
    panel
}

/// Run the UI until the compositor goes away. Blocks the calling thread.
pub fn run(
    events: Channel<UiEvent>,
    requests: UnboundedSender<UiRequest>,
    auth: Arc<dyn Authenticator>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<Ui> = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    // Only advertised to the shell the compositor launched itself.
    let keybindings = globals.bind::<HelixKeybindingsManagerV1, _, _>(&qh, 1..=2, ()).ok();
    let window_list = globals.bind::<HelixWindowListV1, _, _>(&qh, 1..=3, ()).ok();
    let screenshots = globals.bind::<HelixScreenshotManagerV1, _, _>(&qh, 1..=2, ()).ok();
    if keybindings.is_none() {
        tracing::warn!("compositor offers no key bindings to this process; accelerators will not fire");
    }

    let panel = make_panel(&compositor, &layer_shell, &qh);

    let text = Text::load();
    if text.is_none() {
        tracing::warn!("no system font found; text is limited to the clock");
    }

    let pool = SlotPool::new((1920 * PANEL_HEIGHT * 4) as usize, &shm)?;
    let (auth_tx, auth_rx) = channel::channel();
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
        internal: HashMap::new(),
        window_list,
        windows: Default::default(),
        switcher: None,
        switcher_mode: None,
        cycle: None,
        panel_focus: None,
        screenshots,
        picker: None,
        flashes: Vec::new(),
        loop_handle: None,
        banner: None,
        dialog: None,
        power_menu: None,
        monitor_labels: Vec::new(),
        power: Default::default(),
        sharing: false,
        lock: LockScreen {
            state: SessionLockState::new(&globals, &qh),
            session: None,
            surfaces: Vec::new(),
            password: String::new(),
            message: None,
            checking: false,
            auth,
            user: crate::auth::current_user(),
            results: auth_tx,
        },
        exit: false,
    };

    ui.bind_internal();

    let mut event_loop: EventLoop<Ui> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    ui.loop_handle = Some(handle.clone());
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
        .insert_source(auth_rx, |event, _, ui| {
            if let channel::Event::Msg(result) = event {
                ui.password_checked(result);
            }
        })
        .map_err(|e| format!("cannot watch password checks: {e}"))?;
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
        if self.picker_closed(layer) || self.flash_closed(layer) {
            return;
        }
        if layer == &self.panel {
            // Its monitor went away: the compositor puts a new one on the
            // primary monitor.
            tracing::info!("panel closed; making a new one");
            self.panel = make_panel(&self.compositor, &self.layer_shell, &self.qh);
            self.panel_configured = false;
        } else if self.banner.as_ref().is_some_and(|b| &b.layer == layer) {
            self.banner = None;
        } else if self.dialog.as_ref().is_some_and(|d| &d.layer == layer) {
            self.answer_dialog(Choice::Canceled);
        } else if self.power_menu.as_ref().is_some_and(|m| &m.layer == layer) {
            self.power_menu = None;
        } else if self.switcher.as_ref().is_some_and(|o| &o.layer == layer) {
            self.switcher = None;
        } else if self.monitor_labels.iter().any(|l| &l.layer == layer) {
            self.monitor_labels.retain(|l| &l.layer != layer);
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
        if self.picker_configured(layer, configure.new_size) || self.flash_configured(layer, configure.new_size) {
            return;
        }
        if layer == &self.panel {
            if configure.new_size.0 > 0 {
                self.panel_width = configure.new_size.0;
            }
            self.panel_configured = true;
            self.draw_panel();
        } else if let Some(overlay) = self.dialog.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_dialog();
        } else if let Some(overlay) = self.power_menu.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_power_menu();
            self.draw_panel();
        } else if let Some(overlay) = self.switcher.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_switcher();
        } else if let Some(index) = self.monitor_labels.iter().position(|l| &l.layer == layer) {
            let overlay = &mut self.monitor_labels[index];
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_monitor_label(index);
        } else if let Some(view) = self.banner.as_mut().filter(|v| &v.layer == layer) {
            view.width = configure.new_size.0;
            view.height = configure.new_size.1;
            view.configured = true;
            self.draw_banner();
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
        if self.lock.session.is_some() {
            self.lock_key(&event);
        } else if self.picker.is_some() {
            self.picker_key(&event);
        } else if self.dialog.is_some() {
            self.dialog_key(&event);
        } else if self.switcher.is_some() {
            self.switcher_key(&event);
        } else if self.panel_focus.is_some() {
            self.panel_key(&event);
        } else if self.power_menu.is_some() && event.keysym == Keysym::Escape {
            self.power_menu = None;
            self.draw_panel();
        } else {
            self.launcher_key(&event);
        }
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if self.lock.session.is_some() {
            self.lock_key(&event);
        } else {
            self.launcher_key(&event);
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: RawModifiers, _: u32) {
        if self.switcher.is_some() {
            self.switcher_modifiers(&modifiers);
        }
    }
}

impl PointerHandler for Ui {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            if self.picker_pointer(event) {
                continue;
            }
            let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind else {
                continue;
            };
            let (x, y) = event.position;
            let hit_in = |hits: &[(Rect, OverlayHit)]| hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h);
            if let Some(hit) = self.dialog.as_ref().filter(|d| d.layer.wl_surface() == &event.surface).and_then(|d| hit_in(&d.hits)) {
                match hit {
                    OverlayHit::Confirm => self.answer_dialog(Choice::Confirmed),
                    OverlayHit::Cancel => self.answer_dialog(Choice::Canceled),
                    OverlayHit::Monitor(index) => self.share_monitor(index),
                    _ => {}
                }
            } else if self.switcher.as_ref().is_some_and(|o| o.layer.wl_surface() == &event.surface) {
                let hit = self.switcher.as_ref().and_then(|o| hit_in(&o.hits));
                if let Some(OverlayHit::Window(id)) = hit {
                    if let Some(open) = self.switcher.as_mut() {
                        if let Some(pos) = open.state.switcher.entries.iter().position(|e| e.id == id) {
                            open.state.switcher.selected = pos;
                        }
                    }
                    self.finish_switching(true);
                } else {
                    self.finish_switching(false);
                }
            } else if self.power_menu.as_ref().is_some_and(|m| m.layer.wl_surface() == &event.surface) {
                let hit = self.power_menu.as_ref().and_then(|m| hit_in(&m.hits));
                self.power_menu = None;
                if let Some(OverlayHit::Power(action)) = hit {
                    tracing::info!(?action, "power menu");
                    let _ = self.requests.send(UiRequest::Power(action));
                }
                self.draw_panel();
            } else if self.banner.as_ref().is_some_and(|b| b.layer.wl_surface() == &event.surface) {
                self.banner_click(x, y);
            } else if &event.surface == self.panel.wl_surface() {
                if x >= f64::from(self.panel_width as i32 - POWER_BUTTON_W) {
                    self.toggle_power_menu();
                    self.draw_panel();
                } else if x < f64::from(APPS_BUTTON_W) {
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
                if action < INTERNAL_ACTIONS {
                    let _ = ui.requests.send(UiRequest::GrabFailed { action });
                }
            }
            helix_keybinding_v1::Event::Pressed { .. } if action >= INTERNAL_ACTIONS => ui.internal_pressed(action),
            helix_keybinding_v1::Event::Released { .. } if action >= INTERNAL_ACTIONS => {}
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

impl SessionLockHandler for Ui {
    fn locked(&mut self, _: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        // One lock surface per output.
        for output in self.output_state.outputs() {
            let surface = self.compositor.create_surface(qh);
            let lock_surface = session_lock.create_lock_surface(surface, &output, qh);
            self.lock.surfaces.push(LockView { surface: lock_surface, width: 0, height: 0 });
        }
        tracing::info!(surfaces = self.lock.surfaces.len(), "session locked");
        let _ = self.requests.send(UiRequest::Locked);
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        // Refused by the compositor (or the lock was taken away).
        tracing::warn!("session lock finished by the compositor");
        self.lock.session = None;
        self.lock.surfaces.clear();
        wipe(&mut self.lock.password);
        let _ = self.requests.send(UiRequest::LockGone);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let mut views = std::mem::take(&mut self.lock.surfaces);
        if let Some(view) = views.iter_mut().find(|v| v.surface.wl_surface() == surface.wl_surface()) {
            view.width = configure.new_size.0;
            view.height = configure.new_size.1;
            self.paint_lock(view);
        }
        self.lock.surfaces = views;
    }
}

/// Action ids from here up are the shell's own bindings, never handed out
/// to `GrabAccelerator` callers.
const INTERNAL_ACTIONS: u32 = 0xFFFF_0000;

/// What a shell-internal binding does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Internal {
    /// Open a switcher or move its selection. `hold`: modifier bits
    /// (keybinding wire values) whose release picks the entry.
    InputSource { backward: bool, hold: u32 },
    SwitchWindows { backward: bool, hold: u32, mode: SwitchMode },
    /// Screenshot, brightness and volume keys (run on the D-Bus side).
    ShellKey(crate::shell_keys::KeyAction),
    Workspace(crate::windows::WorkspaceKey),
    Wm(crate::windows::WmKey),
}

/// Presses this close continue a window cycle.
const CYCLE_CONTINUE: Duration = Duration::from_millis(1500);
/// Panel controls reachable with `switch-panels`: Apps, Power.
const PANEL_CONTROLS: usize = 2;

struct Cycle {
    order: Vec<u32>,
    index: usize,
    group: bool,
    last: Instant,
}

/// What an open switcher switches.
enum SwitchKind {
    /// Windows or applications; `apps` is the application switcher we
    /// went into an application's windows from (Up goes back).
    Windows { apps: Option<Switcher> },
    /// The sources listed, by entry id.
    InputSources(Vec<crate::input_sources::Source>),
}

struct SwitcherState {
    switcher: Switcher,
    hold: u32,
    kind: SwitchKind,
}

const SWITCHER_ROWS: usize = 10;

/// Power menu entries, offering only what logind allows.
fn power_menu_items(power: crate::logind::Availability) -> Vec<(&'static str, PowerAction)> {
    let mut items = vec![("Lock Screen", PowerAction::Lock)];
    if power.suspend {
        items.push(("Suspend", PowerAction::Suspend));
    }
    items.push(("Log Out…", PowerAction::LogOut));
    if power.restart {
        items.push(("Restart…", PowerAction::Restart));
    }
    if power.power_off {
        items.push(("Power Off…", PowerAction::PowerOff));
    }
    items
}


impl Dispatch<HelixWindowListV1, ()> for Ui {
    fn event(
        ui: &mut Self,
        _: &HelixWindowListV1,
        event: helix_window_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            helix_window_list_v1::Event::Window { id, app_id, title } => ui.windows.update(id, app_id, title),
            helix_window_list_v1::Event::Closed { id } => {
                ui.windows.close(id);
                // A window that went away can't be switched to.
                if let Some(open) = ui.switcher.as_mut().filter(|o| matches!(o.state.kind, SwitchKind::Windows { .. })) {
                    let entries = &mut open.state.switcher.entries;
                    entries.retain(|e| e.id != id);
                    if entries.len() < 2 {
                        ui.switcher = None;
                    } else {
                        open.state.switcher.selected = open.state.switcher.selected.min(entries.len() - 1);
                        ui.draw_switcher();
                    }
                }
            }
            helix_window_list_v1::Event::Focused { id } => ui.windows.focus(id),
            helix_window_list_v1::Event::Workspaces { count, active } => {
                ui.windows.set_workspaces(count, active);
                ui.draw_panel();
            }
            helix_window_list_v1::Event::Workspace { id, workspace } => ui.windows.set_workspace(id, workspace),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logind::Availability;

    #[test]
    fn power_menu_entries_follow_logind() {
        let names = |power| power_menu_items(power).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(Availability::default()), ["Lock Screen", "Log Out…"]);
        let all = Availability { suspend: true, restart: true, power_off: true };
        assert_eq!(names(all), ["Lock Screen", "Suspend", "Log Out…", "Restart…", "Power Off…"]);
    }
}
