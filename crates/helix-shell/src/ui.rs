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
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface, wl_touch,
};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::touch::TouchHandler;
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
use crate::text::{physical, Canvas, Style, Text};
use crate::auth::{AuthError, Authenticator, Secret};
use crate::end_dialog::{Choice, EndDialog, REBOOT, SHUTDOWN, UPDATE_REBOOT, UPGRADE_REBOOT};
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
use crate::windows::{SwitchEntry, SwitchMode, Switcher, WindowAction};
use helix_draw::motion::{Motion, Tween};
use helix_draw::theme;
use helix_draw::theme::size;
use std::collections::HashMap;
use helix_i18n::tr;
use helix_i18n::trf;

mod app_grid;
mod atspi;
#[cfg(test)]
mod atspi_bus;
mod background;
mod break_screen;
mod calendar;
mod capture;
mod clipboard;
mod click_type;
mod dash;
mod desktop_icons;
mod dock;
mod drops;
mod ext_items;
mod extensions_dialog;
#[cfg(test)]
mod gallery;
mod ime;
mod notify;
mod offload;
mod osd;
use offload::source_icon;
mod osk;
mod overview;
mod panel_editor;
mod panel_tooltip;
mod press;
mod prompt;
mod quick;
mod lock;
mod lock_extras;
mod mirror;
mod scale;
mod status;
mod tray;
mod window_menu;
mod workspace_popup;
use notify::BannerHit;

/// The panel's height at text scale 1 (see [`panel_height`]).
pub const PANEL_HEIGHT: u32 = 32;

/// The panel's height: it grows with the text scale.
pub fn panel_height() -> u32 {
    helix_draw::theme::grow(PANEL_HEIGHT as i32) as u32
}

const OSD_DURATION: Duration = Duration::from_secs(2);
const BTN_LEFT: u32 = 0x110;

// The shell's colors (helix_draw::theme, under the names its surfaces
// use).
const PANEL_BG: u32 = theme::PANEL;
const FG: u32 = theme::TEXT;
/// Secondary text (on the panel, popovers and dialogs; not on controls).
const DIM_FG: u32 = theme::TEXT_DIM;
// The accent follows `accent-color` (helix_draw::accent), so it is read
// at each use.
/// The accent: rings, indicators, a slider's filled part.
fn accent() -> u32 {
    theme::accent()
}
/// The accent under text (selected rows, buttons, switches on).
fn accent_fill() -> u32 {
    theme::accent_fill()
}
/// Text and symbols on [`accent_fill`]: dark on a light accent.
fn on_accent() -> u32 {
    theme::on_accent()
}
/// Accent-colored words (links, "Connected").
fn accent_text() -> u32 {
    theme::text_accent()
}
/// Placeholders and labels of what cannot be used now: dimmed, and in
/// high contrast (where all text is white) white at 60 %.
fn hint_text() -> u32 {
    window_menu::disabled_text()
}
/// Words in red: errors, "Sharing" and "Recording".
const ERROR_TEXT: u32 = theme::TEXT_ERROR;
const SCRIM: u32 = theme::SCRIM;
/// Screen sharing indicator (its dot; its words are `ERROR_TEXT`).
const SHARING: u32 = theme::ERROR;

const APPS_LABEL: &str = "Apps";
/// The launcher's result rows and section headings, and the rows' icons
/// (logical px).
const RESULT_H: i32 = 48;
const HEADING_H: i32 = 30;
const RESULT_ICON: i32 = 32;
const ROW_H: i32 = 40;
const MAX_ROWS: usize = 10;

#[derive(Debug, Clone, Copy, Default)]
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
    /// The app grid (shown with an empty search).
    grid: app_grid::GridView,
    /// The monitor it is shown on (from the compositor).
    output: Option<wl_output::WlOutput>,
    /// The result row under the pointer, and the one pressed
    /// ([`CLEAR_HIT`]: the clear button).
    hover: Option<usize>,
    pressed: Option<usize>,
    /// The search field's clear button, while there is a query.
    clear: Option<Rect>,
    /// The app grid as last painted (to repaint only cells that change).
    grid_cache: Option<app_grid::GridCache>,
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
    scales: scale::Scales,

    panel: LayerSurface,
    panel_width: u32,
    panel_configured: bool,
    launcher: Option<LauncherView>,
    /// The interface font (`font-name`'s family; the system font
    /// without one): its regular face, and its bold one (titles,
    /// headings, the clock, bold markup).
    text: Option<Text>,
    bold: Option<Text>,
    font_family: Option<String>,
    /// The face of big clocks (light, else bold).
    display: Option<Text>,
    notify: notify::NotifyState,
    /// The message tray's calendar column.
    calendar: calendar::CalendarState,
    /// The on-screen keyboard.
    osk: osk::OskState,
    click_type: click_type::ClickTypeState,
    desktop_icons: desktop_icons::DesktopIconsState,
    osd: Option<(Osd, Instant)>,
    /// The OSD's surfaces, one per monitor it shows on.
    osd_views: Vec<osd::OsdView>,
    shown_minute: Option<String>,
    /// The lock screen clock's text when it was drawn.
    shown_lock_clock: Option<String>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Unclamped pointer motion (pressure at the dock's edge).
    relative_state: smithay_client_toolkit::seat::relative_pointer::RelativePointerState,
    relative_pointer: Option<smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    touch: Option<wl_touch::WlTouch>,
    /// The touch point taken as a click: id, surface, last position.
    touch_point: Option<(i32, wl_surface::WlSurface, (f64, f64))>,
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
    /// The panel item under the pointer, and the one pressed on it.
    panel_lit: Option<crate::panel_layout::Slot>,
    panel_pressed: Option<crate::panel_layout::Slot>,
    /// The next menu opened was asked for with a key: its first entry
    /// starts selected (taken by `open_menu`).
    menu_by_key: bool,
    /// Each window's input source (`per-window`).
    window_sources: HashMap<u32, crate::input_sources::Source>,
    /// Icons windows' apps set: theme name and image.
    window_icons: HashMap<u32, (String, Option<helix_draw::Image>)>,
    /// Loaded icons by key (and pixel size); None when not found. Decoded
    /// on workers: not there yet the first time.
    icon_cache: crate::icon_cache::IconCache,
    /// Drawing again for icons that came in (a timer, so a burst of them
    /// draws once).
    icon_redraw: Option<smithay_client_toolkit::reexports::calloop::RegistrationToken>,
    /// Work for workers (pictures).
    offload: offload::Offload,
    /// Input method globals (privileged shell only) and state.
    im_manager: Option<wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    vk_manager: Option<wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    ime: Option<ime::Ime>,
    /// Screen captures (privileged shell only).
    screenshots: Option<HelixScreenshotManagerV1>,
    /// Picking an area, a point or what to capture, while open.
    picker: Option<capture::Picker>,
    flashes: Vec<capture::Flash>,
    loop_handle: Option<LoopHandle<'static, Ui>>,
    /// `keyboard-focus-visible-timeout` (seconds; 0 or less: no timeout).
    focus_timeout: i32,
    /// Bumped at each keyboard move in a menu, to tell stale timers.
    focus_serial: u64,
    bindings: HashMap<u32, HelixKeybindingV1>,
    lock: LockScreen,
    banner: Option<BannerView>,
    /// Full-screen overlays: the end-session dialog and the power menu.
    dialog: Option<Overlay<DialogState>>,
    power_menu: Option<Overlay<quick::QuickNav>>,
    /// The break screen (break reminders).
    break_screen: Option<Overlay<crate::wellbeing::BreakView>>,
    /// "Customize Panel…".
    panel_editor: Option<Overlay<panel_editor::EditorState>>,
    /// What the quick settings menu shows.
    quick: quick::QuickState,
    /// The quick settings list that is open (Wi-Fi or Bluetooth).
    quick_list: Option<quick::Switch>,
    /// Quick settings' power actions are open (under its Power button).
    quick_power_open: bool,
    /// The app grid's hover changed in this pointer frame (drawn at its
    /// end).
    grid_pending: bool,
    /// "Pair New Device…" started Bluetooth discovery.
    bt_discovery: bool,
    /// The lock screen's media player and notification counts.
    lock_extras: lock_extras::LockExtras,
    /// The running apps last reported to the D-Bus side.
    running_apps: HashMap<String, u32>,
    window_menu: Option<window_menu::WindowMenu>,
    /// The dock on the desktop.
    dock: dock::DockState,
    workspace_popup: Option<workspace_popup::WorkspacePopup>,
    overview: Option<Overlay<overview::OverviewState>>,
    /// The overview on the other monitors.
    overview_others: Vec<Overlay<overview::OverviewState>>,
    /// Window pictures the overview shows.
    overview_shots: overview::Shots,
    /// Buffers of the overview on the other monitors, kept apart from the
    /// rest (sharing the pool left the primary monitor's overview stale
    /// while a virtual monitor nobody watched was up).
    overview_pool: Option<SlotPool>,
    /// The desktop background on every monitor, and its own pool (large
    /// buffers).
    backgrounds: Vec<background::BackgroundView>,
    background_pool: Option<SlotPool>,
    desktop_background: crate::wallpaper::Background,
    /// The wallpaper shrunk for the overview's workspace thumbnails: (the
    /// image's address and the height, the picture).
    overview_wall: Option<((usize, u32), Arc<helix_draw::Image>)>,
    /// The overview's wallpaper thumbnail being made (on a worker).
    overview_wall_wanted: Option<(usize, u32)>,
    lock_background: crate::wallpaper::Background,
    /// Bumped when the lock background changes (its blurred copies are
    /// made again).
    lock_background_serial: u64,
    /// Where windows are, from the compositor (window list v8).
    window_geometry: HashMap<u32, overview::Geometry>,
    /// The primary monitor's area (x, y, w, h).
    primary_area: (i32, i32, i32, i32),
    /// Numbers identifying monitors (`ShowMonitorLabels`), one per output.
    /// (number, the output's place, its connector)
    monitor_labels: Vec<Overlay<MonitorLabel>>,
    /// Which power actions logind allows (entries shown in the menu).
    power: crate::logind::Availability,
    /// A display manager can switch users.
    user_switch: bool,
    /// Windows that want attention.
    attention: std::collections::HashSet<u32>,
    /// The screen is being shared (panel indicator).
    sharing: bool,
    /// The screen is being recorded (panel indicator).
    recording: bool,
    /// Network, battery, volume, radios.
    status: crate::status::Status,
    /// The launcher's current search (answers for others are dropped).
    search_serial: u64,
    /// The query last sent to the search providers.
    searched_query: Option<String>,
    /// Control is held (launcher shortcuts).
    ctrl_held: bool,
    /// The modifiers held (capturing keys for extension commands).
    mods: Modifiers,
    /// The panel's layout, and where its items were last drawn
    /// (slot, x, width).
    panel_layout: crate::panel_layout::Layout,
    panel_keys: crate::panel_layout::Keys,
    panel_placed: Vec<(crate::panel_layout::Slot, i32, i32)>,
    /// Apps' status icons, and the open status icon menu (its item's key
    /// and its entries' ids).
    tray: Vec<crate::tray::Item>,
    tray_menu: Option<(String, Vec<i32>)>,
    /// What extensions show; the open extension menu (item, menu path,
    /// entries' ids); the extensions, for the Extensions dialog.
    ext: crate::ext_host::Snapshot,
    ext_menu: Option<(String, String, Vec<i32>)>,
    /// Drag and drop (files onto extensions' dock items): the device
    /// manager, the device, and the item a drag is over.
    data_devices: Option<smithay_client_toolkit::data_device_manager::DataDeviceManagerState>,
    data_device: Option<drops::Device>,
    /// The primary selection (middle-click paste into text fields).
    primary_selections: Option<smithay_client_toolkit::primary_selection::PrimarySelectionManagerState>,
    primary_device: Option<smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice>,
    /// Copy and paste in the shell's text fields.
    clipboard: clipboard::Clipboard,
    drop_at: Option<usize>,
    /// Where an extension dock item's menu was asked for (it comes later).
    ext_dock_menu_at: Option<(i32, i32)>,
    /// Dragging a panel item while "Customize Panel…" is open.
    panel_drag: Option<panel_editor::PanelDrag>,
    ext_list: Vec<crate::ext_host::Info>,
    extensions_dialog: Option<Overlay<extensions_dialog::DialogState>>,
    /// What the pointer rests on in the panel, and its tooltip.
    panel_hover: panel_tooltip::Hover,
    panel_tooltip: Option<panel_tooltip::PanelTooltip>,
    /// The input source indicator's text (two or more sources).
    source_label: Option<String>,
    /// The IBus engine's properties, and what the source menu's property
    /// entries are (key, kind, state).
    ime_props: Vec<crate::ibus::Prop>,
    ime_menu: Vec<(String, crate::ibus::PropKind, u32)>,
    /// The accessibility switches.
    a11y: crate::a11y_menu::Status,
    /// The shell's interface on the accessibility bus.
    atspi: atspi::AtSpi,
    exit: bool,
}

/// A monitor label: its number, its output's place, its connector.
type MonitorLabel = (i32, (i32, i32), String);

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
    /// A button of "Customize Panel…".
    PanelEditor(panel_editor::EditorHit),
    /// A button of "Extensions…".
    ExtDialog(extensions_dialog::DialogHit),
    Confirm,
    Cancel,
    /// A window in the switcher.
    Window(u32),
    Power(PowerAction),
    /// Quick settings: the volume slider, the mute toggle (the "Volume"
    /// label), the brightness slider, a switch, the menu's background.
    VolumeSlider,
    Mute,
    BrightnessSlider,
    Switch(quick::Switch),
    Inside,
    /// A monitor in the share dialog, by index.
    Monitor(usize),
    /// The prompt dialog's check box.
    Toggle,
    /// The end-session dialog's "Install pending software updates".
    EndInstall,
    /// One of the prompt dialog's extra check boxes.
    Check(usize),
    /// A power profile in quick settings, by index.
    Profile(usize),
    /// Quick settings: a switch's "›" (its list), a network, a device.
    Expand(quick::Switch),
    /// An extension's slider, and an extension toggle's "›".
    ExtSlider(usize),
    ExtDetails(usize),
    Network(usize),
    BtDevice(usize),
    /// A row of the prompt dialog's list.
    Row(usize),
    /// A place in the prompt dialog's column of places.
    Place(usize),
    /// A field of the prompt dialog (its focus number).
    Field(usize),
    /// "Show" in a password field of the prompt dialog.
    ShowPassword(usize),
    /// "Pair New Device…", and a device found nearby.
    PairNew,
    BtNearby(usize),
    /// "Send Files…" below the paired devices.
    BtSendFiles,
    /// Quick settings' top row: the screenshot UI, the Settings app, and
    /// the power actions' list.
    QuickScreenshot,
    QuickSettingsApp,
    PowerMenu,
    Outside,
}

struct DialogState {
    kind: DialogKind,
    remaining: u32,
    /// The button with the keyboard focus (Enter answers with it); `None`
    /// until a key moves it: then it is the dialog's default.
    focus: Option<Choice>,
    /// When it was first drawn (keys before [`DIALOG_KEY_DELAY`] after
    /// that are ignored).
    shown: Option<Instant>,
    /// The part under the pointer, and the one pressed (it answers when
    /// released over the same part).
    hover: Option<OverlayHit>,
    pressed: Option<OverlayHit>,
}

impl DialogState {
    fn new(kind: DialogKind, remaining: u32) -> Self {
        Self { kind, remaining, focus: None, shown: None, hover: None, pressed: None }
    }

    /// The button Enter chooses: whichever has the focus.
    fn focused(&self) -> Choice {
        self.focus.unwrap_or_else(|| self.kind.default_choice())
    }
}

/// Keys pressed this soon after a dialog appears are ignored: they were on
/// their way to what had the keyboard before.
const DIALOG_KEY_DELAY: Duration = Duration::from_millis(300);

/// Whether a key pressed at `now` reaches a dialog first drawn at `shown`.
fn dialog_takes_key(shown: Option<Instant>, now: Instant) -> bool {
    shown.is_some_and(|shown| now.saturating_duration_since(shown) >= DIALOG_KEY_DELAY)
}

/// What a key does in a question dialog whose focused button is `focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogKey {
    Answer(Choice),
    Focus(Choice),
    ToggleInstall,
}

fn dialog_key_action(keysym: Keysym, focus: Choice) -> Option<DialogKey> {
    let other = if focus == Choice::Confirmed { Choice::Canceled } else { Choice::Confirmed };
    match keysym {
        Keysym::Escape => Some(DialogKey::Answer(Choice::Canceled)),
        Keysym::Return | Keysym::KP_Enter => Some(DialogKey::Answer(focus)),
        Keysym::Tab | Keysym::ISO_Left_Tab => Some(DialogKey::Focus(other)),
        // Cancel is on the left, the action on the right.
        Keysym::Left => Some(DialogKey::Focus(Choice::Canceled)),
        Keysym::Right => Some(DialogKey::Focus(Choice::Confirmed)),
        Keysym::space => Some(DialogKey::ToggleInstall),
        _ => None,
    }
}

/// What the centered dialog asks.
enum DialogKind {
    /// Confirm ending the session (`EndSessionDialog`).
    End(EndDialog),
    /// Keep a display configuration change, or go back
    /// (`org.helix.DisplayChangeDialog`).
    DisplayChange,
    /// An app does not answer: wait or force it to quit (window id, title).
    NotResponding { id: u32, title: String },
    /// May window `id` have all keyboard shortcuts? (`app_id` empty: the
    /// answer is not remembered.)
    ShortcutsInhibit { id: u32, app_id: String, app: String },
    /// May an app control input (remote desktop)?
    RemoteControl { app: String, devices: Vec<String>, reply: Option<tokio::sync::oneshot::Sender<bool>> },
    /// May an extension have what it asks for? (the approval prompt)
    ApproveExtension { name: String, lines: Vec<String>, reply: Option<tokio::sync::oneshot::Sender<bool>> },
    /// Which monitor to share with an app (`org.helix.ScreenCastChooser`).
    ShareScreen {
        app: String,
        /// (what, label, detail)
        sources: Vec<(crate::screencast_chooser::Shared, String, String)>,
        remember: bool,
        /// Devices the app will also control (remote desktop).
        control: Vec<String>,
        /// Several may be chosen: the rows picked.
        multiple: bool,
        picked: Vec<usize>,
        reply: Option<tokio::sync::oneshot::Sender<Option<Vec<crate::screencast_chooser::Shared>>>>,
    },
    /// A password prompt (polkit, keyring).
    Prompt(Box<prompt::PromptView>),
}

impl DialogKind {
    /// The button that has the focus when the dialog appears, so Enter
    /// chooses it: never one that loses work or grants a permission.
    fn default_choice(&self) -> Choice {
        match self {
            DialogKind::End(_) | DialogKind::DisplayChange | DialogKind::ShareScreen { .. } | DialogKind::Prompt(_) => Choice::Confirmed,
            // Wait; Force Quit must be chosen.
            DialogKind::NotResponding { .. } => Choice::Canceled,
            // Deny, Cancel.
            DialogKind::ShortcutsInhibit { .. } | DialogKind::RemoteControl { .. } | DialogKind::ApproveExtension { .. } => Choice::Canceled,
        }
    }

    /// The action button's look: red when it loses work.
    fn confirm_kind(&self) -> theme::Kind {
        use theme::Kind as ButtonKind;
        match self {
            DialogKind::NotResponding { .. } => ButtonKind::Destructive,
            DialogKind::End(d) if !d.blockers.is_empty() => ButtonKind::Destructive,
            _ if self.default_choice() == Choice::Confirmed => ButtonKind::Suggested,
            _ => ButtonKind::Normal,
        }
    }
}

/// The "Recording" indicator's width (clicking it stops the recording).
fn recording_w() -> i32 {
    helix_draw::theme::grow(104)
}
/// The "Sharing" indicator's width.
fn sharing_w() -> i32 {
    helix_draw::theme::grow(96)
}
/// Space on each side of the clock (the unseen-notifications dot sits
/// in the right one).
const CLOCK_PAD: i32 = 24;

/// The panel items that react to clicks, left to right: what
/// `switch-panels` moves between.
/// Each of the `tray_icons` status icons is a control of its own.
fn panel_controls(placed: &[(crate::panel_layout::Slot, i32, i32)], tray_icons: usize) -> Vec<(crate::panel_layout::Slot, i32, i32)> {
    use crate::panel_layout::{Item, Slot};
    let mut out = Vec::new();
    for &(slot, x, w) in placed {
        match slot {
            _ if w <= 0 => {}
            Slot::Privacy => {}
            Slot::Item(Item::Tray) => {
                out.extend((0..tray_icons as i32).map(|i| (slot, x + i * (status::ICON + tray::TRAY_GAP), status::ICON)));
            }
            _ => out.push((slot, x, w)),
        }
    }
    out
}
/// The accessibility indicator's width.
const A11Y_W: i32 = 30;

/// The symbol of an engine's `InputMode` property, if it has one.
fn input_mode_symbol(props: &[crate::ibus::Prop]) -> Option<String> {
    props.iter().find_map(|p| if p.key == "InputMode" && !p.symbol.is_empty() { Some(p.symbol.clone()) } else { input_mode_symbol(&p.sub) })
}

/// The accessibility indicator: a figure with arms out.
fn draw_a11y_glyph(canvas: &mut Canvas, cx: i32, cy: i32) {
    let (cx, cy) = (f64::from(cx), f64::from(cy));
    canvas.circle(cx, cy - 6.5, 2.2, FG);
    canvas.line((cx - 6.5, cy - 2.5), (cx + 6.5, cy - 2.5), 1.8, FG);
    canvas.line((cx, cy - 2.5), (cx, cy + 2.5), 1.8, FG);
    canvas.line((cx, cy + 2.5), (cx - 4.0, cy + 8.5), 1.8, FG);
    canvas.line((cx, cy + 2.5), (cx + 4.0, cy + 8.5), 1.8, FG);
}

struct BannerView {
    data: crate::notifications::Banner,
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    hits: Vec<(Rect, BannerHit)>,
    /// Has keyboard focus (`focus-active-notification`).
    focused: bool,
    /// The part under the pointer, and the one pressed.
    hover: Option<BannerHit>,
    pressed: Option<BannerHit>,
}

/// A banner's width: it grows with the text scale.
fn banner_w() -> u32 {
    helix_draw::theme::grow(440) as u32
}
const MONITOR_LABEL_SIZE: u32 = 72;
const BANNER_PAD: i32 = 14;


/// The lock screen: one surface per output while the session is locked.
struct LockScreen {
    state: SessionLockState,
    session: Option<SessionLock>,
    surfaces: Vec<LockView>,
    password: String,
    message: Option<String>,
    checking: bool,
    auth: Arc<dyn Authenticator>,
    /// The login name (what is checked).
    user: String,
    /// What the lock screen calls the user, and their picture.
    identity: crate::user_identity::Identity,
    /// Results of password checks, from the worker thread.
    results: channel::Sender<Result<(), AuthError>>,
    /// Restart or Power Off asked on the lock screen, to be confirmed.
    confirming: Option<PowerAction>,
    /// Apps holding up logout while confirming.
    blockers: Vec<(String, String)>,
    /// The button under the pointer and the one pressed (on their
    /// surface), and the one with the keyboard focus (`None`: the field).
    hover: Option<(wl_surface::WlSurface, lock::LockHit)>,
    pressed: Option<(wl_surface::WlSurface, lock::LockHit)>,
    focus: Option<lock::LockHit>,
    /// How many characters are being checked (shown as dots meanwhile),
    /// and since when.
    checked_len: usize,
    checking_since: Instant,
    /// The last check failed: the field's ring shows it until a key.
    failed: bool,
    /// When the field was shaken (a wrong password).
    shaken: Option<Instant>,
    /// The monitor (its place) the prompt was moved to by a click.
    prompt_at: Option<(i32, i32)>,
    /// The prompt is shown (else the resting state), how far it got
    /// (0 to 1), and when the user last did something.
    revealed: bool,
    reveal: Tween,
    last_input: Instant,
    /// The lock screen's opacity over its background: fading in after
    /// locking, out before unlocking (`exiting`).
    shown: Tween,
    exiting: bool,
    /// What moves: read when the screen locks.
    motion: Motion,
    /// Frames are drawn until then (something moves).
    animate_until: Option<Instant>,
    /// The next frame's timer.
    frame_timer: Option<smithay_client_toolkit::reexports::calloop::RegistrationToken>,
}

/// The lock screen's Restart and Power Off buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockPowerHit {
    Ask(PowerAction),
    Cancel,
    Confirm,
}

struct LockView {
    surface: SessionLockSurface,
    output: wl_output::WlOutput,
    width: u32,
    height: u32,
    /// What its buttons do, where the last paint put them.
    hits: Vec<(Rect, lock::LockHit)>,
    /// Its output's place in the layout.
    origin: (i32, i32),
    /// Its background, blurred and toned once, and for what.
    backdrop: Option<(lock::BackdropKey, Vec<u8>)>,
    /// The background being made for it (on a worker).
    backdrop_wanted: Option<lock::BackdropKey>,
}

/// Lock screen transitions: the UI fading in after locking, out before
/// unlocking, the prompt shown from the resting state; and how long the
/// prompt stays without input before it rests again.
const LOCK_ENTER: Duration = Duration::from_millis(280);
const LOCK_EXIT: Duration = Duration::from_millis(180);
const LOCK_REVEAL: Duration = Duration::from_millis(240);
const LOCK_IDLE: Duration = Duration::from_secs(30);

/// Clear a password buffer, overwriting its bytes first.
fn wipe(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8, and the string is cleared after.
    unsafe { s.as_mut_vec().iter_mut().for_each(|b| std::ptr::write_volatile(b, 0)) };
    s.clear();
}

/// Local time as "HH:MM".
fn clock_text() -> String {
    crate::clock::now().map(|d| crate::clock::top_bar(crate::clock::current(), &d)).unwrap_or_else(|| "--:--".into())
}

/// The lock screen's clock: the time, its AM/PM suffix (12-hour format)
/// and the date line.
fn lock_clock() -> (String, Option<String>, String) {
    match crate::clock::now() {
        Some(d) => {
            let (time, suffix) = crate::clock::lock_screen_parts(crate::clock::current(), &d);
            (time, suffix, crate::clock::lock_date(&d))
        }
        None => ("--:--".into(), None, String::new()),
    }
}

/// What the lock screen's clock shows, to tell when it changes (the
/// minute, the day).
fn lock_clock_text() -> String {
    let (time, suffix, date) = lock_clock();
    format!("{time} {} {date}", suffix.unwrap_or_default())
}

/// Baseline that vertically centers text of size `px` in a box.
fn baseline(text: Option<&Text>, top: i32, height: i32, px: f32) -> f32 {
    let cap = text.map_or(px * 0.7, |t| t.ascent(px) * 0.72);
    top as f32 + (height as f32 + cap) / 2.0
}

/// A text field's box (helix-desktop-spec `components/README.md`, "Text
/// fields"): rounded, an accent ring while it takes typing, a faint
/// outline otherwise.
fn text_field(canvas: &mut Canvas, r: Rect, focused: bool) {
    theme::field(canvas, (r.x, r.y, r.w, r.h), focused, false);
}

/// The launcher's and the overview's search field: its width and height
/// (logical px, before the text scale), and where its text starts.
const SEARCH_W: i32 = 600;
const SEARCH_H: i32 = 48;
const SEARCH_TEXT_X: i32 = 46;

/// The part of the launcher's search field that clears it (among the
/// result rows' indexes).
const CLEAR_HIT: usize = usize::MAX;

/// Paint a search field at `r` (helix-desktop-spec `components/README.md`,
/// "Text fields"): a pill with the search symbol at its start, `query`
/// (else `placeholder`, dimmed) after it at the Entry size, the caret
/// while `focused`, and a round clear button at its end while there is a
/// query (in `clear`'s look). Returns where the clear button is.
fn search_field(canvas: &mut Canvas, text: Option<&Text>, r: Rect, query: &str, placeholder: &str, focused: bool) -> Option<Rect> {
    search_field_look(canvas, text, r, (query, placeholder), focused, theme::Look::default())
}

/// [`search_field`] with its clear button in `clear`'s look.
fn search_field_look(canvas: &mut Canvas, text: Option<&Text>, r: Rect, (query, placeholder): (&str, &str), focused: bool, clear: theme::Look) -> Option<Rect> {
    theme::field(canvas, (r.x, r.y, r.w, r.h), focused, true);
    let cy = f64::from(r.y) + f64::from(r.h) / 2.0;
    theme::search_symbol(canvas, f64::from(r.x) + 24.0, cy, 16.0, if query.is_empty() { DIM_FG } else { FG });
    let tx = r.x + SEARCH_TEXT_X;
    let d = r.h - 16;
    let room = (r.w - SEARCH_TEXT_X - d - 16) as f32;
    let base = baseline(text, r.y, r.h, size::ENTRY);
    if query.is_empty() {
        // Room for the caret before the placeholder.
        label(text, canvas, placeholder, (tx + if focused { 6 } else { 0 }) as f32, base, size::ENTRY, hint_text(), room);
    } else {
        label(text, canvas, query, tx as f32, base, size::ENTRY, FG, room);
    }
    if focused {
        let x = tx + text.map_or(0.0, |t| t.width(query, size::ENTRY).min(room)) as i32 + 1;
        caret(canvas, r, x, size::ENTRY);
    }
    if query.is_empty() {
        return None;
    }
    let (bx, by) = (r.x + r.w - 8 - d, r.y + 8);
    let (ccx, ccy) = (f64::from(bx) + f64::from(d) / 2.0, f64::from(by) + f64::from(d) / 2.0);
    theme::round_flat(canvas, ccx, ccy, f64::from(d), clear, false);
    theme::close_symbol(canvas, ccx, ccy, 10.0, if clear.hovered || clear.pressed { FG } else { DIM_FG });
    Some(Rect { x: bx, y: by, w: d, h: d })
}

/// The caret at `x`, the height of a line of `px` text in field `r`.
fn caret(canvas: &mut Canvas, r: Rect, x: i32, px: f32) {
    let h = (px * 1.3).round() as i32;
    canvas.rect(x, r.y + (r.h - h) / 2, 2, h, FG);
}

#[allow(clippy::too_many_arguments)]
fn label(text: Option<&Text>, canvas: &mut Canvas, s: &str, x: f32, base: f32, px: f32, color: u32, max_w: f32) {
    if let Some(text) = text {
        text.draw(canvas, s, x, base, Style { px, color, max_width: max_w });
    }
}

/// What the panel shows.
struct PanelModel<'a> {
    pub layout: &'a crate::panel_layout::Layout,
    pub clock: &'a str,
    pub status_items: &'a [status::StatusItem],
    /// Status icons: (icon, overlay, wants attention).
    pub tray_images: &'a [(Option<helix_draw::Image>, Option<helix_draw::Image>, bool)],
    pub tray_w: i32,
    pub ext_items: &'a [crate::ext_host::PanelItemView],
    pub ext_icons: &'a [Option<helix_draw::Image>],
    pub ext_widths: &'a [i32],
    pub locked: bool,
    pub launcher_open: bool,
    pub power_open: bool,
    pub list_open: bool,
    /// The panel item whose menu is open (input sources, accessibility).
    pub menu_open: Option<crate::panel_layout::Item>,
    pub unseen_dot: bool,
    /// (count, active)
    pub workspaces: (u32, u32),
    pub a11y_shown: bool,
    pub source_label: Option<&'a str>,
    pub sharing: bool,
    pub recording: bool,
    /// Dragging in the panel editor: where the item would go.
    pub drag_target: Option<i32>,
    /// The control with the keyboard focus (`switch-panels`).
    pub focus: Option<usize>,
    /// The item under the pointer, and the one being pressed.
    pub hover: Option<crate::panel_layout::Slot>,
    pub pressed: Option<crate::panel_layout::Slot>,
    /// The power icon at the end of the system pill.
    pub power_icon: Option<&'a helix_draw::Image>,
    /// The clock's (bold) face.
    pub bold: Option<&'a Text>,
}

/// Whether the panel's `slot` reacts to clicks (gets the pill's hover and
/// pressed looks).
fn panel_clickable(slot: crate::panel_layout::Slot) -> bool {
    use crate::panel_layout::{Item, Slot};
    !matches!(slot, Slot::Privacy | Slot::Item(Item::Tray) | Slot::Ext(_))
}

/// A panel control's pill (x, iw across the panel `h` tall): clear at
/// rest, a light fill under the pointer, a stronger one while pressed or
/// while what it opens is open. High contrast: white outlines.
fn panel_pill(canvas: &mut Canvas, x: i32, iw: i32, h: i32, hovered: bool, active: bool) {
    if !hovered && !active {
        return;
    }
    let (px, py, pw, ph) = (f64::from(x + 2), 4.0, f64::from(iw - 4), f64::from(h - 8));
    if helix_draw::high_contrast() {
        canvas.round_outline(px + 1.0, py + 1.0, pw - 2.0, ph - 2.0, ph / 2.0 - 1.0, if active { 2.0 } else { 1.0 }, 0xFFFFFFFF);
        return;
    }
    canvas.round_rect(px, py, pw, ph, ph / 2.0, if active { 0x40FFFFFF } else { 0x26FFFFFF });
}

/// The workspace indicator's pills: `count` (at least one) with `active`
/// wide, centered in a pill of width `w` at x.
fn workspace_pills(canvas: &mut Canvas, x: i32, w: i32, h: i32, (count, active): (u32, u32)) {
    let count = count.clamp(1, MAX_PILLS) as i32;
    let total = (count - 1) * (PILL_DOT + PILL_GAP) + PILL_ACTIVE;
    let mut px = x + (w - total) / 2;
    let y = f64::from(h) / 2.0 - f64::from(PILL_DOT) / 2.0;
    for i in 0..count {
        let on = i as u32 == active;
        let pw = if on { PILL_ACTIVE } else { PILL_DOT };
        let color = if on { FG } else { 0x99FFFFFF };
        if helix_draw::high_contrast() && !on {
            canvas.round_outline(f64::from(px) + 0.5, y + 0.5, f64::from(pw) - 1.0, f64::from(PILL_DOT) - 1.0, f64::from(PILL_DOT) / 2.0, 1.0, 0xFFFFFFFF);
        } else {
            canvas.round_rect(f64::from(px), y, f64::from(pw), f64::from(PILL_DOT), f64::from(PILL_DOT) / 2.0, color);
        }
        px += pw + PILL_GAP;
    }
}

/// The workspace indicator's dots and the active one's length, the gap
/// between them, and how many it shows at most.
const PILL_DOT: i32 = 7;
const PILL_ACTIVE: i32 = 24;
const PILL_GAP: i32 = 5;
const MAX_PILLS: u32 = 12;

/// The width of the workspace indicator for `count` workspaces.
fn workspace_pills_w(count: u32) -> i32 {
    let count = count.clamp(1, MAX_PILLS) as i32;
    (count - 1) * (PILL_DOT + PILL_GAP) + PILL_ACTIVE + 2 * 14
}

/// Paint the panel (`m`) on a `w`×`h` canvas; returns where each item went.
fn paint_panel(canvas: &mut Canvas, text: Option<&Text>, (w, h): (usize, usize), m: &PanelModel) -> Vec<(crate::panel_layout::Slot, i32, i32)> {
    canvas.fill(PANEL_BG);
    // Measure, place, then draw each item (helix-desktop-spec
    // `contracts/wayland/README.md`, "Panel layout").
    use crate::panel_layout::{Item, Slot};
    let px = size::LABEL;
    let base = baseline(text, 0, h as i32, px);
    let text_w = |s: &str| text.map_or(0.0, |t| t.width(s, px)) as i32;
    let clock = m.clock;
    let clock_w = match m.bold.or(text) {
        Some(text) => text.width(clock, px) as i32,
        None => font::text_width(clock, 2) as i32,
    };
    let status_w: i32 = m.status_items
        .iter()
        .map(|item| match item {
            status::StatusItem::Icon(_) => status::ICON + 10,
            status::StatusItem::Text(s) => text_w(s) + 4,
        })
        .sum();
    let size = |slot: Slot| match slot {
        // The workspace indicator (GNOME 45's "Activities" pill).
        Slot::Item(Item::Apps) => workspace_pills_w(m.workspaces.0),
        // Shown by Apps when it is in the panel too.
        Slot::Item(Item::Workspaces) if m.layout.contains(Item::Apps) => 0,
        Slot::Item(Item::Workspaces) if m.workspaces.0 > 1 => workspace_pills_w(m.workspaces.0),
        Slot::Item(Item::Workspaces) => 0,
        Slot::Item(Item::Clock) => clock_w + 2 * CLOCK_PAD,
        Slot::Item(Item::Tray) => m.tray_w,
        Slot::Item(Item::Accessibility) if m.a11y_shown => A11Y_W,
        Slot::Item(Item::Accessibility) => 0,
        Slot::Item(Item::Keyboard) => m.source_label.map_or(0, |s| text_w(s) + 16),
        // The status icons and the power icon are one system pill
        // (Power's); Status alone only without Power in the panel.
        Slot::Item(Item::Status) if m.layout.contains(Item::Power) => 0,
        Slot::Item(Item::Status) => status_w + 16,
        Slot::Item(Item::Power) => status_w + status::ICON + 20,
        Slot::Privacy => (if m.sharing { sharing_w() } else { 0 }) + if m.recording { recording_w() } else { 0 },
        // Nothing from extensions while locked.
        Slot::Ext(_) if m.locked => 0,
        Slot::Ext(i) => m.ext_widths.get(i).copied().unwrap_or(0),
    };
    let placed = crate::panel_layout::place(m.layout, w as i32, size);
    for &(slot, x, iw) in &placed {
        // Every clickable item is a pill: lit under the pointer, stronger
        // while pressed or while what it opens is open.
        if iw > 0 && panel_clickable(slot) {
            let open = match slot {
                Slot::Item(Item::Apps) | Slot::Item(Item::Workspaces) => m.launcher_open,
                Slot::Item(Item::Clock) => m.list_open,
                Slot::Item(Item::Status) | Slot::Item(Item::Power) => m.power_open,
                Slot::Item(item @ (Item::Keyboard | Item::Accessibility)) => m.menu_open == Some(item),
                _ => false,
            };
            panel_pill(canvas, x, iw, h as i32, m.hover == Some(slot), open || m.pressed == Some(slot));
        }
        match slot {
            Slot::Item(Item::Apps) | Slot::Item(Item::Workspaces) => workspace_pills(canvas, x, iw, h as i32, m.workspaces),
            Slot::Item(Item::Clock) => {
                let tx = x + CLOCK_PAD;
                match text {
                    Some(text) => {
                        // Bold, as GNOME's top bar clock.
                        let face = m.bold.unwrap_or(text);
                        face.draw(canvas, clock, tx as f32, base, Style { px, color: FG, max_width: f32::MAX });
                        // Unseen notifications: a dot after the clock.
                        if m.unseen_dot {
                            canvas.circle(f64::from(tx + clock_w + 9), f64::from(h as i32) / 2.0, 3.0, FG);
                        }
                    }
                    None => {
                        let ty = (h - font::GLYPH_H * 2) / 2;
                        font::render(clock, 2, |gx, gy| canvas.rect(tx + gx as i32, (ty + gy) as i32, 1, 1, FG));
                    }
                }
            }
            Slot::Item(Item::Tray) => {
                let y = (h as i32 - status::ICON) / 2;
                for (i, (image, overlay, attention)) in m.tray_images.iter().enumerate() {
                    let ix = x + i as i32 * (status::ICON + tray::TRAY_GAP);
                    if let Some(image) = image {
                        canvas.image(f64::from(ix), f64::from(y), f64::from(status::ICON), f64::from(status::ICON), image);
                    }
                    // The overlay: half size, over the lower-right corner.
                    if let Some(overlay) = overlay {
                        let half = status::ICON / 2;
                        canvas.image(f64::from(ix + half), f64::from(y + half), f64::from(half), f64::from(half), overlay);
                    }
                    if *attention {
                        canvas.rect(ix, h as i32 - 4, status::ICON, 2, accent());
                    }
                }
            }
            Slot::Item(Item::Accessibility) => draw_a11y_glyph(canvas, x + iw / 2, h as i32 / 2),
            Slot::Item(Item::Keyboard) => {
                if let Some(s) = m.source_label {
                    label(text, canvas, s, (x + 8) as f32, base, px, FG, (iw - 15) as f32);
                }
            }
            Slot::Item(Item::Status) | Slot::Item(Item::Power) => {
                // The power icon last, then the status items, the first
                // rightmost.
                let mut sx = x + iw - 8;
                if slot == Slot::Item(Item::Power) {
                    sx -= status::ICON;
                    if let Some(image) = m.power_icon {
                        canvas.image(f64::from(sx), f64::from((h as i32 - status::ICON) / 2), f64::from(status::ICON), f64::from(status::ICON), image);
                    }
                    sx -= 10;
                }
                for item in m.status_items {
                    match item {
                        status::StatusItem::Icon(image) => {
                            sx -= status::ICON;
                            canvas.image(f64::from(sx), f64::from((h as i32 - status::ICON) / 2), f64::from(status::ICON), f64::from(status::ICON), image);
                            sx -= 10;
                        }
                        status::StatusItem::Text(s) => {
                            let tw = text_w(s);
                            sx -= tw;
                            label(text, canvas, s, sx as f32, base, px, FG, tw as f32 + 1.0);
                            sx -= 4;
                        }
                    }
                }
            }
            Slot::Ext(i) => {
                if let Some(item) = m.ext_items.get(i) {
                    ext_items::draw_ext_item(text, canvas, item, m.ext_icons.get(i).and_then(Option::as_ref), x, iw, h as i32);
                }
            }
            Slot::Privacy => {
                let mut ix = x;
                if m.sharing {
                    canvas.rect(ix, h as i32 / 2 - 4, 8, 8, SHARING);
                    label(text, canvas, tr("Sharing"), (ix + 14) as f32, base, px, ERROR_TEXT, (sharing_w() - 16) as f32);
                    ix += sharing_w();
                }
                if m.recording {
                    canvas.rect(ix, h as i32 / 2 - 4, 8, 8, SHARING);
                    label(text, canvas, tr("Recording"), (ix + 14) as f32, base, px, ERROR_TEXT, (recording_w() - 16) as f32);
                }
            }
        }
    }
    // Dragging in the editor: where the item would go.
    if let Some(lx) = m.drag_target {
        canvas.rect(lx - 1, 3, 3, h as i32 - 6, accent());
    }
    // Keyboard focus ring (switch-panels).
    let controls = panel_controls(&placed, m.tray_images.len());
    if let Some(&(_, x, iw)) = m.focus.and_then(|i| controls.get(i)) {
        let (y, bh) = (5.0, h as f64 - 10.0);
        helix_draw::theme::focus_ring_round(canvas, f64::from(x + 3), y, f64::from(iw - 6), bh, bh / 2.0);
    }
    placed
}

/// A question dialog (not the share dialog or a prompt) over a scrim on a
/// `w`×`h` surface: its clickable parts and its accessible node.
fn paint_question(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (w, h): (usize, usize), state: &DialogState) -> Option<(Vec<(Rect, OverlayHit)>, crate::a11y::Node)> {
canvas.fill(SCRIM);
    let remaining = state.remaining;
    let no_blockers = Vec::new();
    // The approval prompt lists what the extension will be able to do.
    let approval_rows: Vec<(String, String)> = match &state.kind {
        DialogKind::ApproveExtension { lines, .. } => lines.iter().map(|l| (l.clone(), String::new())).collect(),
        _ => Vec::new(),
    };
    let list_header = if matches!(state.kind, DialogKind::ApproveExtension { .. }) {
        tr("It will be able to:")
    } else {
        tr("Some applications are busy or have unsaved work:")
    };
    let (title, message, cancel_label, confirm_label, blockers): (String, String, &str, String, &Vec<_>) = match &state.kind {
        DialogKind::End(d) => {
            let (title, verb) = match d.kind {
                SHUTDOWN => (tr("Power Off"), tr("Power Off")),
                REBOOT => (tr("Restart"), tr("Restart")),
                UPDATE_REBOOT => (tr("Restart & Install Updates"), tr("Restart & Install")),
                UPGRADE_REBOOT => (tr("Restart & Install Upgrade"), tr("Restart & Install")),
                _ => (tr("Log Out"), tr("Log Out")),
            };
            let confirm = if d.blockers.is_empty() { verb.to_owned() } else { trf!("{verb} Anyway", verb = verb) };
            let message = match (d.kind, &d.upgrade) {
                (UPDATE_REBOOT, _) => trf!("The computer will restart and install updates in {remaining} seconds. Installing can take a while.", remaining = remaining),
                (UPGRADE_REBOOT, Some(name)) => trf!("{name} will be installed after the restart, in {remaining} seconds. Installing can take a long time.", name = name, remaining = remaining),
                _ => trf!("The session will end automatically in {remaining} seconds.", remaining = remaining),
            };
            (title.to_owned(), message, tr("Cancel"), confirm, &d.blockers)
        }
        DialogKind::ShareScreen { .. } | DialogKind::Prompt(_) => return None,
        DialogKind::ApproveExtension { name, .. } => (trf!("Turn on {name}?", name = name), String::new(), tr("Cancel"), tr("Turn On").to_owned(), &approval_rows),
        DialogKind::RemoteControl { app, devices, .. } => (
            tr("Allow remote control?").to_owned(),
            trf!("{app} wants to control your {devices}.", app = app, devices = crate::screencast_chooser::device_list(devices)),
            tr("Deny"),
            tr("Allow").to_owned(),
            &no_blockers,
        ),
        DialogKind::ShortcutsInhibit { app, .. } => (
            trf!("Allow {app} to use all keyboard shortcuts?", app = app),
            tr("While it has the keyboard, the desktop's shortcuts go to it. Super+Escape gives them back.").to_owned(),
            tr("Deny"),
            tr("Allow").to_owned(),
            &no_blockers,
        ),
        DialogKind::NotResponding { title, .. } => (
            trf!("“{title}” is not responding.", title = title),
            tr("You may choose to wait a short while for it to continue or force the app to quit entirely.").to_owned(),
            tr("Wait"),
            tr("Force Quit").to_owned(),
            &no_blockers,
        ),
        DialogKind::DisplayChange => (
            tr("Keep the new display settings?").to_owned(),
            trf!("The previous settings come back in {remaining} seconds.", remaining = remaining),
            tr("Revert"),
            tr("Keep").to_owned(),
            &no_blockers,
        ),
    };
    let blocked = !blockers.is_empty();
    // Lengths that hold text grow with the text scale.
    let grow = helix_draw::theme::grow;
    let bw = grow(460).min(w as i32 - 40);
    let rows = blockers.len().min(6) as i32;
    let message_lines = text.map_or(1, |t| t.wrap(&message, size::LABEL, (bw - 48) as f32, 2).len().max(1)) as i32;
    let install = match &state.kind {
        DialogKind::End(d) => d.install,
        _ => None,
    };
    // Rows with a second line (a blocking app's reason) are taller.
    let row_h = if blockers.iter().any(|(_, r)| !r.is_empty()) { grow(40) } else { grow(26) };
    let bh = if blocked { grow(150) + grow(26) + rows * row_h } else { grow(160) + (message_lines - 1) * grow(19) } + if install.is_some() { grow(34) } else { 0 };
    let bx = (w as i32 - bw) / 2;
    let by = (h as i32 - bh) / 3;
    theme::popover(canvas, bx, by, bw, bh);
    let inner = (bw - 48) as f32;
    let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
    // The title in bold; centered as measured in that face.
    let title_face = bold.or(text);
    let tw = title_face.map_or(0.0, |t| t.width(&title, size::TITLE).min(inner));
    label(title_face, canvas, &title, bx as f32 + (bw as f32 - tw) / 2.0, (by + grow(44)) as f32, size::TITLE, FG, inner);
    let mut y = by + grow(78);
    if blocked {
        label(text, canvas, list_header, (bx + 24) as f32, y as f32, size::LABEL, DIM_FG, inner);
        y += grow(16);
        for (app, reason) in blockers.iter().take(6) {
            label(text, canvas, app, (bx + 24) as f32, (y + grow(18)) as f32, size::BODY, FG, inner);
            label(text, canvas, reason, (bx + 24) as f32, (y + grow(36)) as f32, size::CAPTION, DIM_FG, inner);
            y += row_h;
        }
    } else {
        let lines = text.map_or_else(|| vec![message.clone()], |t| t.wrap(&message, size::LABEL, inner, 2));
        for (i, line) in lines.iter().enumerate() {
            label(text, canvas, line, center(line, size::LABEL), (y + i as i32 * grow(19)) as f32, size::LABEL, DIM_FG, inner);
        }
    }
    let mut extra_hits = Vec::new();
    if let Some(on) = install {
        let r = Rect { x: bx + 24, y: by + bh - grow(56) - grow(42), w: bw - 48, h: grow(26) };
        theme::check_box(canvas, f64::from(r.x), f64::from(r.y + (r.h - 18) / 2), 18.0, on, theme::Look { hovered: state.hover == Some(OverlayHit::EndInstall), pressed: state.pressed == Some(OverlayHit::EndInstall), ..Default::default() });
        let name = tr("Install pending software updates");
        label(text, canvas, name, (r.x + 28) as f32, baseline(text, r.y, r.h, size::LABEL), size::LABEL, FG, inner - 28.0);
        extra_hits.push((r, OverlayHit::EndInstall));
    }
    // Buttons: Cancel and the action.
    let by2 = by + bh - grow(56);
    let half = (bw - 24 * 2 - 12) / 2;
    let cancel = Rect { x: bx + 24, y: by2, w: half, h: grow(36) };
    let confirm = Rect { x: bx + 24 + half + 12, y: by2, w: half, h: grow(36) };
    let focus = state.focused();
    let button = |canvas: &mut Canvas, r: Rect, s: &str, kind, choice, hit| {
        let look = theme::Look { hovered: state.hover == Some(hit), pressed: state.pressed == Some(hit), ..Default::default() };
        theme::button(canvas, text, (r.x, r.y, r.w, r.h), s, kind, look, focus == choice);
    };
    button(canvas, cancel, cancel_label, theme::Kind::Normal, Choice::Canceled, OverlayHit::Cancel);
    button(canvas, confirm, &confirm_label, state.kind.confirm_kind(), Choice::Confirmed, OverlayHit::Confirm);
    let mut hits = vec![(cancel, OverlayHit::Cancel), (confirm, OverlayHit::Confirm)];
    hits.extend(extra_hits);
    let list = blocked.then_some((list_header, blockers.as_slice()));
    let node = atspi::dialog(&title, &message, list, install, &hits, (cancel_label, &confirm_label), focus == Choice::Confirmed);
    Some((hits, node))
}

/// Words for the OSD icons the shell uses itself.
fn osd_caption(osd: &Osd) -> Option<String> {
    if let Some(label) = &osd.label {
        return Some(label.clone());
    }
    let caption = match osd.icon.as_deref()? {
        "audio-volume-muted-symbolic" => tr("Muted"),
        i if i.starts_with("audio-volume-") => tr("Volume"),
        "display-brightness-symbolic" => tr("Brightness"),
        "microphone-sensitivity-muted-symbolic" => tr("Microphone off"),
        i if i.starts_with("microphone-sensitivity-") => tr("Microphone on"),
        _ => return None,
    };
    Some(caption.to_owned())
}

impl Ui {

    // ----- Panel -----------------------------------------------------------

    fn draw_panel(&mut self) {
        let _busy = crate::watchdog::scope("draw panel");
        if !self.panel_configured || self.panel_width == 0 {
            return;
        }
        let (w, h) = (self.panel_width as usize, panel_height() as usize);
        let scale = self.scales.of(self.panel.wl_surface());
        let status_items = self.status_items(scale);
        let power_icon = self.symbolic_icon("system-shutdown-symbolic", scale);
        let tray_images = self.tray_images(scale);
        let ext_icons: Vec<Option<helix_draw::Image>> = (0..self.ext.panel.len()).map(|i| self.ext_icon(i, scale)).collect();
        let ext_widths: Vec<i32> = ext_icons.iter().enumerate().map(|(i, icon)| self.ext_width(i, icon.is_some())).collect();
        let (tray_w, unseen_dot) = (self.tray_width(), self.unseen_dot());
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate panel buffer");
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(self.panel.wl_surface()));
        let clock = clock_text();
        let model = PanelModel {
            layout: &self.panel_layout,
            clock: &clock,
            status_items: &status_items,
            tray_images: &tray_images,
            tray_w,
            ext_items: &self.ext.panel,
            ext_icons: &ext_icons,
            ext_widths: &ext_widths,
            locked: self.lock.session.is_some(),
            launcher_open: self.launcher.is_some(),
            power_open: self.power_menu.is_some(),
            list_open: self.notify.view.is_some(),
            menu_open: self.window_menu.as_ref().and_then(|m| m.panel_item),
            unseen_dot,
            workspaces: (self.windows.count, self.windows.active),
            a11y_shown: self.a11y.shown,
            source_label: self.source_label.as_deref(),
            sharing: self.sharing,
            recording: self.recording,
            drag_target: self.panel_drag.as_ref().filter(|d| d.moving && !d.off_panel).and_then(|d| d.target.as_ref()).map(|t| t.2),
            focus: self.panel_focus,
            hover: self.panel_lit,
            pressed: self.panel_pressed,
            power_icon: power_icon.as_ref(),
            bold: self.bold.as_ref(),
        };
        let placed = paint_panel(&mut canvas, self.text.as_ref(), (w, h), &model);
        self.shown_minute = Some(clock);
        self.panel_placed = placed;

        let surface = self.panel.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            self.panel.commit();
        }
    }

    // ----- Launcher --------------------------------------------------------

    fn open_launcher(&mut self, notify: bool) {
        if self.launcher.is_some() {
            return;
        }
        let was_shown = self.overview_shown();
        self.close_overview(false);
        let notify = notify && !was_shown;
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
        let folders = crate::favorites::folders(&apps);
        let favorites = crate::favorites::favorites();
        // The search providers are read again.
        self.searched_query = None;
        self.search_serial += 1;
        let _ = self.requests.send(UiRequest::Search { serial: self.search_serial, query: String::new() });
        self.launcher = Some(LauncherView {
            layer,
            model: {
                let mut model = Launcher::with_windows(apps, self.windows.mru().to_vec());
                model.set_collections(favorites, folders);
                model
            },
            width: 0,
            height: 0,
            configured: false,
            content: Rect { x: 0, y: 0, w: 0, h: 0 },
            rows: Vec::new(),
            grid: app_grid::GridView::new(crate::item_feedback::motion()),
            output: None,
            hover: None,
            pressed: None,
            clear: None,
            grid_cache: None,
        });
        if notify {
            let _ = self.requests.send(UiRequest::Overview(true));
        }
        self.draw_panel();
        self.dock_update();
    }

    fn close_launcher(&mut self, notify: bool) {
        if self.launcher.take().is_some() {
            if notify {
                let _ = self.requests.send(UiRequest::Overview(false));
            }
            self.draw_panel();
            self.dock_update();
        }
    }

    /// Ask the search providers when the launcher's query changed.
    pub(super) fn search_changed(&mut self) {
        let Some(view) = &self.launcher else { return };
        let query = view.model.query.clone();
        if self.searched_query.as_deref() == Some(query.as_str()) {
            return;
        }
        self.searched_query = Some(query.clone());
        if query.trim().is_empty() {
            return;
        }
        self.search_serial += 1;
        let _ = self.requests.send(UiRequest::Search { serial: self.search_serial, query });
    }

    /// Put `text` on the clipboard (through the compositor).
    fn copy_text(&mut self, text: &str) {
        let Some(manager) = self.screenshots.as_ref().filter(|m| m.version() >= 6) else {
            tracing::info!("compositor cannot take the clipboard; text not copied");
            return;
        };
        let Some(mut file) = crate::screenshot::memfd() else { return };
        use std::io::{Seek, Write};
        if file.write_all(text.as_bytes()).is_ok() && file.seek(std::io::SeekFrom::Start(0)).is_ok() {
            manager.copy_text(std::os::fd::AsFd::as_fd(&file));
        }
    }

    fn launch_selected(&mut self) {
        let Some(view) = &self.launcher else { return };
        match view.model.selected() {
            Some(launcher::Hit::Folder(i)) => {
                if let Some(view) = self.launcher.as_mut() {
                    view.model.enter_folder(i);
                }
                self.draw_launcher();
                return;
            }
            Some(launcher::Hit::Search(i)) => {
                let hit = &view.model.external[i];
                tracing::info!(app = hit.app, id = hit.id, "opening search result");
                if let Some(text) = hit.clipboard.clone() {
                    self.copy_text(&text);
                }
                let view = self.launcher.as_ref().expect("checked above");
                let hit = &view.model.external[i];
                let _ = self.requests.send(UiRequest::SearchActivate { provider: hit.provider, id: hit.id.clone(), query: view.model.query.clone() });
            }
            Some(launcher::Hit::More(i)) => {
                let hit = &view.model.external[i];
                tracing::info!(app = hit.app, "opening the search in its app");
                let _ = self.requests.send(UiRequest::SearchLaunch { provider: hit.provider, query: view.model.query.clone() });
            }
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

    // ----- Panel pointer --------------------------------------------------------

    /// The pointer over the panel's pills: hover, press, and a click that
    /// acts on release over the item it was pressed on (moving off while
    /// pressed cancels it). Items with their own handling (status icons,
    /// extension items, the privacy indicators) are left to it.
    fn panel_pointer(&mut self, event: &PointerEvent) -> bool {
        if &event.surface != self.panel.wl_surface() {
            return false;
        }
        let x = event.position.0;
        let hit = self.panel_placed.iter().copied().find(|&(slot, px, pw)| pw > 0 && panel_clickable(slot) && x >= f64::from(px) && x < f64::from(px + pw));
        let under = hit.map(|h| h.0);
        match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                if self.panel_lit != under {
                    self.panel_lit = under;
                    self.draw_panel();
                }
                false
            }
            PointerEventKind::Leave { .. } => {
                if self.panel_lit.take().is_some() | self.panel_pressed.take().is_some() {
                    self.draw_panel();
                }
                false
            }
            PointerEventKind::Press { button: BTN_LEFT, .. } if under.is_some() => {
                self.panel_pressed = under;
                self.draw_panel();
                true
            }
            PointerEventKind::Release { button: BTN_LEFT, .. } => {
                let pressed = self.panel_pressed.take();
                if pressed.is_none() {
                    return false;
                }
                self.draw_panel();
                if let Some((slot, px, _)) = hit.filter(|h| Some(h.0) == pressed) {
                    self.panel_activate(slot, px, None);
                }
                true
            }
            _ => false,
        }
    }

    // ----- Panel keyboard focus (switch-panels) -------------------------------

    /// `switch-panels` (Ctrl+Alt+Tab): the panel, then the dock (when
    /// there is one), then the panel again; Shift goes the other way.
    fn switch_panels(&mut self, backward: bool) {
        if self.panel_focus.is_some() && self.dock_reachable() {
            self.leave_panel();
            self.focus_dock(backward);
        } else if self.dock_keyboard_view().is_some() {
            self.leave_dock();
            self.focus_panel(backward);
        } else if self.panel_focus.is_none() && backward && self.dock_reachable() {
            self.focus_dock(true);
        } else {
            self.focus_panel(backward);
        }
    }

    fn focus_panel(&mut self, backward: bool) {
        let count = panel_controls(&self.panel_placed, self.tray_shown().len()).len().max(1);
        let next = match self.panel_focus {
            None if backward => count - 1,
            None => 0,
            Some(i) if backward => (i + count - 1) % count,
            Some(i) => (i + 1) % count,
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

    /// The panel item at `index` takes the keyboard focus again (a menu it
    /// opened closed).
    pub(super) fn refocus_panel(&mut self, index: usize) {
        if self.lock.session.is_some() {
            return;
        }
        self.panel_focus = Some(index);
        self.panel.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        self.panel.commit();
        self.draw_panel();
    }

    /// A click on panel item `slot`, drawn at `x`, or Enter on it while it
    /// is the `key`th focused control (what it opens takes the keyboard).
    fn panel_activate(&mut self, slot: crate::panel_layout::Slot, x: i32, key: Option<usize>) {
        use crate::panel_layout::{Item, Slot};
        match slot {
            Slot::Item(Item::Apps) => {
                if self.launcher.is_some() {
                    self.close_launcher(true);
                } else {
                    self.open_launcher(true);
                }
            }
            Slot::Item(Item::Clock) => self.toggle_notification_list(key.map_or(quick::Opened::Pointer, quick::Opened::Panel)),
            Slot::Item(Item::Keyboard) => {
                if self.window_menu.take().is_none() {
                    self.show_sources_menu(x);
                }
                self.draw_panel();
            }
            Slot::Item(Item::Accessibility) => {
                if self.window_menu.take().is_none() {
                    self.show_a11y_menu(x);
                }
                self.draw_panel();
            }
            Slot::Item(Item::Status) | Slot::Item(Item::Power) => {
                self.toggle_power_menu(key.map_or(quick::Opened::Pointer, quick::Opened::Panel));
                self.draw_panel();
            }
            Slot::Item(Item::Tray) => {
                let tray_x = self.panel_placed.iter().find(|p| p.0 == slot).map_or(x, |p| p.1);
                if let Some((key, ix)) = self.tray_at(f64::from(x) + 1.0, tray_x) {
                    self.tray_request(key, crate::tray::Action::Primary, ix);
                }
            }
            Slot::Ext(i) => self.ext_activate(i, x),
            // The workspace indicator opens the launcher, as Apps.
            Slot::Item(Item::Workspaces) => {
                if self.launcher.is_some() {
                    self.close_launcher(true);
                } else {
                    self.open_launcher(true);
                }
            }
            Slot::Privacy => {}
        }
    }

    /// The panel layout keys changed (or were read at start).
    pub(super) fn panel_layout_changed(&mut self, keys: crate::panel_layout::Keys) {
        let layout = crate::panel_layout::resolve(&keys, &self.ext_panels());
        if keys.quick != self.panel_keys.quick {
            self.panel_keys.quick = keys.quick.clone();
            self.draw_power_menu();
        }
        self.panel_keys = keys;
        if layout != self.panel_layout {
            tracing::info!(?layout, "panel layout");
            self.panel_layout = layout;
            self.panel_focus = None;
            self.draw_panel();
        }
        self.draw_panel_editor();
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
                let index = self.panel_focus;
                let control = index.and_then(|i| panel_controls(&self.panel_placed, self.tray_shown().len()).get(i).copied());
                self.leave_panel();
                if let Some((slot, x, _)) = control {
                    self.menu_by_key = true;
                    self.panel_activate(slot, x, index);
                    self.menu_by_key = false;
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
                count: 1,
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
        // Window pictures for the window switcher (not for applications).
        let windows = matches!(kind, SwitchKind::Windows { .. }) && self.switcher_mode != Some(SwitchMode::Apps);
        let mode = crate::windows::icon_mode();
        let thumbnails = (windows && mode != crate::windows::IconMode::AppIconOnly).then_some(mode);
        if thumbnails.is_some() {
            let ids: Vec<u32> = switcher.entries.iter().map(|e| e.id).collect();
            self.capture_shots(&ids);
        }
        let overlay = self.new_overlay("helix-switcher", KeyboardInteractivity::Exclusive, SwitcherState { switcher, hold, kind, thumbnails, hover: None, pressed: None });
        self.switcher = Some(overlay);
    }

    fn commit_switch(&mut self, kind: &SwitchKind, id: u32) {
        match kind {
            SwitchKind::Windows { .. } => self.activate_window(id),
            SwitchKind::InputSources(sources) => {
                if let Some(source) = sources.get(id as usize) {
                    crate::input_sources::activate(source);
                    self.ime_sync();
                    let focused = self.windows.focused();
                    if focused != 0 {
                        self.window_sources.insert(focused, source.clone());
                    }
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

    /// `accent-color` changed: every surface showing the accent is drawn
    /// again with it (high contrast keeps its own colors, so nothing
    /// changes on screen while it is on).
    fn accent_changed(&mut self, accent: helix_draw::accent::Accent) {
        if accent == helix_draw::accent::chosen() {
            return;
        }
        tracing::info!(accent = accent.name(), "accent");
        helix_draw::accent::set(accent);
        if helix_draw::high_contrast() {
            return;
        }
        self.redraw_all();
        self.draw_power_menu();
        self.draw_workspace_popup();
        self.draw_overview();
        self.dock_draw();
        self.draw_desktop_icons();
        self.draw_osk();
        self.draw_candidates();
        self.draw_click_type();
        self.draw_panel_editor();
        self.draw_extensions_dialog();
        self.draw_break();
    }

    /// The interface font (`font-name`) changed: its faces are loaded and
    /// everything shown is drawn again with them.
    fn font_changed(&mut self, family: Option<String>) {
        if family == self.font_family {
            return;
        }
        tracing::info!(?family, "interface font");
        let regular = Text::load_family(family.as_deref(), None);
        if regular.is_none() {
            return;
        }
        self.text = regular;
        self.bold = Text::load_family(family.as_deref(), Some("bold"));
        self.display = helix_draw::screen::display_face_of(family.as_deref());
        self.font_family = family;
        if let Some(data) = self.banner.as_ref().map(|b| b.data.clone()) {
            self.show_banner(Some(data));
        }
        self.redraw_all();
        self.draw_power_menu();
        self.draw_workspace_popup();
        self.draw_overview();
        self.dock_draw();
        self.draw_desktop_icons();
        self.draw_osk();
        self.draw_candidates();
        self.draw_click_type();
        self.draw_panel_editor();
        self.draw_extensions_dialog();
        self.draw_break();
    }

    /// `text-scaling-factor` changed: the panel grows (or shrinks) and
    /// everything shown is drawn again at the new size.
    fn text_scale_changed(&mut self, factor: f64) {
        let before = helix_draw::theme::text_scale();
        helix_draw::theme::set_text_scale(factor);
        if helix_draw::theme::text_scale() == before {
            return;
        }
        tracing::info!(factor, "text scale");
        self.panel.set_size(0, panel_height());
        self.panel.set_exclusive_zone(panel_height() as i32);
        self.panel.commit();
        if let Some(o) = self.overview.as_ref() {
            o.layer.set_margin(panel_height() as i32, 0, 0, 0);
            o.layer.commit();
        }
        if let Some(data) = self.banner.as_ref().map(|b| b.data.clone()) {
            self.show_banner(Some(data));
        }
        self.redraw_all();
        self.draw_overview();
    }

    fn redraw_all(&mut self) {
        self.draw_panel();
        self.draw_launcher();
        self.draw_switcher();
        self.draw_lock();
        self.draw_banner();
        self.draw_dialog();
        if self.power_menu.is_some() {
            self.draw_power_menu();
        }
        for index in 0..self.monitor_labels.len() {
            self.draw_monitor_label(index);
        }
        self.redraw_picker();
        self.draw_notification_list();
        self.draw_window_menu();
    }

    fn draw_switcher(&mut self) {
        let _busy = crate::watchdog::scope("draw switcher");
        let Some(mut overlay) = self.switcher.take() else { return };
        if overlay.configured && overlay.width > 0 {
            self.paint_switcher(&mut overlay);
        }
        self.switcher = Some(overlay);
    }

    fn paint_switcher(&mut self, overlay: &mut Overlay<SwitcherState>) {
        // Drilled into an application's windows: tiles too.
        let drilled = matches!(&overlay.state.kind, SwitchKind::Windows { apps: Some(_) });
        let tiles = overlay.state.thumbnails.or_else(|| {
            let mode = crate::windows::icon_mode();
            (drilled && mode != crate::windows::IconMode::AppIconOnly).then_some(mode)
        });
        if let Some(mode) = tiles {
            if self.switcher_mode != Some(SwitchMode::Apps) || drilled {
                return self.paint_switcher_tiles(overlay, mode);
            }
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        // Applications: a row of large icons, as long as they fit.
        let apps = self.switcher_mode == Some(SwitchMode::Apps) && matches!(overlay.state.kind, SwitchKind::Windows { apps: None });
        if let Some(icon) = apps.then(|| app_row_icon(w as i32, overlay.state.switcher.entries.len())).flatten() {
            let px = (f64::from(icon) * scale).ceil() as u32;
            let ids: Vec<u32> = overlay.state.switcher.entries.iter().map(|e| e.id).collect();
            let icons: Vec<Option<helix_draw::Image>> = ids.iter().map(|id| self.window_icon(*id, px)).collect();
            let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
                return;
            };
            let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(overlay.layer.wl_surface()));
            let state = &overlay.state;
            overlay.hits = paint_app_row(&mut canvas, (self.text.as_ref(), self.bold.as_ref()), (w as i32, h as i32), &state.switcher, &icons, icon, (state.hover, state.pressed));
            let surface = overlay.layer.wl_surface();
            self.scales.prepare(&self.qh, surface, w as i32, h as i32);
            if buffer.attach_to(surface).is_ok() {
                overlay.layer.commit();
            }
            return;
        }
        let (shown, first) = {
            let switcher = &overlay.state.switcher;
            let shown = switcher.entries.len().clamp(1, SWITCHER_ROWS);
            (shown, switcher.selected.saturating_sub(shown - 1))
        };
        // Window entries get their window's icon (input sources none).
        let windows = matches!(overlay.state.kind, SwitchKind::Windows { .. });
        let px = (f64::from(ICON_PX) * scale).ceil() as u32;
        let ids: Vec<u32> = overlay.state.switcher.entries.iter().map(|e| e.id).collect();
        let icons: Vec<Option<helix_draw::Image>> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| if windows && (first..first + shown).contains(&i) { self.window_icon(*id, px) } else { None })
            .collect();
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(overlay.layer.wl_surface()));
        canvas.fill(0x00000000);
        let switcher = &overlay.state.switcher;
        let shown = switcher.entries.len().clamp(1, SWITCHER_ROWS);
        let first = switcher.selected.saturating_sub(shown - 1);
        let text = self.text.as_ref();
        let switcher = &overlay.state.switcher;
        let bw = helix_draw::theme::grow(480).min(w as i32 - 40);
        let row_h = helix_draw::theme::grow(ROW_H);
        let bh = shown as i32 * row_h + 16;
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 2;
        theme::popover(&mut canvas, bx, by, bw, bh);
        overlay.hits.clear();
        for (row, index) in (first..switcher.entries.len()).take(shown).enumerate() {
            let entry = &switcher.entries[index];
            let r = Rect { x: bx + 8, y: by + 8 + row as i32 * row_h, w: bw - 16, h: row_h };
            let hit = OverlayHit::Window(entry.id);
            let look = theme::Look { on: index == switcher.selected, hovered: overlay.state.hover == Some(hit), pressed: overlay.state.pressed == Some(hit) };
            theme::row(&mut canvas, r.x, r.y, r.w, r.h, look);
            let base = baseline(text, r.y, r.h, size::BODY);
            let detail_w = text.map_or(0.0, |t| t.width(&entry.detail, size::CAPTION).min(160.0));
            let mut label_x = r.x + 12;
            if let Some(icon) = icons.get(index).and_then(|i| i.as_ref()) {
                let y = f64::from(r.y + (r.h - ICON_PX) / 2);
                canvas.image(f64::from(r.x + 10), y, f64::from(ICON_PX), f64::from(ICON_PX), icon);
                label_x = r.x + 18 + ICON_PX;
            }
            let fg = if index == switcher.selected { on_accent() } else { FG };
            label(text, &mut canvas, &entry.label, label_x as f32, base, size::BODY, fg, (r.x + r.w - label_x) as f32 - 24.0 - detail_w);
            let detail_color = if index == switcher.selected { fg } else { DIM_FG };
            label(text, &mut canvas, &entry.detail, r.x as f32 + r.w as f32 - 12.0 - detail_w, base, size::CAPTION, detail_color, 160.0);
            overlay.hits.push((r, OverlayHit::Window(entry.id)));
        }
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// The window switcher as a row of window pictures.
    fn paint_switcher_tiles(&mut self, overlay: &mut Overlay<SwitcherState>, mode: crate::windows::IconMode) {
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let fits = ((w as i32 - 40 - THUMB_GAP) / (THUMB_W + THUMB_GAP)).max(1) as usize;
        let switcher = &overlay.state.switcher;
        let shown = switcher.entries.len().clamp(1, THUMB_MAX.min(fits));
        let first = switcher.selected.saturating_sub(shown - 1);
        let ids: Vec<u32> = switcher.entries.iter().map(|e| e.id).collect();
        // Pictures not asked for yet (e.g. after drilling in).
        let missing: Vec<u32> = ids.iter().copied().filter(|id| self.overview_shots.picture(*id).is_none()).collect();
        self.capture_shots(&missing);
        let icon_px = (32.0 * scale).ceil() as u32;
        let big_px = (64.0 * scale).ceil() as u32;
        let icons: Vec<(Option<helix_draw::Image>, Option<helix_draw::Image>)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                if !(first..first + shown).contains(&i) {
                    return (None, None);
                }
                let has_picture = self.overview_shots.picture(*id).is_some();
                let small = (mode == crate::windows::IconMode::Both && has_picture).then(|| self.window_icon(*id, icon_px)).flatten();
                let big = (!has_picture).then(|| self.window_icon(*id, big_px)).flatten();
                (small, big)
            })
            .collect();
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(overlay.layer.wl_surface()));
        canvas.fill(0x00000000);
        let text = self.text.as_ref();
        let switcher = &overlay.state.switcher;
        let bw = shown as i32 * (THUMB_W + THUMB_GAP) + THUMB_GAP;
        let bh = THUMB_H + THUMB_TITLE_H + 2 * THUMB_GAP;
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 2;
        theme::popover(&mut canvas, bx, by, bw, bh);
        overlay.hits.clear();
        for (slot, index) in (first..switcher.entries.len()).take(shown).enumerate() {
            let entry = &switcher.entries[index];
            let tile = Rect { x: bx + THUMB_GAP + slot as i32 * (THUMB_W + THUMB_GAP), y: by + THUMB_GAP, w: THUMB_W, h: THUMB_H + THUMB_TITLE_H };
            let hit = OverlayHit::Window(entry.id);
            if index == switcher.selected {
                canvas.round_rect(f64::from(tile.x - 4), f64::from(tile.y - 4), f64::from(tile.w + 8), f64::from(tile.h + 4), theme::RADIUS_BUTTON, accent());
            } else {
                let look = theme::Look { hovered: overlay.state.hover == Some(hit), pressed: overlay.state.pressed == Some(hit), ..Default::default() };
                theme::flat(&mut canvas, (f64::from(tile.x - 4), f64::from(tile.y - 4), f64::from(tile.w + 8), f64::from(tile.h + 4)), theme::RADIUS_BUTTON, look);
            }
            let pic = Rect { x: tile.x + 4, y: tile.y + 4, w: tile.w - 8, h: THUMB_H - 8 };
            match self.overview_shots.picture(entry.id) {
                Some(image) if image.width > 0 && image.height > 0 => {
                    let s = (f64::from(pic.w) / image.width as f64).min(f64::from(pic.h) / image.height as f64).min(1.0);
                    let (iw, ih) = (image.width as f64 * s, image.height as f64 * s);
                    let (ix, iy) = (f64::from(pic.x) + (f64::from(pic.w) - iw) / 2.0, f64::from(pic.y) + (f64::from(pic.h) - ih) / 2.0);
                    canvas.image_rounded(ix, iy, iw, ih, theme::RADIUS_ROW, image, 1.0);
                    if let Some(icon) = &icons[index].0 {
                        canvas.image(ix + iw - 28.0, iy + ih - 28.0, 32.0, 32.0, icon);
                    }
                }
                _ => match &icons[index].1 {
                    Some(icon) => canvas.image(f64::from(pic.x + (pic.w - 64) / 2), f64::from(pic.y + (pic.h - 64) / 2), 64.0, 64.0, icon),
                    None => theme::control(&mut canvas, pic.x, pic.y, pic.w, pic.h, theme::Look::default()),
                },
            }
            let tw = text.map_or(0.0, |t| t.width(&entry.label, size::CAPTION)).min((tile.w - 8) as f32);
            label(text, &mut canvas, &entry.label, tile.x as f32 + (tile.w as f32 - tw) / 2.0, baseline(text, tile.y + THUMB_H, THUMB_TITLE_H, size::CAPTION), size::CAPTION, FG, (tile.w - 8) as f32);
            overlay.hits.push((tile, OverlayHit::Window(entry.id)));
        }
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// The launcher results' icons by result index (the first ones), and
    /// each search provider's app icon by its first result's index.
    #[allow(clippy::type_complexity)]
    fn result_icons(&mut self, model: &Launcher, scale: f64) -> (HashMap<usize, Option<helix_draw::Image>>, HashMap<usize, Option<helix_draw::Image>>) {
        let px = (f64::from(RESULT_ICON) * scale).ceil() as u32;
        let small = (16.0 * scale).ceil() as u32;
        let app_icon = |cache: &mut crate::icon_cache::IconCache, id: &str, px: u32| {
            let app = id.strip_suffix(".desktop").unwrap_or(id).to_owned();
            cache.get(format!("app:{app}@{px}"), move || crate::icons::app_icon(&app, px))
        };
        let mut rows = HashMap::new();
        let mut headings = HashMap::new();
        for (i, hit) in model.results.iter().enumerate().take(80) {
            let icon = match *hit {
                launcher::Hit::Window(w) => model.windows.get(w).and_then(|e| self.window_icon(e.id, px)),
                launcher::Hit::App(a) => model.apps.get(a).and_then(|app| app_icon(&mut self.icon_cache, &app.id, px)),
                launcher::Hit::Search(e) => {
                    let found = model.external.get(e);
                    // Its own icon, else its provider's app's.
                    let own = found.and_then(|h| h.icon.clone()).and_then(|source| source_icon(&mut self.icon_cache, source, px));
                    own.or_else(|| found.and_then(|h| model.apps.iter().find(|a| a.name == h.app)).and_then(|app| app_icon(&mut self.icon_cache, &app.id, px)))
                }
                launcher::Hit::Folder(_) | launcher::Hit::More(_) => None,
            };
            rows.insert(i, icon);
        }
        for item in model.sections() {
            if let launcher::ListItem::Heading(launcher::Section::Provider(first)) = item {
                let app = model.external.get(first).and_then(|h| model.apps.iter().find(|a| a.name == h.app));
                headings.insert(first, app.and_then(|app| app_icon(&mut self.icon_cache, &app.id, small)));
            }
        }
        (rows, headings)
    }

    /// Window `id`'s icon at `px` pixels: its own, else its app's.
    fn window_icon(&mut self, id: u32, px: u32) -> Option<helix_draw::Image> {
        if let Some((name, image)) = self.window_icons.get(&id) {
            if let Some(image) = image {
                return Some(image.clone());
            }
            if !name.is_empty() {
                let key = format!("name:{name}@{px}");
                let source = crate::icons::IconSource::Names(vec![name.clone()]);
                if let Some(found) = self.icon_cache.get(key.clone(), move || crate::icons::load(&source, px)) {
                    return Some(found);
                }
                if self.icon_cache.is_pending(&key) {
                    // Its own icon is on the way: not the app's meanwhile.
                    return None;
                }
            }
        }
        let app_id = self.windows.mru().iter().find(|e| e.id == id).map(|e| e.app_id.clone())?;
        let key = format!("app:{app_id}@{px}");
        self.icon_cache.get(key, move || crate::icons::app_icon(&app_id, px))
    }

    fn draw_launcher(&mut self) {
        let _busy = crate::watchdog::scope("draw launcher");
        let Some(mut view) = self.launcher.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_launcher(&mut view);
        }
        let moving = view.model.query.is_empty() && view.grid.feedback.moving(Instant::now());
        self.launcher = Some(view);
        // The grid's feedback still moving: the next frame.
        if moving {
            self.grid_next_frame();
        }
    }

    fn paint_launcher(&mut self, view: &mut LauncherView) {
        let (w, h) = (view.width as usize, view.height as usize);
        let scale = self.scales.of(view.layer.wl_surface());
        // The results' icons (and the search providers' apps' icons for
        // their headings), asked for before drawing.
        let (row_icons, heading_icons) = if view.model.query.is_empty() { Default::default() } else { self.result_icons(&view.model, scale) };
        let Ok((buffer, data)) =
            self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate launcher buffer");
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(view.layer.wl_surface()));
        canvas.fill(SCRIM);

        // Lengths that hold text grow with the text scale.
        let grow = helix_draw::theme::grow;
        let bw = grow(SEARCH_W).min(w as i32 - 48);
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 / 6).max(panel_height() as i32 + 24);

        // Search field.
        let field = Rect { x: bx, y: by, w: bw, h: grow(SEARCH_H) };
        let placeholder = view.model.folder_name().unwrap_or(tr("Type to search…"));
        let clear_look = theme::Look { hovered: view.hover == Some(CLEAR_HIT), pressed: view.pressed == Some(CLEAR_HIT), ..Default::default() };
        view.clear = search_field_look(&mut canvas, self.text.as_ref(), field, (&view.model.query, placeholder), true, clear_look);

        // An empty search: the app grid.
        if view.model.query.is_empty() {
            view.rows.clear();
            view.content = field;
            // The frame without the cells, kept for repainting single cells
            // (`grid_repaint_cells`).
            let base = canvas.data.to_vec();
            app_grid::paint_grid(&mut app_grid::Paint { cache: &mut self.icon_cache, text: self.text.as_ref(), scale }, view, &mut canvas, w as i32, h as i32, by + field.h + 24);
            view.grid_cache = app_grid::GridCache::new(view, base, canvas.data.to_vec(), (canvas.width, canvas.height, scale));
            let surface = view.layer.wl_surface();
            self.scales.prepare(&self.qh, surface, w as i32, h as i32);
            if buffer.attach_to(surface).is_ok() {
                view.layer.commit();
            }
            return;
        }
        // Results, in sections: open windows, apps, each search provider.
        let list_top = by + field.h + 8;
        let fonts = (self.text.as_ref(), self.bold.as_ref());
        let (rows, list_h) = paint_results(&mut canvas, fonts, &view.model, (&row_icons, &heading_icons), (bx, list_top, bw, h as i32 - 24), (view.hover, view.pressed));
        view.rows = rows;
        view.content = Rect { x: bx, y: by, w: bw, h: list_top + list_h - by };

        let surface = view.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            view.layer.commit();
        }
    }

    fn launcher_key(&mut self, event: &KeyEvent) {
        if self.grid_key(event.keysym) {
            return;
        }
        let ctrl = self.ctrl_held;
        let Some(view) = self.launcher.as_mut() else { return };
        match event.keysym {
            Keysym::Escape => {
                // Out of a folder first.
                if !view.model.leave_folder() {
                    self.close_launcher(true);
                    return;
                }
            }
            Keysym::d | Keysym::D if ctrl => {
                if let Some(app) = view.model.selected_app().map(|a| a.id.clone()) {
                    let now = crate::favorites::toggle_favorite(&app);
                    tracing::info!(app, favorite = now, "favorites changed");
                    let selected = view.model.selected();
                    view.model.set_collections(crate::favorites::favorites(), view.model.folders.clone());
                    view.model.selected = selected.and_then(|s| view.model.results.iter().position(|r| *r == s)).unwrap_or(0);
                }
            }
            Keysym::BackSpace if view.model.query.is_empty() => {
                view.model.leave_folder();
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
        self.search_changed();
        self.draw_launcher();
    }

    // ----- Lock screen -------------------------------------------------------

    fn start_lock(&mut self) {
        if self.lock.session.is_some() {
            // Asked to lock while fading out after an unlock: stay locked.
            if self.lock.exiting {
                self.lock.exiting = false;
                let d = self.lock_duration(LOCK_ENTER);
                self.lock.shown.go(1.0, d);
                self.lock_animate_for(d);
                self.lock_reveal(false);
                self.draw_lock();
            }
            return;
        }
        self.finish_pick(crate::screenshot::Picked::Cancelled);
        self.finish_switching(false);
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::ShareScreen { .. } | DialogKind::RemoteControl { .. } | DialogKind::ApproveExtension { .. })) {
            self.answer_dialog(Choice::Canceled);
        }
        self.close_launcher(true);
        // Who is shown (AccountsService may have changed it).
        let _ = self.requests.send(UiRequest::LockIdentity(self.lock.user.clone()));
        self.lock.motion = crate::item_feedback::motion();
        // It starts resting; the UI fades in once the compositor says the
        // session is locked.
        self.lock.revealed = false;
        self.lock.reveal = Tween::still(0.0);
        self.lock.shown = Tween::still(0.0);
        self.lock.exiting = false;
        self.lock.focus = None;
        self.lock.prompt_at = None;
        self.lock.failed = false;
        match self.lock.state.lock(&self.qh) {
            Ok(session) => {
                self.lock.session = Some(session);
                self.dock_update();
            }
            Err(err) => {
                tracing::error!(%err, "compositor does not let this process lock the session");
                let _ = self.requests.send(UiRequest::LockGone);
            }
        }
    }

    fn lock_key(&mut self, event: &KeyEvent) {
        if self.lock.checking || self.lock.exiting {
            return;
        }
        // Resting: any key shows the prompt; a character also goes in
        // the field (the first key is never lost).
        if !self.lock.revealed {
            self.lock_reveal(true);
            let printable = event.utf8.as_ref().is_some_and(|t| t.chars().any(|c| !c.is_control()));
            if !printable {
                return;
            }
        }
        self.lock.last_input = Instant::now();
        // Escape on an empty field: back to resting.
        if event.keysym == Keysym::Escape && self.lock.password.is_empty() && self.lock.confirming.is_none() && self.lock.focus.is_none() {
            self.lock_reveal(false);
            return;
        }
        // The keyboard among the buttons: Tab moves, Return or Space
        // presses, Escape answers the question with Cancel.
        let shift = self.mods.shift;
        match event.keysym {
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                self.lock_move_focus(!shift && event.keysym == Keysym::Tab);
                self.draw_lock();
                return;
            }
            Keysym::Left | Keysym::Right if self.lock.confirming.is_some() => {
                self.lock_move_focus(event.keysym == Keysym::Right);
                self.draw_lock();
                return;
            }
            Keysym::Escape if self.lock.confirming.is_some() => {
                self.lock_activate(lock::LockHit::Power(LockPowerHit::Cancel));
                return;
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space if self.lock.focus.is_some() => {
                if let Some(hit) = self.lock.focus {
                    self.lock_activate(hit);
                }
                return;
            }
            _ if self.lock.confirming.is_some() => return,
            _ => {}
        }
        // Anything else goes to the field.
        self.lock.focus = None;
        let typed = match event.keysym {
            Keysym::Return | Keysym::KP_Enter => {
                self.submit_password();
                false
            }
            Keysym::Escape => {
                wipe(&mut self.lock.password);
                self.lock.message = None;
                true
            }
            Keysym::BackSpace => {
                self.lock.password.pop();
                true
            }
            _ => match &event.utf8 {
                Some(text) if text.chars().any(|c| !c.is_control()) => {
                    self.lock.password.extend(text.chars().filter(|c| !c.is_control()));
                    true
                }
                _ => false,
            },
        };
        // Typing again: the failure is over.
        if typed {
            self.lock.failed = false;
        }
        self.draw_lock();
    }

    fn submit_password(&mut self) {
        if self.lock.password.is_empty() {
            return;
        }
        self.lock.checked_len = self.lock.password.chars().count();
        let secret = Secret::new(std::mem::take(&mut self.lock.password));
        self.lock.checking = true;
        self.lock.checking_since = Instant::now();
        self.lock.failed = false;
        self.lock.message = Some(tr("Checking…").into());
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
        self.lock.checked_len = 0;
        match result {
            Ok(()) => {
                tracing::info!("unlocking");
                // The lock screen fades out (still locked, still opaque),
                // then the session shows.
                let exit = self.lock_duration(LOCK_EXIT);
                if exit.is_zero() {
                    self.finish_unlock();
                    return;
                }
                self.lock.exiting = true;
                self.lock.shown.go(0.0, exit);
                self.lock_animate_for(exit);
                // However the frames go, never later than this.
                match self.loop_handle.as_ref() {
                    Some(handle) => {
                        let _ = handle.insert_source(Timer::from_duration(exit + Duration::from_millis(40)), |_, _, ui: &mut Ui| {
                            if ui.lock.exiting {
                                ui.finish_unlock();
                            }
                            TimeoutAction::Drop
                        });
                    }
                    None => self.finish_unlock(),
                }
                self.draw_lock();
            }
            Err(AuthError::Denied) => {
                self.lock.message = Some(tr("Incorrect password. Try again.").into());
                self.lock.failed = true;
                if self.lock.motion.moves() {
                    self.lock.shaken = Some(Instant::now());
                    self.lock_animate_for(helix_draw::motion::SHAKE);
                }
                self.draw_lock();
            }
            Err(AuthError::Unavailable(err)) => {
                tracing::error!(%err, "cannot check passwords");
                self.lock.message = Some(trf!("Cannot check password: {err}", err = err));
                self.lock.failed = true;
                self.draw_lock();
            }
        }
    }

    fn draw_lock(&mut self) {
        self.draw_lock_views(false);
    }

    /// The session unlocks: the lock surfaces go and the session shows.
    fn finish_unlock(&mut self) {
        self.lock.exiting = false;
        if let Some(session) = self.lock.session.take() {
            session.unlock();
            self.lock.confirming = None;
        }
        self.lock.surfaces.clear();
        self.lock.message = None;
        let _ = self.requests.send(UiRequest::Unlocked);
        self.dock_update();
    }

    /// How long a lock screen transition of `full` takes with the motion
    /// allowed: nothing without animations.
    fn lock_duration(&self, full: Duration) -> Duration {
        if self.lock.motion.fades() { full } else { Duration::ZERO }
    }

    /// Show the prompt (`true`) or go back to the resting state.
    fn lock_reveal(&mut self, show: bool) {
        self.lock.last_input = Instant::now();
        if self.lock.revealed == show {
            return;
        }
        self.lock.revealed = show;
        let d = self.lock_duration(LOCK_REVEAL);
        self.lock.reveal.go(if show { 1.0 } else { 0.0 }, d);
        if !show {
            wipe(&mut self.lock.password);
            self.lock.focus = None;
            self.lock.failed = false;
            self.lock.message = None;
        }
        self.lock_animate_for(d);
        self.draw_lock();
    }

    /// A lock surface for `output`.
    fn add_lock_surface(&mut self, session: &SessionLock, output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if self.lock.surfaces.iter().any(|v| v.output == output) {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let lock_surface = session.create_lock_surface(surface, &output, qh);
        let origin = self.output_state.info(&output).and_then(|i| i.logical_position).unwrap_or_default();
        self.lock.surfaces.push(LockView { surface: lock_surface, output, width: 0, height: 0, hits: Vec::new(), origin, backdrop: None, backdrop_wanted: None });
    }

    /// Draw the lock surfaces (`prompt_only`: just the one with the
    /// prompt, when only it changes).
    fn draw_lock_views(&mut self, prompt_only: bool) {
        let prompt = self.lock_prompt_origin();
        let mut views = std::mem::take(&mut self.lock.surfaces);
        for view in &mut views {
            let has_prompt = Some(view.origin) == prompt;
            if has_prompt || !prompt_only {
                self.paint_lock(view, has_prompt);
            }
        }
        self.lock.surfaces = views;
        self.lock_next_frame();
    }

    /// Where the monitor with the prompt is (helix-desktop-spec
    /// `contracts/dbus/org.gnome.ScreenSaver.md`, "Monitors"): the one
    /// last clicked, while it is there, else the primary monitor, else
    /// the first.
    pub(super) fn lock_prompt_origin(&self) -> Option<(i32, i32)> {
        let shown = |origin: (i32, i32)| self.lock.surfaces.iter().any(|v| v.origin == origin && v.width > 0);
        let primary = (self.primary_area.0, self.primary_area.1);
        self.lock.prompt_at.filter(|o| shown(*o)).or_else(|| shown(primary).then_some(primary)).or_else(|| self.lock.surfaces.iter().find(|v| v.width > 0).map(|v| v.origin))
    }

    /// Something on the lock screen moves for `duration` from now: draw
    /// frames until then (none when nothing may move).
    fn lock_animate_for(&mut self, duration: Duration) {
        if !self.lock.motion.fades() {
            return;
        }
        let until = Instant::now() + duration;
        if self.lock.animate_until.is_none_or(|t| t < until) {
            self.lock.animate_until = Some(until);
        }
        self.lock_next_frame();
    }

    /// Whether the lock screen still moves.
    fn lock_animating(&self) -> bool {
        let spinning = self.lock.checking && self.lock.motion.moves();
        self.lock.session.is_some() && (spinning || self.lock.animate_until.is_some_and(|t| Instant::now() < t))
    }

    /// Draw the lock screen again in a frame's time while it moves.
    fn lock_next_frame(&mut self) {
        if self.lock.frame_timer.is_some() || !self.lock_animating() {
            return;
        }
        let Some(handle) = self.loop_handle.as_ref() else { return };
        let token = handle.insert_source(Timer::from_duration(Duration::from_millis(16)), |_, _, ui: &mut Ui| {
            ui.lock.frame_timer = None;
            ui.lock_frame();
            TimeoutAction::Drop
        });
        self.lock.frame_timer = token.ok();
    }

    /// A frame of the lock screen's movement.
    fn lock_frame(&mut self) {
        // Fading in or out: every monitor; else only the prompt moves (the
        // spinner, the shake, the reveal).
        self.draw_lock_views(self.lock.shown.done());
        if self.lock.exiting && self.lock.shown.done() {
            self.finish_unlock();
        }
    }

    /// A lock surface's background was made: shown if still wanted.
    fn lock_backdrop_made(&mut self, output: &wl_output::WlOutput, key: lock::BackdropKey, pixels: Vec<u8>) {
        let Some(view) = self.lock.surfaces.iter_mut().find(|v| &v.output == output && v.backdrop_wanted == Some(key)) else { return };
        view.backdrop_wanted = None;
        view.backdrop = Some((key, pixels));
        self.draw_lock();
    }

    fn paint_lock(&mut self, view: &mut LockView, prompt: bool) {
        let _busy = crate::watchdog::scope("draw lock screen");
        let (w, h) = (view.width as usize, view.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        let scale = self.scales.of(view.surface.wl_surface());
        let counts = self.lock_note_counts();
        let icons = self.lock_icons(scale, &counts);
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        // The background, blurred and toned once per picture and size, on
        // a worker (a large picture takes a while). Meanwhile the one
        // before if it fits, else plain black.
        let key = lock::BackdropKey { serial: self.lock_background_serial, size: (pw, ph), scale: scale.to_bits(), high_contrast: helix_draw::high_contrast() };
        if view.backdrop.as_ref().is_none_or(|(k, _)| *k != key) && view.backdrop_wanted != Some(key) {
            view.backdrop_wanted = Some(key);
            let (background, output) = (self.lock_background.clone(), view.output.clone());
            self.offload(move || lock::backdrop(&background, (w, h), scale), move |ui, pixels| ui.lock_backdrop_made(&output, key, pixels));
        }
        let Ok((buffer, data)) = self.pool.create_buffer(pw as i32, ph as i32, (pw * 4) as i32, wl_shm::Format::Argb8888) else {
            tracing::warn!("cannot allocate lock screen buffer");
            return;
        };
        match &view.backdrop {
            Some((k, pixels)) if k.size == key.size => data.copy_from_slice(pixels),
            _ => helix_draw::backdrop::high_contrast(data),
        }
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(view.surface.wl_surface()));
        let (clock, suffix, date) = lock_clock();
        self.shown_lock_clock = Some(lock_clock_text());
        let restart = crate::screensaver_flag("restart-enabled", false);
        let model = lock::LockModel {
            prompt,
            clock: &clock,
            suffix: suffix.as_deref(),
            date: &date,
            user: if self.lock.identity.name.is_empty() { &self.lock.user } else { &self.lock.identity.name },
            picture: self.lock.identity.picture.as_ref(),
            typed: if self.lock.checking { self.lock.checked_len } else { self.lock.password.chars().count() },
            checking: self.lock.checking,
            failed: self.lock.failed,
            shake: self.lock.shaken.map_or(0.0, |t| helix_draw::motion::shake(t.elapsed())),
            spinner: self.lock.motion.moves().then(|| helix_draw::motion::spin(self.lock.checking_since.elapsed())),
            message: self.lock.message.as_deref(),
            media: self.lock_extras.media.as_ref(),
            counts: &counts,
            icons: &icons,
            restart: restart && self.power.restart,
            power_off: restart && self.power.power_off,
            confirming: self.lock.confirming,
            blockers: &self.lock.blockers,
            hover: self.lock.hover.as_ref().filter(|(s, _)| s == view.surface.wl_surface()).map(|(_, h)| *h),
            pressed: self.lock.pressed.as_ref().filter(|(s, _)| s == view.surface.wl_surface()).map(|(_, h)| *h),
            focus: self.lock.focus,
            caps_lock: self.mods.caps_lock,
            revealed: self.lock.revealed,
            reveal_pos: if self.lock.motion.moves() { self.lock.reveal.value() } else { self.lock.reveal.target() },
            reveal_alpha: self.lock.reveal.value(),
            alpha: self.lock.shown.value(),
            slide: if self.lock.motion.moves() && !self.lock.exiting { (1.0 - self.lock.shown.value()) * 12.0 } else { 0.0 },
            layout: self.source_label.as_deref(),
            switch_user: self.user_switch && crate::user_switch::allowed() && crate::user_switch::on_lock_screen(),
        };
        let fonts = lock::LockFonts { text: self.text.as_ref(), bold: self.bold.as_ref(), display: self.display.as_ref() };
        // The lock screen keeps its own type scale (Large Text there is
        // not done yet; helix-desktop#16).
        let text_scale = helix_draw::theme::text_scale();
        helix_draw::theme::set_text_scale(1.0);
        let painted = lock::paint(&mut canvas, &fonts, &model, w, h);
        helix_draw::theme::set_text_scale(f64::from(text_scale));
        view.hits = painted.hits;
        // The same on the accessibility bus.
        self.atspi_record(view.surface.wl_surface(), painted.nodes);

        let surface = view.surface.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
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
        let height = self.banner_height(&data);
        if let Some(view) = self.banner.as_mut().filter(|v| v.data.id == data.id && v.data.critical == data.critical) {
            view.data = data;
            if view.height != height || view.width != banner_w() {
                view.layer.set_size(banner_w(), height);
                view.layer.commit();
            } else {
                self.draw_banner();
            }
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        // The top layer, which a full-screen window hides (as GNOME holds
        // banners back in full screen); critical ones go above it.
        let kind = if data.critical { Layer::Overlay } else { Layer::Top };
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, kind, Some("helix-notification"), None);
        layer.set_anchor(Anchor::TOP);
        layer.set_margin(8, 0, 0, 0);
        layer.set_size(banner_w(), height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.banner = Some(BannerView { data, layer, width: 0, height: 0, configured: false, hits: Vec::new(), focused: false, hover: None, pressed: None });
    }

    fn draw_banner(&mut self) {
        let Some(mut view) = self.banner.take() else { return };
        if view.configured && view.width > 0 && view.height > 0 {
            self.paint_banner(&mut view);
        }
        self.banner = Some(view);
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
                self.show_dialog(DialogState::new(DialogKind::End(data), remaining));
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
            Some(remaining) => self.show_dialog(DialogState::new(DialogKind::DisplayChange, remaining)),
            None => {
                if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::DisplayChange)) {
                    self.dialog = None;
                }
            }
        }
    }

    /// Tell the compositor whether window `id` may have all shortcuts.
    fn answer_shortcuts(&self, id: u32, allowed: bool) {
        if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 6) {
            list.allow_shortcuts_inhibit(id, u32::from(allowed));
        }
    }

    /// Window `id` asks for all keyboard shortcuts.
    fn shortcuts_inhibit(&mut self, id: u32) {
        if self.lock.session.is_some() {
            self.answer_shortcuts(id, false);
            return;
        }
        let app_id = self.windows.mru().iter().find(|e| e.id == id).map(|e| e.app_id.clone()).unwrap_or_default();
        if app_id.is_empty() {
            self.ask_shortcuts(id, app_id);
        } else {
            let _ = self.requests.send(UiRequest::ShortcutsInhibit { id, app_id });
        }
    }

    /// Ask the user (nothing remembered) whether window `id` may have all
    /// shortcuts.
    fn ask_shortcuts(&mut self, id: u32, app_id: String) {
        if self.lock.session.is_some() || self.dialog.is_some() {
            self.answer_shortcuts(id, false);
            return;
        }
        let entry = self.windows.mru().iter().find(|e| e.id == id).cloned();
        let Some(entry) = entry else { return };
        let app = crate::windows::app_names()
            .remove(&app_id.to_lowercase())
            .unwrap_or_else(|| if app_id.is_empty() { entry.label().to_owned() } else { app_id.clone() });
        self.show_dialog(DialogState::new(DialogKind::ShortcutsInhibit { id, app_id, app }, 0));
    }

    /// Window `id`'s app answered (or the window went away): close its
    /// dialog.
    fn close_not_responding(&mut self, id: u32) {
        let about = |kind: &DialogKind| match kind {
            DialogKind::NotResponding { id: shown, .. } | DialogKind::ShortcutsInhibit { id: shown, .. } => *shown == id,
            _ => false,
        };
        if self.dialog.as_ref().is_some_and(|d| about(&d.state.kind)) {
            self.dialog = None;
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
                if let DialogKind::RemoteControl { reply, .. } | DialogKind::ApproveExtension { reply, .. } = &mut open.state.kind {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(false);
                    }
                }
                if let DialogKind::Prompt(view) = &mut open.state.kind {
                    view.cancel();
                }
                if let DialogKind::ShortcutsInhibit { id, .. } = open.state.kind {
                    if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 6) {
                        list.allow_shortcuts_inhibit(id, 0);
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
        crate::click_feedback();
        if self.dialog.as_ref().is_some_and(|d| matches!(d.state.kind, DialogKind::Prompt(_))) {
            self.prompt_answer(choice == Choice::Confirmed);
            return;
        }
        let Some(dialog) = self.dialog.take() else { return };
        let request = match dialog.state.kind {
            DialogKind::End(d) => UiRequest::EndDialogAnswer(choice, d.install),
            DialogKind::DisplayChange => UiRequest::DisplayChangeAnswer { keep: choice == Choice::Confirmed },
            DialogKind::RemoteControl { reply, .. } | DialogKind::ApproveExtension { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(choice == Choice::Confirmed);
                }
                return;
            }
            DialogKind::ShortcutsInhibit { id, app_id, .. } => {
                let allowed = choice == Choice::Confirmed;
                self.answer_shortcuts(id, allowed);
                if !app_id.is_empty() {
                    let _ = self.requests.send(UiRequest::StoreShortcutsAnswer { app_id, allowed });
                }
                return;
            }
            DialogKind::NotResponding { id, .. } => {
                let action = if choice == Choice::Confirmed { WindowAction::ForceQuit } else { WindowAction::Wait };
                if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 5) {
                    list.window_action(id, action);
                }
                return;
            }
            DialogKind::Prompt(_) => return,
            DialogKind::ShareScreen { sources, reply, multiple, picked, .. } => {
                // Confirm (Enter, Share) shares the rows picked, else the
                // first entry.
                let chosen = (choice == Choice::Confirmed).then(|| {
                    if multiple && !picked.is_empty() {
                        picked.iter().filter_map(|i| sources.get(*i).map(|s| s.0.clone())).collect()
                    } else {
                        sources.first().map(|s| vec![s.0.clone()]).unwrap_or_default()
                    }
                });
                if let Some(reply) = reply {
                    let _ = reply.send(chosen);
                }
                return;
            }
        };
        let _ = self.requests.send(request);
    }

    /// A row of the share dialog was clicked: shared at once, or (several
    /// may be chosen) picked or unpicked.
    fn share_monitor(&mut self, index: usize) {
        if let Some(DialogKind::ShareScreen { multiple: true, picked, .. }) = self.dialog.as_mut().map(|d| &mut d.state.kind) {
            match picked.iter().position(|i| *i == index) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(index),
            }
            self.draw_dialog();
            return;
        }
        let Some(dialog) = self.dialog.take() else { return };
        if let DialogKind::ShareScreen { sources, reply: Some(reply), .. } = dialog.state.kind {
            let _ = reply.send(sources.get(index).map(|s| vec![s.0.clone()]));
        }
    }

    fn dialog_key(&mut self, event: &KeyEvent) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let ready = dialog_takes_key(dialog.state.shown, Instant::now());
        if matches!(dialog.state.kind, DialogKind::Prompt(_)) {
            // Typing goes in at once; only Enter waits.
            if ready || !matches!(event.keysym, Keysym::Return | Keysym::KP_Enter) {
                self.prompt_key(event);
            }
            return;
        }
        if !ready {
            tracing::debug!("key too soon after the dialog appeared; ignored");
            return;
        }
        match dialog_key_action(event.keysym, dialog.state.focused()) {
            Some(DialogKey::Answer(choice)) => self.answer_dialog(choice),
            // The share dialog has no buttons to move between.
            Some(DialogKey::Focus(_)) if matches!(dialog.state.kind, DialogKind::ShareScreen { .. }) => {}
            Some(DialogKey::Focus(choice)) => {
                if dialog.state.focus != Some(choice) {
                    dialog.state.focus = Some(choice);
                    self.draw_dialog();
                }
            }
            Some(DialogKey::ToggleInstall) => self.end_toggle_install(),
            None => {}
        }
    }

    /// Tick or untick "Install pending software updates".
    fn end_toggle_install(&mut self) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        if let DialogKind::End(EndDialog { install: Some(on), .. }) = &mut dialog.state.kind {
            *on = !*on;
            crate::click_feedback();
            self.draw_dialog();
        }
    }

    /// One second passed: count down. At zero, ending the session goes ahead
    /// (unless something blocks it) and a display change is reverted.
    fn dialog_tick(&mut self) {
        let Some(dialog) = self.dialog.as_mut() else { return };
        let on_timeout = match &dialog.state.kind {
            DialogKind::ShareScreen { .. }
            | DialogKind::NotResponding { .. }
            | DialogKind::RemoteControl { .. }
            | DialogKind::ApproveExtension { .. }
            | DialogKind::ShortcutsInhibit { .. }
            | DialogKind::Prompt(_) => return,
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
        if matches!(overlay.state.kind, DialogKind::Prompt(_)) {
            self.paint_prompt(overlay);
            return;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(overlay.layer.wl_surface()));
        let Some((hits, node)) = paint_question(&mut canvas, (self.text.as_ref(), self.bold.as_ref()), (w, h), &overlay.state) else { return };
        overlay.hits = hits;
        overlay.state.shown.get_or_insert_with(Instant::now);
        self.atspi_record(overlay.layer.wl_surface(), vec![node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    /// "Share your screen?": one row per monitor, and Cancel.
    fn paint_share_dialog(&mut self, overlay: &mut Overlay<DialogState>) {
        let DialogKind::ShareScreen { app, sources, remember, control, multiple, picked, .. } = &overlay.state.kind else { return };
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale).mirrored(mirror::on(overlay.layer.wl_surface()));
        canvas.fill(SCRIM);
        let text = self.text.as_ref();
        let rows = sources.len().min(8) as i32;
        let bw = helix_draw::theme::grow(460).min(w as i32 - 40);
        let row_h = helix_draw::theme::grow(ROW_H);
        let bh = helix_draw::theme::grow(140) + rows * (row_h + 8) + helix_draw::theme::grow(56) + if *remember { helix_draw::theme::grow(12) } else { 0 };
        let bx = (w as i32 - bw) / 2;
        let by = (h as i32 - bh) / 3;
        theme::popover(&mut canvas, bx, by, bw, bh);
        let inner = (bw - 48) as f32;
        let center = |s: &str, px: f32| bx as f32 + (bw as f32 - text.map_or(0.0, |t| t.width(s, px).min(inner))) / 2.0;
        let title = tr("Share your screen?");
        let bold = self.bold.as_ref().or(text);
        let tw = bold.map_or(0.0, |t| t.width(title, size::TITLE).min(inner));
        label(bold, &mut canvas, title, bx as f32 + (bw as f32 - tw) / 2.0, (by + helix_draw::theme::grow(44)) as f32, size::TITLE, FG, inner);
        let message = if control.is_empty() {
            trf!("{app} wants to see your screen. Choose what to share.", app = app)
        } else {
            trf!("{app} wants to see your screen and control your {devices}.", app = app, devices = crate::screencast_chooser::device_list(control))
        };
        label(text, &mut canvas, &message, center(&message, size::LABEL), (by + helix_draw::theme::grow(78)) as f32, size::LABEL, DIM_FG, inner);
        if *remember {
            let note = tr("It will be able to share the same screen again without asking.");
            label(text, &mut canvas, note, center(note, size::CAPTION), (by + helix_draw::theme::grow(96)) as f32, size::CAPTION, DIM_FG, inner);
        }
        let mut hits = Vec::new();
        let top = if *remember { by + helix_draw::theme::grow(112) } else { by + helix_draw::theme::grow(100) };
        for (i, (_, name, connector)) in sources.iter().take(8).enumerate() {
            let r = Rect { x: bx + 24, y: top + i as i32 * (row_h + 8), w: bw - 48, h: row_h };
            let on = picked.contains(&i);
            let hit = OverlayHit::Monitor(i);
            let look = theme::Look { on, hovered: overlay.state.hover == Some(hit), pressed: overlay.state.pressed == Some(hit) };
            theme::control(&mut canvas, r.x, r.y, r.w, r.h, look);
            let fg = theme::control_text(look);
            let base = baseline(text, r.y, r.h, size::BODY);
            let mut nx = r.x + 12;
            if *multiple {
                // A check mark per row (the row itself is the box).
                let (cx, cy) = (r.x + 12, r.y + (r.h - 14) / 2);
                canvas.round_outline(f64::from(cx) + 1.0, f64::from(cy) + 1.0, 12.0, 12.0, theme::RADIUS_CHECK - 1.0, 1.0, fg);
                if on {
                    canvas.line((f64::from(cx) + 3.0, f64::from(cy) + 7.5), (f64::from(cx) + 6.0, f64::from(cy) + 10.5), 1.8, fg);
                    canvas.line((f64::from(cx) + 6.0, f64::from(cy) + 10.5), (f64::from(cx) + 11.0, f64::from(cy) + 3.5), 1.8, fg);
                }
                nx += 24;
            }
            label(text, &mut canvas, name, nx as f32, base, size::BODY, fg, (r.x + r.w - nx) as f32 - 120.0);
            let cw = text.map_or(0.0, |t| t.width(connector, size::CAPTION).min(100.0));
            label(text, &mut canvas, connector, r.x as f32 + r.w as f32 - 12.0 - cw, base, size::CAPTION, if on { fg } else { DIM_FG }, 100.0);
            hits.push((r, OverlayHit::Monitor(i)));
        }
        // Cancel; with several to choose, Share beside it.
        let cw = if *multiple { (bw - 48 - 12) / 2 } else { bw - 48 };
        let cancel = Rect { x: bx + 24, y: by + bh - helix_draw::theme::grow(56), w: cw, h: helix_draw::theme::grow(36) };
        let look = |hit| theme::Look { hovered: overlay.state.hover == Some(hit), pressed: overlay.state.pressed == Some(hit), ..Default::default() };
        theme::button(&mut canvas, text, (cancel.x, cancel.y, cancel.w, cancel.h), tr("Cancel"), theme::Kind::Normal, look(OverlayHit::Cancel), false);
        hits.push((cancel, OverlayHit::Cancel));
        if *multiple {
            let share = Rect { x: cancel.x + cw + 12, y: cancel.y, w: cw, h: cancel.h };
            if picked.is_empty() {
                // Nothing picked: Share cannot be used yet.
                theme::control(&mut canvas, share.x, share.y, share.w, share.h, theme::Look::default());
                let sw = text.map_or(0.0, |t| t.width(tr("Share"), size::BODY));
                label(text, &mut canvas, tr("Share"), share.x as f32 + (share.w as f32 - sw) / 2.0, baseline(text, share.y, share.h, size::BODY), size::BODY, hint_text(), share.w as f32);
            } else {
                theme::button(&mut canvas, text, (share.x, share.y, share.w, share.h), tr("Share"), theme::Kind::Suggested, look(OverlayHit::Confirm), false);
            }
            if !picked.is_empty() {
                hits.push((share, OverlayHit::Confirm));
            }
        }
        let node = atspi::share_dialog(&message, *remember, sources, *multiple, picked, &hits);
        overlay.hits = hits;
        self.atspi_record(overlay.layer.wl_surface(), vec![node]);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
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
            layer.set_margin(panel_height() as i32 + 12, 0, 0, 12);
            layer.set_size(MONITOR_LABEL_SIZE, MONITOR_LABEL_SIZE);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            tracing::debug!(name, number, "monitor label");
            let origin = self.output_state.info(&output).and_then(|i| i.logical_position).unwrap_or((0, 0));
            self.monitor_labels.push(Overlay {
                layer,
                width: 0,
                height: 0,
                configured: false,
                hits: Vec::new(),
                state: (number, origin, name),
            });
        }
    }

    fn draw_monitor_label(&mut self, index: usize) {
        let Some(overlay) = self.monitor_labels.get(index) else { return };
        if !overlay.configured || overlay.width == 0 {
            return;
        }
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let scale = self.scales.of(overlay.layer.wl_surface());
        let Ok((buffer, data)) = self.pool.create_buffer(physical(w, scale) as i32, physical(h, scale) as i32, (physical(w, scale) * 4) as i32, wl_shm::Format::Argb8888) else {
            return;
        };
        let mut canvas = Canvas::scaled(data, w, h, scale);
        canvas.fill(0);
        theme::popover(&mut canvas, 2, 2, w as i32 - 4, h as i32 - 4);
        let text = self.text.as_ref();
        let number = overlay.state.0.to_string();
        let px = size::DISPLAY;
        let width = text.map(|t| t.width(&number, px)).unwrap_or(0.0);
        let base = baseline(text, 0, h as i32, px);
        label(text, &mut canvas, &number, (w as f32 - width) / 2.0, base, px, FG, w as f32);
        let surface = overlay.layer.wl_surface();
        self.scales.prepare(&self.qh, surface, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            overlay.layer.commit();
        }
    }

    fn handle(&mut self, event: UiEvent) {
        match event {
            UiEvent::DesktopIcons(items) => self.desktop_icons_set(items),
            UiEvent::PanelLayout(keys) => self.panel_layout_changed(keys),
            UiEvent::IconTheme => {
                // Every cached icon came from the old theme.
                crate::icons::forget_theme();
                self.icon_cache.clear();
                self.draw_panel();
                self.dock_update();
            }
            UiEvent::Tray(items) => self.tray_changed(items),
            UiEvent::Extensions(snapshot) => self.extensions_changed(snapshot),
            UiEvent::ExtensionList(list) => {
                self.ext_list = list;
                self.draw_extensions_dialog();
            }
            UiEvent::ExtMenu { full_id, menu, x, entries } => self.show_ext_menu(full_id, menu, x, entries),
            UiEvent::TrayMenu { key, x, entries } => self.show_tray_menu(key, x, entries),
            UiEvent::ClickTypeWindow(wanted) => {
                self.click_type.wanted = wanted;
                self.click_type_sync();
            }
            UiEvent::ClickType(current) => {
                if self.click_type.current != current {
                    self.click_type.current = current;
                    self.draw_click_type();
                }
            }
            UiEvent::A11y(status) => {
                let changed = status.shown != self.a11y.shown;
                self.a11y = status;
                if changed {
                    self.draw_panel();
                }
            }
            UiEvent::Dock(settings) => {
                dock::forget_desktop_ids();
                // It may move away from under the pointer.
                self.dock_reset_pointer();
                let rebind = (self.dock.settings.enabled, &self.dock.settings.hot_keys) != (settings.enabled, &settings.hot_keys);
                self.dock.settings = settings;
                self.dock_filter_mounts();
                self.dock_update();
                if rebind {
                    self.rebind_internal();
                }
            }
            UiEvent::LauncherEntry(app, entry) => {
                let was_urgent = self.dock.entries.get(&app.to_lowercase()).is_some_and(|e| e.urgent);
                if entry.as_ref().is_some_and(|e| e.urgent) && !was_urgent {
                    self.dock_urgent(&app);
                }
                match entry {
                    Some(entry) => self.dock.entries.insert(app.to_lowercase(), entry),
                    None => self.dock.entries.remove(&app.to_lowercase()),
                };
                self.dock_draw();
            }
            UiEvent::Mounts(mounts) => {
                self.dock.all_mounts = mounts;
                self.dock_filter_mounts();
                self.dock_update();
            }
            UiEvent::Trash(full) => {
                if self.dock.trash_full != full {
                    self.dock.trash_full = full;
                    self.dock_draw();
                }
            }
            UiEvent::Notifications(list) => {
                self.notifications_changed(list);
                self.dock_draw();
            }
            UiEvent::Calendar(data) => self.calendar_data(*data),
            UiEvent::Break(view) => self.show_break(view),
            UiEvent::LockMedia(player) => self.lock_media(player),
            UiEvent::LockArt(url, art) => self.lock_art(url, art),
            UiEvent::LockIdentity(identity) => {
                if self.lock.identity != *identity {
                    self.lock.identity = *identity;
                    self.draw_lock();
                }
            }
            UiEvent::DoNotDisturb(on) => {
                self.notify.do_not_disturb = on;
                self.draw_notification_list();
            }
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
                        if q.virtual_monitor {
                            sources.push((Shared::Virtual, tr("New virtual monitor").to_owned(), String::new()));
                        }
                        if sources.is_empty() {
                            let _ = q.reply.send(None);
                        } else {
                            let kind = DialogKind::ShareScreen { app: q.app, sources, remember: q.remember, control: q.control, multiple: q.multiple, picked: Vec::new(), reply: Some(q.reply) };
                            self.show_dialog(DialogState::new(kind, 0));
                        }
                    }
                }
            }
            UiEvent::Windows(slot) => {
                if let Some(reply) = slot.take() {
                    let _ = reply.send(self.windows.mru().iter().map(|w| (w.id, w.app_id.clone(), w.title.clone())).collect());
                }
            }
            UiEvent::ApproveExtension(slot) => {
                if let Some(q) = slot.take() {
                    // Not over the lock screen, and one question at a time.
                    if self.lock.session.is_some() || self.dialog.is_some() {
                        let _ = q.reply.send(false);
                    } else {
                        let lines = crate::ext_host::approval::lines(&q.can, q.sandboxed);
                        let kind = DialogKind::ApproveExtension { name: q.name, lines, reply: Some(q.reply) };
                        self.show_dialog(DialogState::new(kind, 0));
                    }
                }
            }
            UiEvent::AllowControl(slot) => {
                if let Some(q) = slot.take() {
                    if self.lock.session.is_some() || self.dialog.is_some() {
                        let _ = q.reply.send(false);
                    } else {
                        let kind = DialogKind::RemoteControl { app: q.app, devices: q.devices, reply: Some(q.reply) };
                        self.show_dialog(DialogState::new(kind, 0));
                    }
                }
            }
            UiEvent::Ime(event) => self.ime_event(event),
            UiEvent::QuickSettings { volume, brightness } => self.quick_settings_changed(quick::QuickState { volume, brightness }),
            UiEvent::ShortcutsAnswer { id, allowed } => self.answer_shortcuts(id, allowed),
            UiEvent::AskShortcuts { id, app_id } => self.ask_shortcuts(id, app_id),
            UiEvent::Sharing(on) => {
                self.sharing = on;
                self.draw_panel();
            }
            UiEvent::SearchResults { serial, provider, hits } => {
                if serial == self.search_serial {
                    if let Some(view) = self.launcher.as_mut() {
                        view.model.set_search(provider, hits);
                        self.draw_launcher();
                    }
                }
            }
            UiEvent::InterfaceA11y { shapes, focus_timeout } => {
                self.focus_timeout = focus_timeout;
                if shapes != helix_draw::status_shapes() {
                    tracing::info!(shapes, "status shapes");
                    helix_draw::set_status_shapes(shapes);
                    self.redraw_all();
                    self.draw_power_menu();
                }
            }
            UiEvent::TextScale(factor) => self.text_scale_changed(factor),
            UiEvent::Accent(accent) => self.accent_changed(accent),
            UiEvent::InterfaceFont(family) => self.font_changed(family),
            UiEvent::HighContrast(on) => {
                if on != helix_draw::high_contrast() {
                    tracing::info!(on, "high contrast");
                    helix_draw::set_high_contrast(on);
                    self.redraw_all();
                    self.draw_workspace_popup();
                    self.draw_overview();
                }
            }
            UiEvent::Status(status) => {
                self.status = *status;
                self.draw_panel();
                self.draw_power_menu();
            }
            UiEvent::Recording(on) => {
                self.recording = on;
                self.draw_panel();
            }
            UiEvent::Screenshot(slot) => {
                if let Some(job) = slot.take() {
                    self.screenshot_job(job);
                }
            }
            UiEvent::PowerAvailability(power) => self.power = power,
            UiEvent::UserSwitch(available) => self.user_switch = available,
            UiEvent::FoldersChanged => self.grid_reload(),
            UiEvent::LockBlockers(blockers) => {
                self.lock.blockers = blockers;
                self.draw_lock();
            }
            UiEvent::ActivateWindow(id) => self.activate_window(id),
            UiEvent::Lock => self.start_lock(),
            UiEvent::Banner(banner) => self.show_banner(banner),
            UiEvent::Osd(osd) => self.show_osd(osd),
            UiEvent::Overview(true) => self.open_overview(false),
            UiEvent::Overview(false) => {
                self.close_overview(false);
                self.close_launcher(false);
            }
            UiEvent::Launcher => self.open_launcher(false),
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
            UiEvent::RebindKeys => self.rebind_internal(),
            UiEvent::Prompt(slot) => {
                if let Some(request) = slot.take() {
                    self.show_prompt(request);
                }
            }
            UiEvent::PromptDone => self.prompt_done(),
            UiEvent::PromptRows(rows) => self.prompt_rows(rows),
            UiEvent::PromptPreview(path, image) => self.prompt_preview(path, image),
            UiEvent::Background { desktop, lock } => {
                self.desktop_background = *desktop;
                self.lock_background = *lock;
                self.lock_background_serial += 1;
                self.draw_backgrounds();
                if self.lock.session.is_some() {
                    self.draw_lock();
                }
            }
            UiEvent::Unbind { action } => {
                if let Some(binding) = self.bindings.remove(&action) {
                    binding.destroy();
                }
            }
        }
    }

    /// Key settings changed: drop the shell's own bindings and bind again.
    fn rebind_internal(&mut self) {
        let ids: Vec<u32> = self.bindings.keys().copied().filter(|a| *a >= INTERNAL_ACTIONS).collect();
        for id in ids {
            if let Some(binding) = self.bindings.remove(&id) {
                binding.destroy();
            }
        }
        self.internal.clear();
        self.bind_internal();
        tracing::info!(bound = self.internal.len(), "shell keys bound again");
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
        // Extensions' commands (taken keys were refused when set).
        let keys: Vec<String> = self.ext.commands.iter().map(|c| c.accelerator.clone()).collect();
        for (i, text) in keys.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let Some(accelerator) = parse(text, "extension command") else { continue };
            self.internal.insert(next, Internal::ExtCommand(i));
            self.bind(next, &accelerator);
            next += 1;
        }
        for (key, defaults, internal) in [
            ("toggle-overview", &[][..], Internal::Overview),
            ("toggle-application-view", &["<Super>a"][..], Internal::Launcher),
            ("shift-overview-up", &["<Super><Alt>Up"][..], Internal::ShiftOverview(true)),
            ("shift-overview-down", &["<Super><Alt>Down"][..], Internal::ShiftOverview(false)),
            ("toggle-message-tray", &["<Super>v", "<Super>m"][..], Internal::MessageTray),
            ("focus-active-notification", &["<Super>n"][..], Internal::FocusNotification),
            ("toggle-quick-settings", &["<Super>s"][..], Internal::QuickSettings),
        ] {
            for text in crate::shell_keys::shell_strv(key, defaults) {
                let Some(accelerator) = parse(&text, "overview") else { continue };
                self.internal.insert(next, internal);
                self.bind(next, &accelerator);
                next += 1;
            }
        }
        for text in crate::shell_keys::wm_strv("panel-run-dialog", &["<Alt>F2"]) {
            let Some(accelerator) = parse(&text, "run dialog") else { continue };
            self.internal.insert(next, Internal::RunDialog);
            self.bind(next, &accelerator);
            next += 1;
        }
        if self.window_list.is_none() {
            return;
        }
        match self.dock.settings.hot_keys.clone().filter(|_| self.dock.settings.enabled) {
            // Dash to Dock's keys replace GNOME's.
            Some(keys) => {
                for (n, which, text) in keys.apps {
                    let Some(accelerator) = parse(&text, "dock hot key") else { continue };
                    self.internal.insert(next, Internal::DockHotKey(n, which));
                    self.bind(next, &accelerator);
                    next += 1;
                }
                for text in keys.shortcut {
                    let Some(accelerator) = parse(&text, "dock shortcut") else { continue };
                    self.internal.insert(next, Internal::DockShortcut);
                    self.bind(next, &accelerator);
                    next += 1;
                }
            }
            None => {
                for n in 1..=9usize {
                    for (key, default, launch) in [
                        (format!("switch-to-application-{n}"), format!("<Super>{n}"), false),
                        (format!("open-new-window-application-{n}"), format!("<Super><Control>{n}"), true),
                    ] {
                        for text in crate::shell_keys::shell_strv(&key, &[default.as_str()]) {
                            let Some(accelerator) = parse(&text, "switch to application") else { continue };
                            self.internal.insert(next, Internal::DockApp(n, launch));
                            self.bind(next, &accelerator);
                            next += 1;
                        }
                    }
                }
            }
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
            Some(Internal::ExtCommand(i)) => {
                if self.lock.session.is_none() {
                    if let Some(command) = self.ext.commands.get(i) {
                        let _ = self.requests.send(UiRequest::ExtCommand(command.full_id.clone()));
                    }
                }
            }
            Some(Internal::ShellKey(key)) => {
                let _ = self.requests.send(UiRequest::ShellKey(key));
            }
            Some(Internal::Workspace(key)) => self.workspace_key(key),
            Some(Internal::Overview) => {
                if self.overview_shown() {
                    self.close_overview(true);
                } else {
                    self.open_overview(true);
                }
            }
            Some(Internal::ShiftOverview(up)) => match (up, self.overview_shown(), self.launcher.is_some()) {
                (true, false, false) => self.open_overview(true),
                (true, true, _) => self.open_launcher(true),
                (false, _, true) => self.open_overview(true),
                (false, true, _) => self.close_overview(true),
                _ => {}
            },
            Some(Internal::MessageTray) => self.toggle_notification_list(quick::Opened::Keyboard),
            Some(Internal::FocusNotification) => self.focus_banner(true),
            Some(Internal::DockApp(n, launch)) => self.switch_to_app(n, launch),
            Some(Internal::DockHotKey(n, which)) => self.dock_hot_key(n, which),
            Some(Internal::DockShortcut) => self.dock_flash(),
            Some(Internal::RunDialog) => {
                if !crate::locked_down("disable-command-line") {
                    let _ = self.requests.send(UiRequest::RunDialog);
                }
            }
            Some(Internal::QuickSettings) => {
                self.toggle_power_menu(quick::Opened::Keyboard);
                self.draw_panel();
            }
            Some(Internal::Launcher) => {
                if self.launcher.is_some() {
                    self.close_launcher(true);
                } else {
                    self.open_launcher(true);
                }
            }
            Some(Internal::Wm(crate::windows::WmKey::Cycle { group, backward })) => self.cycle_windows(group, backward),
            Some(Internal::Wm(crate::windows::WmKey::Panels { backward })) => self.switch_panels(backward),
            Some(Internal::Wm(crate::windows::WmKey::ToggleOnAllWorkspaces)) => self.toggle_on_all_workspaces(),
            Some(Internal::Wm(crate::windows::WmKey::Window(action))) => {
                if let Some(list) = self.window_list.as_ref().filter(|l| l.version() >= 4) {
                    list.window_action(0, action);
                }
            }
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
        self.show_workspace_popup(target);
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

    /// Tell the D-Bus side which apps have windows, when that changed.
    fn report_running_apps(&mut self) {
        let focused = self.windows.mru().iter().find(|e| e.id == self.windows.focused()).map(|e| e.app_id.clone());
        let apps = crate::portal_dialog::app_states(self.windows.mru().iter().map(|e| e.app_id.as_str()), focused.as_deref());
        if apps != self.running_apps {
            self.running_apps = apps.clone();
            let _ = self.requests.send(UiRequest::RunningApps(apps));
        }
    }

    /// Follow the active input source in the panel indicator.
    pub(super) fn refresh_source_label(&mut self) {
        let sources = crate::input_sources::current();
        // An IBus engine's input mode symbol (e.g. "あ") stands for it.
        let mode = sources.first().filter(|s| s.0 == "ibus").and_then(|_| input_mode_symbol(&self.ime_props));
        let label = (sources.len() >= 2 || mode.is_some()).then(|| mode.unwrap_or_else(|| crate::input_sources::short_label(&sources[0])));
        if label != self.source_label {
            self.source_label = label;
            self.draw_panel();
        }
    }

    /// Redraw when the minute changes or an OSD expires.
    fn tick(&mut self) {
        // Input sources may change from outside (Settings).
        self.ime_sync();
        self.osk_sync();
        self.quick_discovery_check();
        self.lock_extras_tick();
        self.report_running_apps();
        self.refresh_source_label();
        self.dialog_tick();
        self.osd_tick();
        let minute_changed = self.shown_minute.as_deref() != Some(&clock_text());
        if minute_changed {
            self.draw_panel();
        }
        // The list's arrival times ("5 min") move on with the clock.
        if minute_changed && self.notify.view.is_some() {
            self.draw_notification_list();
        }
        if !self.lock.surfaces.is_empty() && self.shown_lock_clock.as_deref() != Some(&lock_clock_text()) {
            self.draw_lock();
        }
        // The prompt rests again after a while without input.
        let idle = self.lock.last_input.elapsed() >= LOCK_IDLE;
        if self.lock.session.is_some() && self.lock.revealed && idle && self.lock.password.is_empty() && !self.lock.checking && !self.lock.exiting && self.lock.confirming.is_none() {
            self.lock_reveal(false);
        }
    }
}

/// The top panel (on the primary monitor: no output is named).
fn make_panel(compositor: &CompositorState, layer_shell: &LayerShell, qh: &QueueHandle<Ui>) -> LayerSurface {
    let surface = compositor.create_surface(qh);
    let panel = layer_shell.create_layer_surface(qh, surface, Layer::Top, Some("helix-panel"), None);
    panel.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    panel.set_size(0, panel_height());
    panel.set_exclusive_zone(panel_height() as i32);
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
    let data_devices = smithay_client_toolkit::data_device_manager::DataDeviceManagerState::bind(&globals, &qh).ok();
    let primary_selections = smithay_client_toolkit::primary_selection::PrimarySelectionManagerState::bind(&globals, &qh).ok();
    // Only advertised to the shell the compositor launched itself.
    let keybindings = globals.bind::<HelixKeybindingsManagerV1, _, _>(&qh, 1..=2, ()).ok();
    let window_list = globals.bind::<HelixWindowListV1, _, _>(&qh, 1..=10, ()).ok();
    let screenshots = globals.bind::<HelixScreenshotManagerV1, _, _>(&qh, 1..=6, ()).ok();
    let im_manager = globals
        .bind::<wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ())
        .ok();
    let vk_manager = globals
        .bind::<wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .ok();
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
    // Icons and pictures are made on workers; results come back here.
    let (finished_tx, finished_rx) = channel::channel::<offload::Finish>();
    let icons_done = std::sync::Mutex::new(finished_tx.clone());
    let icon_sink: crate::icon_cache::Sink = Arc::new(move |done| {
        let finish: offload::Finish = Box::new(move |ui: &mut Ui| ui.icon_done(done));
        let _ = icons_done.lock().unwrap_or_else(|e| e.into_inner()).send(finish);
    });
    let mut ui = Ui {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        compositor,
        layer_shell,
        qh: qh.clone(),
        scales: scale::Scales::new(
            globals.bind(&qh, 1..=1, ()).ok(),
            globals.bind(&qh, 1..=1, ()).ok(),
        ),
        panel,
        panel_width: 0,
        panel_configured: false,
        launcher: None,
        text,
        bold: Text::load_matching("sans-serif:bold"),
        font_family: None,
        display: helix_draw::screen::display_face(),
        notify: Default::default(),
        calendar: Default::default(),
        osk: Default::default(),
        click_type: Default::default(),
        desktop_icons: Default::default(),
        osd: None,
        osd_views: Vec::new(),
        shown_minute: None,
        shown_lock_clock: None,
        keyboard: None,
        pointer: None,
        relative_state: smithay_client_toolkit::seat::relative_pointer::RelativePointerState::bind(&globals, &qh),
        relative_pointer: None,
        touch: None,
        touch_point: None,
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
        panel_lit: None,
        panel_pressed: None,
        menu_by_key: false,
        window_sources: HashMap::new(),
        window_icons: HashMap::new(),
        icon_cache: crate::icon_cache::IconCache::spawning(crate::workers::Workers::sized("icons"), icon_sink),
        icon_redraw: None,
        offload: offload::Offload::new(finished_tx),
        im_manager,
        vk_manager,
        ime: None,
        screenshots,
        picker: None,
        flashes: Vec::new(),
        loop_handle: None,
        focus_timeout: -1,
        focus_serial: 0,
        banner: None,
        dialog: None,
        power_menu: None,
        break_screen: None,
        panel_editor: None,
        quick: Default::default(),
        quick_list: None,
        quick_power_open: false,
        grid_pending: false,
        bt_discovery: false,
        running_apps: HashMap::new(),
        lock_extras: Default::default(),
        window_menu: None,
        workspace_popup: None,
        dock: dock::DockState::default(),
        overview: None,
        overview_others: Vec::new(),
        overview_shots: Default::default(),
        overview_pool: None,
        backgrounds: Vec::new(),
        background_pool: None,
        desktop_background: Default::default(),
        overview_wall: None,
        overview_wall_wanted: None,
        lock_background: Default::default(),
        lock_background_serial: 0,
        window_geometry: HashMap::new(),
        primary_area: (0, 0, 1920, 1080),
        monitor_labels: Vec::new(),
        power: Default::default(),
        user_switch: false,
        attention: Default::default(),
        sharing: false,
        recording: false,
        status: Default::default(),
        panel_layout: Default::default(),
        panel_keys: Default::default(),
        panel_placed: Vec::new(),
        tray: Vec::new(),
        tray_menu: None,
        ext: Default::default(),
        ext_menu: None,
        ext_dock_menu_at: None,
        panel_drag: None,
        data_devices,
        data_device: None,
        primary_selections,
        primary_device: None,
        clipboard: Default::default(),
        drop_at: None,
        ext_list: Vec::new(),
        extensions_dialog: None,
        panel_hover: Default::default(),
        panel_tooltip: None,
        source_label: None,
        a11y: crate::a11y_menu::Status::default(),
        atspi: Default::default(),
        ime_props: Vec::new(),
        ime_menu: Vec::new(),
        search_serial: 0,
        searched_query: None,
        mods: Default::default(),
        ctrl_held: false,
        lock: LockScreen {
            state: SessionLockState::new(&globals, &qh),
            session: None,
            surfaces: Vec::new(),
            password: String::new(),
            message: None,
            checking: false,
            auth,
            user: crate::auth::current_user(),
            identity: Default::default(),
            results: auth_tx,
            confirming: None,
            blockers: Vec::new(),
            hover: None,
            pressed: None,
            focus: None,
            checked_len: 0,
            checking_since: Instant::now(),
            failed: false,
            shaken: None,
            prompt_at: None,
            revealed: false,
            reveal: Tween::still(0.0),
            last_input: Instant::now(),
            shown: Tween::still(1.0),
            exiting: false,
            motion: Motion::Full,
            animate_until: None,
            frame_timer: None,
        },
        exit: false,
    };

    ui.bind_internal();
    // Who the lock screen shows, ready before it locks.
    let _ = ui.requests.send(UiRequest::LockIdentity(ui.lock.user.clone()));

    let mut event_loop: EventLoop<Ui> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    ui.loop_handle = Some(handle.clone());
    // Work on the UI thread is timed: while it runs, the Wayland socket
    // is not read.
    crate::watchdog::start();
    handle
        .insert_source(WaylandSource::new(conn, event_queue), |_, queue, ui| {
            let _busy = crate::watchdog::scope("Wayland events");
            queue.dispatch_pending(ui)
        })
        .map_err(|e| format!("cannot watch the Wayland connection: {e}"))?;
    handle
        .insert_source(events, |event, _, ui| {
            if let channel::Event::Msg(event) = event {
                let _busy = crate::watchdog::event_scope(&event);
                ui.handle(event);
            }
        })
        .map_err(|e| format!("cannot watch UI events: {e}"))?;
    handle
        .insert_source(finished_rx, |event, _, ui| {
            if let channel::Event::Msg(finish) = event {
                let _busy = crate::watchdog::scope("background work done");
                finish(ui);
            }
        })
        .map_err(|e| format!("cannot watch background work: {e}"))?;
    handle
        .insert_source(auth_rx, |event, _, ui| {
            if let channel::Event::Msg(result) = event {
                let _busy = crate::watchdog::scope("password checked");
                ui.password_checked(result);
            }
        })
        .map_err(|e| format!("cannot watch password checks: {e}"))?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, ui| {
            let _busy = crate::watchdog::scope("clock tick");
            ui.tick();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        })
        .map_err(|e| format!("cannot start UI timer: {e}"))?;

    ui.atspi_start(&handle);

    tracing::info!("shell UI running");
    while !ui.exit {
        if let Err(err) = event_loop.dispatch(None, &mut ui) {
            // The compositor went away (it restarts or stops the shell);
            // exit rather than linger holding org.gnome.Shell.
            tracing::warn!(%err, "lost the compositor connection; shell UI stopping");
            break;
        }
        let _busy = crate::watchdog::scope("accessibility tree");
        ui.atspi_sync();
    }
    Ok(())
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, output: &wl_output::WlOutput) {
        if let Some(view) = self.launcher.as_mut().filter(|v| v.layer.wl_surface() == surface) {
            view.output = Some(output.clone());
        }
    }
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // A monitor plugged in while locked gets its lock surface.
        if let Some(session) = self.lock.session.clone() {
            self.add_lock_surface(&session, output.clone(), qh);
        }
        self.add_background(output);
        self.dock_update();
        self.desktop_icons_sync();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Monitors move: the prompt follows the primary one.
        let origin = self.output_state.info(&output).and_then(|i| i.logical_position).unwrap_or_default();
        if let Some(view) = self.lock.surfaces.iter_mut().find(|v| v.output == output) {
            view.origin = origin;
        }
        if self.lock.session.is_some() {
            self.draw_lock();
        }
        // Spanned pictures depend on every monitor.
        self.draw_backgrounds();
        self.dock_update();
        self.desktop_icons_sync();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // The prompt moves on (to the primary monitor) with what was typed.
        if self.lock.surfaces.iter().any(|v| v.output == output) {
            self.lock.surfaces.retain(|v| v.output != output);
            self.draw_lock();
        }
        self.remove_background(&output);
        self.draw_backgrounds();
        if self.dock.output_is(&output) {
            self.dock_forget_output();
        }
        self.dock_update();
    }
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.osd_closed(layer) {
            return;
        }
        if self.backgrounds.iter().any(|b| &b.layer == layer) {
            self.backgrounds.retain(|b| &b.layer != layer);
            return;
        }
        if self.dock_closed(layer)
            || self.picker_closed(layer)
            || self.overview_closed(layer)
            || self.flash_closed(layer)
            || self.notify_closed(layer)
            || self.window_menu_closed(layer)
            || self.osk_closed(layer)
            || self.click_type_closed(layer)
            || self.desktop_icons_closed(layer)
            || self.workspace_popup_closed(layer)
            || self.panel_tooltip_closed(layer)
        {
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
        } else if self.break_screen.as_ref().is_some_and(|m| &m.layer == layer) {
            self.break_screen = None;
        } else if self.panel_editor.as_ref().is_some_and(|m| &m.layer == layer) {
            self.panel_editor = None;
        } else if self.extensions_dialog.as_ref().is_some_and(|m| &m.layer == layer) {
            self.extensions_dialog = None;
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
        if self.background_configured(layer, configure.new_size)
            || self.dock_configured(layer, configure.new_size)
            || self.picker_configured(layer, configure.new_size)
            || self.overview_configured(layer, configure.new_size)
            || self.flash_configured(layer, configure.new_size)
            || self.notify_configured(layer, configure.new_size)
            || self.window_menu_configured(layer, configure.new_size)
            || self.osk_configured(layer, configure.new_size)
            || self.click_type_configured(layer, configure.new_size)
            || self.desktop_icons_configured(layer, configure.new_size)
            || self.workspace_popup_configured(layer, configure.new_size)
            || self.panel_tooltip_configured(layer, configure.new_size)
            || self.osd_configured(layer, configure.new_size)
        {
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
        } else if let Some(overlay) = self.break_screen.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_break();
        } else if let Some(overlay) = self.panel_editor.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_panel_editor();
        } else if let Some(overlay) = self.extensions_dialog.as_mut().filter(|o| &o.layer == layer) {
            overlay.width = configure.new_size.0;
            overlay.height = configure.new_size.1;
            overlay.configured = true;
            self.draw_extensions_dialog();
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
        if capability == Capability::Keyboard {
            self.start_ime(&seat);
        }
        // The clipboard and drops come through the seat's data device.
        if matches!(capability, Capability::Keyboard | Capability::Pointer) && self.data_device.is_none() {
            self.data_device = self.data_devices.as_ref().map(|m| m.get_data_device(qh, &seat));
        }
        if matches!(capability, Capability::Keyboard | Capability::Pointer) && self.primary_device.is_none() {
            self.primary_device = self.primary_selections.as_ref().map(|m| m.get_selection_device(qh, &seat));
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(err) => tracing::warn!(%err, "no keyboard"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => {
                    self.relative_pointer = self.relative_state.get_relative_pointer(&p, qh).ok();
                    self.pointer = Some(p);
                }
                Err(err) => tracing::warn!(%err, "no pointer"),
            }
        }
        if capability == Capability::Touch && self.touch.is_none() {
            match self.seat_state.get_touch(qh, &seat) {
                Ok(t) => self.touch = Some(t),
                Err(err) => tracing::warn!(%err, "no touch"),
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
        if capability == Capability::Touch {
            if let Some(t) = self.touch.take() {
                t.release();
            }
            self.touch_point = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Ui {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, serial: u32, _: &[u32], _: &[Keysym]) {
        self.clipboard.serial = serial;
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32) {
        // The panel or dock lost the keyboard it had after switch-panels
        // (a click on a window took it): end the keyboard mode and give
        // up the grab, as Escape does.
        if self.panel_focus.is_some() && self.panel.wl_surface() == surface {
            self.leave_panel();
        } else if self.dock_keyboard_surface(surface) {
            self.leave_dock();
        }
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, serial: u32, event: KeyEvent) {
        self.clipboard.serial = serial;
        // Copy, cut and paste in the text field typed into.
        if self.field_clipboard_key(&event) {
            return;
        }
        if self.lock.session.is_some() {
            self.lock_key(&event);
        } else if self.window_menu.is_some() {
            self.window_menu_key(&event);
        } else if self.picker.is_some() {
            self.picker_key(&event);
        } else if self.dialog.is_some() {
            self.dialog_key(&event);
        } else if self.switcher.is_some() {
            self.switcher_key(&event);
        } else if self.banner.as_ref().is_some_and(|b| b.focused) {
            self.banner_key(&event);
        } else if self.overview.is_some() {
            self.overview_key(&event);
        } else if self.power_menu.is_some() {
            self.quick_key(&event);
        } else if self.notify.view.is_some() && self.panel_focus.is_none() {
            self.notification_list_key(&event);
        } else if self.panel_focus.is_some() {
            self.panel_key(&event);
        } else if self.dock_keyboard_view().is_some() {
            self.dock_key(&event);
        } else if self.break_screen.is_some() {
            self.break_key(&event);
        } else if self.panel_editor.is_some() {
            self.panel_editor_key(&event);
        } else if self.extensions_dialog.is_some() {
            self.extensions_dialog_key(&event);
        } else {
            self.launcher_key(&event);
        }
    }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if self.lock.session.is_some() {
            self.lock_key(&event);
        } else if self.notify.view.is_some() {
            if matches!(event.keysym, Keysym::Left | Keysym::Right | Keysym::Up | Keysym::Down | Keysym::Page_Up | Keysym::Page_Down) {
                self.notification_list_key(&event);
            }
        } else if self.power_menu.is_some() {
            // Held arrows keep moving a slider.
            if matches!(event.keysym, Keysym::Left | Keysym::Right | Keysym::Up | Keysym::Down | Keysym::Page_Up | Keysym::Page_Down) {
                self.quick_key(&event);
            }
        } else {
            self.launcher_key(&event);
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: RawModifiers, _: u32) {
        self.ctrl_held = modifiers.ctrl;
        let caps_changed = modifiers.caps_lock != self.mods.caps_lock;
        self.mods = modifiers;
        // The lock screen's Caps Lock warning follows at once.
        if caps_changed && self.lock.session.is_some() {
            self.draw_lock_views(true);
        }
        if self.switcher.is_some() {
            self.switcher_modifiers(&modifiers);
        }
    }
}

impl PointerHandler for Ui {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        self.pointer_events(events);
    }
}

/// A pointer event standing in for a touch.
fn touch_as_pointer(surface: &wl_surface::WlSurface, position: (f64, f64), kind: PointerEventKind) -> PointerEvent {
    PointerEvent { surface: surface.clone(), position, kind }
}

/// Shell surfaces take a tap as a click (helix-desktop-spec
/// `contracts/gsettings/input-settings.md`, "Touchscreens"): the first
/// touch point acts as the left button, others are ignored.
impl TouchHandler for Ui {
    fn down(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, serial: u32, time: u32, surface: wl_surface::WlSurface, id: i32, position: (f64, f64)) {
        if self.touch_point.is_some() {
            return;
        }
        self.touch_point = Some((id, surface.clone(), position));
        self.pointer_events(&[
            touch_as_pointer(&surface, position, PointerEventKind::Enter { serial }),
            touch_as_pointer(&surface, position, PointerEventKind::Motion { time }),
            touch_as_pointer(&surface, position, PointerEventKind::Press { time, button: BTN_LEFT, serial }),
        ]);
    }
    fn up(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, serial: u32, time: u32, id: i32) {
        let Some((_, surface, position)) = self.touch_point.take_if(|(t, _, _)| *t == id) else { return };
        self.pointer_events(&[
            touch_as_pointer(&surface, position, PointerEventKind::Release { time, button: BTN_LEFT, serial }),
            touch_as_pointer(&surface, position, PointerEventKind::Leave { serial }),
        ]);
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, time: u32, id: i32, position: (f64, f64)) {
        let Some((_, surface, last)) = self.touch_point.as_mut().filter(|(t, _, _)| *t == id) else { return };
        *last = position;
        let surface = surface.clone();
        self.pointer_events(&[touch_as_pointer(&surface, position, PointerEventKind::Motion { time })]);
    }
    fn shape(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, _: i32, _: f64, _: f64) {}
    fn orientation(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, _: i32, _: f64) {}
    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch) {
        // A cancelled touch is not a click: drop it without the release.
        self.touch_point = None;
        self.picker_cancel_drag();
    }
}

impl Ui {
    fn pointer_events(&mut self, events: &[PointerEvent]) {
        self.pointer_batch(events);
        self.grid_flush();
    }

    fn pointer_batch(&mut self, events: &[PointerEvent]) {
        for event in events {
            // Mirrored surfaces (right-to-left) hit-test unmirrored.
            let event = &mirror::adjust(event);
            if self.lock.session.is_some() {
                self.lock_pointer(event);
                continue;
            }
            if self.field_middle_click(event) {
                continue;
            }
            if self.banner_pointer(event) || self.overlay_pointer(event) || self.panel_drag_pointer(event) || self.tray_pointer(event) || self.ext_pointer(event) || self.extensions_dialog_axis(event) || self.panel_context(event) || self.grid_pointer(event) || self.dock_pointer(event) || self.picker_pointer(event) || self.overview_pointer(event) || self.notification_list_pointer(event) || self.quick_pointer(event) || self.window_menu_pointer(event) || self.osk_pointer(event) || self.click_type_pointer(event) || self.desktop_icons_pointer(event) || self.panel_pointer(event) {
                continue;
            }
            let PointerEventKind::Press { button: BTN_LEFT, .. } = event.kind else {
                continue;
            };
            let (x, y) = event.position;
            if self.break_screen.as_ref().is_some_and(|m| m.layer.wl_surface() == &event.surface) {
                self.break_click(x, y);
            } else if &event.surface == self.panel.wl_surface() {
                let hit = self.panel_placed.iter().copied().find(|&(_, px, pw)| x >= f64::from(px) && x < f64::from(px + pw));
                if let Some((slot, px, _)) = hit {
                    if slot == crate::panel_layout::Slot::Privacy {
                        // "Sharing" comes first; clicking "Recording" stops it.
                        let recording_x = px + if self.sharing { sharing_w() } else { 0 };
                        if self.recording && x >= f64::from(recording_x) {
                            let _ = self.requests.send(UiRequest::StopRecording);
                        }
                    }
                }
            }
        }
    }

    /// Hover, press and click (on release, over what was pressed) on the
    /// dialogs, quick settings, the switcher, the launcher's results, the
    /// panel editor and the Extensions dialog; true if `event` was for
    /// one of them.
    fn overlay_pointer(&mut self, event: &PointerEvent) -> bool {
        let (x, y) = event.position;
        let kind = &event.kind;
        let pressed_left = matches!(kind, PointerEventKind::Press { button: BTN_LEFT, .. });
        let on = |layer: &LayerSurface| layer.wl_surface() == &event.surface;
        let hit_in = |hits: &[(Rect, OverlayHit)]| hits.iter().find(|(r, _)| r.contains(x, y)).copied();
        if let Some(d) = self.dialog.as_mut().filter(|d| on(&d.layer)) {
            let hit = hit_in(&d.hits).map(|(_, h)| h);
            let out = press::track(&mut d.state.hover, &mut d.state.pressed, kind, hit);
            match out.click {
                Some(hit) => self.dialog_click(hit),
                None if out.redraw => self.draw_dialog(),
                None => {}
            }
            return true;
        }
        if let Some(m) = self.power_menu.as_mut().filter(|m| on(&m.layer)) {
            let found = hit_in(&m.hits);
            if pressed_left && found.is_none_or(|(_, h)| h == OverlayHit::Outside) {
                self.close_quick_settings();
                return true;
            }
            let part = found.map(|(_, h)| h).filter(|h| !matches!(h, OverlayHit::Inside | OverlayHit::Outside));
            let out = press::track(&mut m.state.hover, &mut m.state.pressed, kind, part);
            // A slider follows the pointer from the press on, also when
            // it moves off the bar while held.
            let held_slider = m.state.pressed.filter(|h| quick::is_slider(*h));
            let slider_rect = held_slider.and_then(|s| m.hits.iter().find(|(_, h)| *h == s).map(|(r, _)| *r));
            if let (Some(slider), Some(rect)) = (held_slider, slider_rect) {
                if matches!(kind, PointerEventKind::Motion { .. } | PointerEventKind::Press { .. }) {
                    self.quick_click(slider, x, rect);
                }
            }
            if let Some(hit) = out.click.filter(|h| !quick::is_slider(*h)) {
                let rect = found.map_or_else(Rect::default, |(r, _)| r);
                if !self.quick_click(hit, x, rect) {
                    self.close_quick_settings();
                }
            } else if out.redraw {
                self.draw_power_menu();
            }
            return true;
        }
        if let Some(o) = self.switcher.as_mut().filter(|o| on(&o.layer)) {
            let hit = hit_in(&o.hits).map(|(_, h)| h);
            if pressed_left && hit.is_none() {
                self.finish_switching(false);
                return true;
            }
            let out = press::track(&mut o.state.hover, &mut o.state.pressed, kind, hit);
            if let Some(OverlayHit::Window(id)) = out.click {
                if let Some(pos) = o.state.switcher.entries.iter().position(|e| e.id == id) {
                    o.state.switcher.selected = pos;
                }
                self.finish_switching(true);
            } else if out.redraw {
                self.draw_switcher();
            }
            return true;
        }
        if let Some(view) = self.launcher.as_mut().filter(|v| on(&v.layer)) {
            // The app grid has its own (`grid_pointer`).
            if view.model.query.is_empty() {
                return false;
            }
            let row = view.rows.iter().find(|(r, _)| r.contains(x, y)).map(|(_, i)| *i);
            let row = row.or_else(|| view.clear.filter(|c| c.contains(x, y)).map(|_| CLEAR_HIT));
            if pressed_left && row.is_none() && !view.content.contains(x, y) {
                self.close_launcher(true);
                return true;
            }
            let out = press::track(&mut view.hover, &mut view.pressed, kind, row);
            if out.click == Some(CLEAR_HIT) {
                view.model.set_query("");
                view.hover = None;
                self.search_changed();
                self.draw_launcher();
            } else if let Some(result) = out.click {
                view.model.selected = result;
                self.launch_selected();
            } else if out.redraw {
                self.draw_launcher();
            }
            return true;
        }
        if let Some(o) = self.panel_editor.as_mut().filter(|o| on(&o.layer)) {
            let hit = hit_in(&o.hits).and_then(|(_, h)| if let OverlayHit::PanelEditor(e) = h { Some(e) } else { None });
            let out = press::track(&mut o.state.hover, &mut o.state.pressed, kind, hit);
            if let Some(hit) = out.click {
                self.panel_editor_press(hit);
            } else if out.redraw {
                self.draw_panel_editor();
            }
            // (Drags of panel items go on in the panel editor's own handler.)
            return !matches!(kind, PointerEventKind::Axis { .. });
        }
        if let Some(o) = self.extensions_dialog.as_mut().filter(|o| on(&o.layer)) {
            let hit = hit_in(&o.hits).and_then(|(_, h)| if let OverlayHit::ExtDialog(e) = h { Some(e) } else { None });
            if matches!(kind, PointerEventKind::Axis { .. }) {
                return false;
            }
            let out = press::track(&mut o.state.hover, &mut o.state.pressed, kind, hit);
            if let Some(hit) = out.click {
                self.extensions_dialog_press(hit);
            } else if out.redraw {
                self.draw_extensions_dialog();
            }
            return true;
        }
        false
    }

    /// A click on a dialog's or prompt's part.
    fn dialog_click(&mut self, hit: OverlayHit) {
        match hit {
            OverlayHit::Confirm => self.answer_dialog(Choice::Confirmed),
            OverlayHit::Cancel => self.answer_dialog(Choice::Canceled),
            OverlayHit::Monitor(index) => self.share_monitor(index),
            OverlayHit::Toggle => self.prompt_toggle(),
            OverlayHit::EndInstall => self.end_toggle_install(),
            OverlayHit::Check(i) => self.prompt_check(i),
            OverlayHit::Row(i) => self.prompt_pick(i),
            OverlayHit::Place(i) => self.prompt_place(i),
            OverlayHit::Field(f) => self.prompt_focus(f),
            OverlayHit::ShowPassword(f) => self.prompt_show_password(f),
            _ => {}
        }
        self.draw_dialog();
    }
}

impl smithay_client_toolkit::seat::relative_pointer::RelativePointerHandler for Ui {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: smithay_client_toolkit::seat::relative_pointer::RelativeMotionEvent,
    ) {
        self.dock_relative_motion(event.delta_unaccel, event.utime);
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
            self.add_lock_surface(&session_lock, output, qh);
        }
        self.lock_extras_start();
        // The first frame is the background alone (opaque); the rest fades
        // in over it.
        let enter = self.lock_duration(LOCK_ENTER);
        self.lock.shown = Tween::new(0.0, 1.0, enter);
        self.lock_animate_for(enter);
        tracing::info!(surfaces = self.lock.surfaces.len(), "session locked");
        let _ = self.requests.send(UiRequest::Locked);
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.lock.prompt_at = None;
        // Refused by the compositor (or the lock was taken away).
        tracing::warn!("session lock finished by the compositor");
        self.lock.session = None;
        self.lock.surfaces.clear();
        wipe(&mut self.lock.password);
        self.dock_update();
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
        }
        self.lock.surfaces = views;
        // Every surface: the prompt may have just found its monitor.
        self.draw_lock();
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
    /// An extension's command (index into the extensions' commands).
    ExtCommand(usize),
    Workspace(crate::windows::WorkspaceKey),
    Wm(crate::windows::WmKey),
    /// `toggle-overview`
    Overview,
    /// `toggle-application-view`
    Launcher,
    /// `shift-overview-up` (true) / `-down`
    ShiftOverview(bool),
    /// `toggle-message-tray`
    MessageTray,
    /// `focus-active-notification`
    FocusNotification,
    /// `toggle-quick-settings`
    QuickSettings,
    /// `switch-to-application-N`, or with true
    /// `open-new-window-application-N`
    DockApp(usize, bool),
    /// Dash to Dock's `app-hotkey-N` and its Shift and Ctrl kinds.
    DockHotKey(usize, crate::dock::HotKey),
    /// Dash to Dock's `shortcut`.
    DockShortcut,
    /// `panel-run-dialog`.
    RunDialog,
}

/// Presses this close continue a window cycle.
const CYCLE_CONTINUE: Duration = Duration::from_millis(1500);

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
    /// Window pictures in tiles (`app-icon-mode` not `app-icon-only`), with
    /// the app's icon too (`both`).
    thumbnails: Option<crate::windows::IconMode>,
    /// The entry under the pointer, and the one pressed.
    hover: Option<OverlayHit>,
    pressed: Option<OverlayHit>,
}

/// Paint the launcher's results in sections (open windows, apps, each
/// search provider under a heading with its app's icon and name, "Search
/// for … in <app>" last) in a popover at (bx, list_top), `bw` wide, down
/// to `bottom` at most, scrolled to keep the selection in sight; every
/// row with an icon. Returns the rows' places (with their result
/// indexes) and the list's height.
#[allow(clippy::type_complexity)]
fn paint_results(
    canvas: &mut Canvas,
    (text, bold): (Option<&Text>, Option<&Text>),
    model: &Launcher,
    (row_icons, heading_icons): (&HashMap<usize, Option<helix_draw::Image>>, &HashMap<usize, Option<helix_draw::Image>>),
    (bx, list_top, bw, bottom): (i32, i32, i32, i32),
    (hover, pressed): (Option<usize>, Option<usize>),
) -> (Vec<(Rect, usize)>, i32) {
    let grow = helix_draw::theme::grow;
    let items = model.sections();
    let (row_h, head_h) = (grow(RESULT_H), grow(HEADING_H));
    let heights: Vec<i32> = items.iter().map(|i| if matches!(i, launcher::ListItem::Heading(_)) { head_h } else { row_h }).collect();
    let room = (bottom - list_top - 8).max(row_h + head_h);
    let keep = items.iter().position(|i| *i == launcher::ListItem::Row(model.selected)).unwrap_or(0);
    let start = launcher::list_start(&heights, keep, room);
    let mut shown = Vec::new();
    let mut used = 0;
    for (item, ih) in items.iter().zip(&heights).skip(start) {
        if used + ih > room {
            break;
        }
        used += ih;
        shown.push((*item, *ih));
    }
    // A heading left alone at the end goes.
    if matches!(shown.last(), Some((launcher::ListItem::Heading(_), _))) {
        let (_, ih) = shown.pop().unwrap_or((launcher::ListItem::Row(0), 0));
        used -= ih;
    }
    let list_h = used.max(row_h) + 8;
    theme::popover(canvas, bx, list_top, bw, list_h);
    let mut rows = Vec::new();
    if shown.is_empty() {
        let base = baseline(text, list_top + 4, row_h, size::BODY);
        label(text, canvas, tr("No matching apps"), (bx + 16) as f32, base, size::BODY, DIM_FG, (bw - 32) as f32);
    }
    let mut y = list_top + 4;
    for (item, ih) in shown {
        let result = match item {
            launcher::ListItem::Heading(section) => {
                let (title, icon) = match section {
                    launcher::Section::Windows => (tr("Open Windows").to_owned(), None),
                    launcher::Section::Apps => (tr("Apps").to_owned(), None),
                    launcher::Section::Provider(first) => {
                        let app = model.external.get(first).map(|h| h.app.clone()).unwrap_or_default();
                        let icon = heading_icons.get(&first).cloned().flatten();
                        (app, icon)
                    }
                };
                let mut tx = bx + 16;
                if let Some(icon) = icon {
                    canvas.image(f64::from(tx), f64::from(y + (ih - 16) / 2 + 2), 16.0, 16.0, &icon);
                    tx += 22;
                }
                let face = bold.or(text);
                label(face, canvas, &title, tx as f32, baseline(face, y + 2, ih, size::CAPTION), size::CAPTION, DIM_FG, (bw - 32) as f32);
                y += ih;
                continue;
            }
            launcher::ListItem::Row(result) => result,
        };
        let rect = Rect { x: bx + 4, y, w: bw - 8, h: ih };
        y += ih;
        let hit = model.results[result];
        let look = theme::Look { on: result == model.selected, hovered: hover == Some(result), pressed: pressed == Some(result) };
        theme::row(canvas, rect.x, rect.y, rect.w, rect.h, look);
        let fg = theme::control_text(look);
        let dim = if look.on { fg } else { DIM_FG };
        let (name, _) = model.row_label(hit);
        let more = matches!(hit, launcher::Hit::More(_));
        let more_label;
        let name = if more {
            more_label = trf!("Search for “{query}” in {name}", query = model.query.trim(), name = name);
            more_label.as_str()
        } else {
            name
        };
        let base = baseline(text, rect.y, rect.h, size::BODY);
        // Every row's icon at its start: the app's, the window's (with
        // a small window sign), the result's; a search symbol for
        // "Search in".
        let icon = RESULT_ICON;
        let (ix, iy) = (f64::from(rect.x + 10), f64::from(rect.y + (rect.h - icon) / 2));
        match row_icons.get(&result).cloned().flatten() {
            _ if more => theme::search_symbol(canvas, ix + f64::from(icon) / 2.0, iy + f64::from(icon) / 2.0, 18.0, dim),
            Some(image) => canvas.image(ix, iy, f64::from(icon), f64::from(icon), &image),
            None => theme::control_round(canvas, (ix + 2.0, iy + 2.0, f64::from(icon - 4), f64::from(icon - 4)), theme::RADIUS_ROW, theme::Look::default()),
        }
        if matches!(hit, launcher::Hit::Window(_)) {
            // The open-window sign: a small framed window on the
            // icon's bottom right corner.
            let (ex, ey, e) = (ix + f64::from(icon) - 11.0, iy + f64::from(icon) - 11.0, 13.0);
            canvas.round_rect(ex, ey, e, e, 3.0, if helix_draw::high_contrast() { 0xFF00_0000 } else { theme::POPOVER });
            canvas.round_outline(ex + 1.5, ey + 1.5, e - 3.0, e - 3.0, 1.5, 1.4, FG);
            canvas.rect((ex + 2.5) as i32, (ey + 2.5) as i32, (e - 5.0) as i32, 2, FG);
        }
        let text_x = rect.x + 10 + icon + 12;
        let name_max = (rect.x + rect.w - text_x - 12) as f32;
        label(text, canvas, name, text_x as f32, base, size::BODY, if more && !look.on { DIM_FG } else { fg }, name_max);
        // A search result's description after its name, dimmer.
        if let Some(found) = model.search_hit(hit).filter(|h| !more && !h.description.is_empty()) {
            let nw = text.map_or(0.0, |t| t.width(name, size::BODY)) + 12.0;
            if nw < name_max - 40.0 {
                label(text, canvas, &found.description, text_x as f32 + nw, base, size::CAPTION, dim, name_max - nw);
            }
        }
        rows.push((rect, result));
    }
    (rows, list_h)
}

/// The application switcher's icons: as large as fit `n` of them on a
/// surface `w` wide, from [`APP_ICON_MAX`] down to [`APP_ICON_MIN`]
/// (`None`: not even those fit; the list is shown instead).
fn app_row_icon(w: i32, n: usize) -> Option<i32> {
    let n = n.max(1) as i32;
    let room = w - 2 * APP_ROW_MARGIN - 2 * APP_ROW_PAD;
    let icon = (room / n - APP_ITEM_PAD * 2).min(APP_ICON_MAX);
    (icon >= APP_ICON_MIN).then_some(icon)
}

/// The application switcher's icon sizes (logical px), the room around
/// each icon, inside the box and outside it.
const APP_ICON_MAX: i32 = 96;
const APP_ICON_MIN: i32 = 32;
const APP_ITEM_PAD: i32 = 12;
const APP_ROW_PAD: i32 = 12;
const APP_ROW_MARGIN: i32 = 24;

/// Paint the application switcher on a `w`×`h` surface: a rounded box
/// with a row of `icon`-sized app icons, the selected one in a rounded
/// highlight, small dots under apps with several windows, the selected
/// app's name under the row. Returns each entry's place.
fn paint_app_row(canvas: &mut Canvas, (text, bold): (Option<&Text>, Option<&Text>), (w, h): (i32, i32), switcher: &Switcher, icons: &[Option<helix_draw::Image>], icon: i32, (hover, pressed): (Option<OverlayHit>, Option<OverlayHit>)) -> Vec<(Rect, OverlayHit)> {
    canvas.fill(0x00000000);
    let n = switcher.entries.len() as i32;
    let item = icon + 2 * APP_ITEM_PAD;
    let name_h = helix_draw::theme::grow(40);
    let bw = n * item + 2 * APP_ROW_PAD;
    let bh = item + 2 * APP_ROW_PAD + name_h;
    let (bx, by) = ((w - bw) / 2, (h - bh) / 2);
    theme::popover(canvas, bx, by, bw, bh);
    let mut hits = Vec::new();
    for (i, entry) in switcher.entries.iter().enumerate() {
        let r = Rect { x: bx + APP_ROW_PAD + i as i32 * item, y: by + APP_ROW_PAD, w: item, h: item };
        let hit = OverlayHit::Window(entry.id);
        let selected = i == switcher.selected;
        // The selection: a light rounded square (Hovered); under the
        // pointer a fainter one (Control), pressed Pressed.
        let rect = (f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h));
        let (hovered, held) = (hover == Some(hit), pressed == Some(hit));
        if selected || held {
            theme::control_round(canvas, rect, theme::RADIUS_CARD, theme::Look { hovered: selected, pressed: held, on: false });
            if helix_draw::high_contrast() && selected {
                theme::inner_outline(canvas, rect, theme::RADIUS_CARD, 2.0, 0xFFFF_FFFF);
            }
        } else if hovered {
            theme::control_round(canvas, rect, theme::RADIUS_CARD, theme::Look::default());
        }
        let (ix, iy) = (f64::from(r.x + APP_ITEM_PAD), f64::from(r.y + APP_ITEM_PAD));
        match icons.get(i).and_then(Option::as_ref) {
            Some(image) => canvas.image(ix, iy, f64::from(icon), f64::from(icon), image),
            None => theme::control_round(canvas, (ix + 4.0, iy + 4.0, f64::from(icon - 8), f64::from(icon - 8)), theme::RADIUS_CARD, theme::Look::default()),
        }
        // Several windows: a dot for each (at most four) under the icon.
        if entry.count > 1 {
            let dots = entry.count.min(4) as i32;
            let (gap, d) = (7, 4.0);
            let x0 = f64::from(r.x + r.w / 2) - f64::from((dots - 1) * gap) / 2.0;
            for k in 0..dots {
                canvas.circle(x0 + f64::from(k * gap), f64::from(r.y + r.h - APP_ITEM_PAD / 2), d / 2.0, FG);
            }
        }
        hits.push((r, hit));
    }
    // The selected app's name under the row.
    if let Some(entry) = switcher.entries.get(switcher.selected) {
        let face = bold.or(text);
        let room = (bw - 24) as f32;
        let tw = face.map_or(0.0, |t| t.width(&entry.label, size::BODY)).min(room);
        let top = by + APP_ROW_PAD + item;
        label(face, canvas, &entry.label, bx as f32 + (bw as f32 - tw) / 2.0, baseline(face, top, name_h, size::BODY), size::BODY, FG, room);
    }
    hits
}

/// Window switcher tiles: the picture area and the title below.
const THUMB_W: i32 = 168;
const THUMB_H: i32 = 108;
const THUMB_TITLE_H: i32 = 30;
const THUMB_GAP: i32 = 12;
const THUMB_MAX: usize = 8;

const SWITCHER_ROWS: usize = 10;
/// Icon size in switcher rows (logical pixels).
const ICON_PX: i32 = 24;

/// A window icon image from the compositor: size×size premultiplied
/// ARGB8888 (native order) → premultiplied RGBA.
fn read_icon(data: std::os::fd::OwnedFd, size: u32) -> Option<helix_draw::Image> {
    use std::io::Read;
    let len = (size as usize).checked_mul(size as usize)?.checked_mul(4)?;
    let mut bytes = Vec::with_capacity(len);
    let mut file = std::fs::File::from(data);
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).ok()?;
    file.take(len as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() != len {
        return None;
    }
    for px in bytes.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Some(helix_draw::Image { width: size as usize, height: size as usize, rgba: bytes })
}

/// Power menu entries, offering only what logind allows.
/// `lock` and `log_out`: whether the lockdown settings allow them.
fn power_menu_items(power: crate::logind::Availability, lock: bool, log_out: bool, switch_user: bool) -> Vec<(&'static str, PowerAction)> {
    let mut items = Vec::new();
    if lock {
        items.push((tr("Lock Screen"), PowerAction::Lock));
    }
    if switch_user {
        items.push((tr("Switch User…"), PowerAction::SwitchUser));
    }
    if power.suspend {
        items.push((tr("Suspend"), PowerAction::Suspend));
    }
    if log_out {
        items.push((tr("Log Out…"), PowerAction::LogOut));
    }
    if power.restart {
        items.push((tr("Restart…"), PowerAction::Restart));
    }
    if power.power_off {
        items.push((tr("Power Off…"), PowerAction::PowerOff));
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
            helix_window_list_v1::Event::Window { id, app_id, title } => {
                ui.windows.update(id, app_id, title);
                ui.request_shots();
                ui.draw_overview();
                ui.dock_update();
            }
            helix_window_list_v1::Event::Icon { id, name, size, data } => {
                let image = (size > 0).then(|| read_icon(data, size)).flatten();
                if name.is_empty() && image.is_none() {
                    ui.window_icons.remove(&id);
                } else {
                    ui.window_icons.insert(id, (name, image));
                }
                ui.draw_switcher();
            }
            helix_window_list_v1::Event::Closed { id } => {
                ui.window_icons.remove(&id);
                ui.window_geometry.remove(&id);
                if let Some(overview) = ui.overview.as_mut() {
                    if overview.state.selected == Some(id) {
                        overview.state.selected = None;
                    }
                }
                ui.draw_overview();
                ui.close_not_responding(id);
                if ui.attention.contains(&id) {
                    ui.window_attention(id, false);
                }
                ui.windows.close(id);
                ui.window_sources.remove(&id);
                ui.dock_update();
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
            helix_window_list_v1::Event::Focused { id } => {
                ui.windows.focus(id);
                ui.dock_update();
                if id != 0 && crate::input_sources::per_window() {
                    let active = crate::input_sources::active();
                    let (switch, remember) = crate::input_sources::on_focus(ui.window_sources.get(&id), active.as_ref());
                    if let Some(source) = switch {
                        crate::input_sources::activate(&source);
                        ui.ime_sync();
                    }
                    if let Some(source) = remember {
                        ui.window_sources.insert(id, source);
                    }
                }
            }
            helix_window_list_v1::Event::Menu { id, x, y, state: WEnum::Value(state) } => ui.show_window_menu(id, x, y, state),
            helix_window_list_v1::Event::NotResponding { id, responding } => {
                if responding != 0 {
                    ui.close_not_responding(id);
                } else if ui.lock.session.is_none() && ui.dialog.is_none() {
                    let title = ui.windows.mru().iter().find(|e| e.id == id).map_or_else(|| tr("The app").to_owned(), |e| e.label().to_owned());
                    ui.show_dialog(DialogState::new(DialogKind::NotResponding { id, title }, 0));
                }
            }
            helix_window_list_v1::Event::Attention { id, wanting } => ui.window_attention(id, wanting != 0),
            helix_window_list_v1::Event::ShortcutsInhibit { id } => ui.shortcuts_inhibit(id),
            helix_window_list_v1::Event::Workspaces { count, active } => {
                let switched = active != ui.windows.active;
                ui.windows.set_workspaces(count, active);
                ui.draw_panel();
                ui.dock_update();
                ui.workspaces_changed();
                if switched {
                    ui.refresh_shots();
                } else {
                    ui.draw_overview();
                }
            }
            helix_window_list_v1::Event::Workspace { id, workspace } => {
                ui.windows.set_workspace(id, workspace);
                ui.draw_overview();
                ui.dock_update();
            }
            helix_window_list_v1::Event::Geometry { id, x, y, width, height, minimized } => {
                ui.window_geometry.insert(id, (x, y, width, height, minimized != 0));
                ui.draw_overview();
                ui.dock_update();
            }
            helix_window_list_v1::Event::Primary { x, y, width, height } => {
                ui.primary_area = (x, y, width, height);
                ui.draw_overview();
                ui.dock_update();
                ui.desktop_icons_sync();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn focused_field_has_ring_and_caret() {
        // B, G, R, A at logical (x, y) of a 60×40 canvas at scale 1.
        let px = |data: &[u8], x: usize, y: usize| -> [u8; 4] { data[(y * 60 + x) * 4..][..4].try_into().unwrap() };
        let [r, g, b, a] = helix_draw::rgba(super::accent()).map(|c| (c * 255.0).round() as u8);
        let accent = [b, g, r, a];
        let [r, g, b, a] = helix_draw::rgba(super::FG).map(|c| (c * 255.0).round() as u8);
        let fg = [b, g, r, a];
        let r = super::Rect { x: 10, y: 10, w: 40, h: 20 };
        let mut data = vec![0u8; 60 * 40 * 4];
        let mut canvas = super::Canvas::scaled(&mut data, 60, 40, 1.0);
        super::text_field(&mut canvas, r, true);
        super::caret(&mut canvas, r, 20, size::LABEL);
        assert_eq!(px(&data, 25, 8), accent, "ring above");
        assert_eq!(px(&data, 9, 20), accent, "ring left");
        assert_eq!(px(&data, 20, 20), fg, "caret");
        let mut data = vec![0u8; 60 * 40 * 4];
        let mut canvas = super::Canvas::scaled(&mut data, 60, 40, 1.0);
        super::text_field(&mut canvas, r, false);
        assert_eq!(px(&data, 25, 9), [0, 0, 0, 0], "unfocused: no ring");
        assert_ne!(px(&data, 25, 10), px(&data, 25, 11), "unfocused: a faint outline at the edge");
        assert_eq!(px(&data, 25, 12), px(&data, 25, 11), "outline is 1px");
        assert_eq!(px(&data, 10, 10), [0, 0, 0, 0], "rounded corners");
    }

    use super::*;
    use crate::logind::Availability;

    #[test]
    fn power_menu_entries_follow_logind() {
        let names = |power, lock, log_out| power_menu_items(power, lock, log_out, false).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(Availability::default(), true, true), ["Lock Screen", "Log Out…"]);
        let all = Availability { suspend: true, restart: true, power_off: true };
        assert_eq!(names(all, true, true), ["Lock Screen", "Suspend", "Log Out…", "Restart…", "Power Off…"]);
        assert_eq!(names(all, false, false), ["Suspend", "Restart…", "Power Off…"]);
        let with_switch: Vec<_> = power_menu_items(Availability::default(), true, true, true).into_iter().map(|(n, _)| n).collect();
        assert_eq!(with_switch, ["Lock Screen", "Switch User…", "Log Out…"]);
    }

    fn end(blockers: usize) -> DialogKind {
        let blockers = (0..blockers).map(|i| (format!("App {i}"), String::new())).collect();
        DialogKind::End(EndDialog { kind: SHUTDOWN, seconds: 60, blockers, install: None, upgrade: None })
    }

    /// The application switcher shows as icons as long as they fit (12
    /// and more on a 1280 px screen), shrinking; each entry is placed on
    /// the surface, in order.
    #[test]
    fn app_switcher_row() {
        assert_eq!(app_row_icon(1920, 5), Some(APP_ICON_MAX));
        let twelve = app_row_icon(1280, 12).expect("12 apps fit at 1280");
        assert!(twelve >= 64, "{twelve}");
        assert!(app_row_icon(1280, 20).is_some_and(|i| i >= APP_ICON_MIN));
        assert_eq!(app_row_icon(1280, 40), None, "the list instead");
        let entries: Vec<SwitchEntry> = (0..14).map(|i| SwitchEntry { id: 100 + i, label: format!("App {i}"), detail: String::new(), count: 1 + i as usize % 3 }).collect();
        let switcher = Switcher { entries, selected: 3 };
        let icon = app_row_icon(1280, 14).unwrap();
        let mut data = vec![0u8; 1280 * 720 * 4];
        let mut canvas = Canvas::scaled(&mut data, 1280, 720, 1.0);
        let hits = paint_app_row(&mut canvas, (None, None), (1280, 720), &switcher, &[], icon, (None, None));
        assert_eq!(hits.len(), 14);
        assert!(hits.windows(2).all(|p| p[0].0.x + p[0].0.w <= p[1].0.x), "left to right");
        assert!(hits.iter().all(|(r, _)| r.x >= 0 && r.x + r.w <= 1280), "on the screen");
        assert_eq!(hits[3].1, OverlayHit::Window(103));
    }

    /// The result row under the pointer is lit apart from the keyboard's
    /// selection (which keeps the accent fill), and only while the pointer
    /// is on it.
    #[test]
    fn result_hover() {
        let app = |name: &str| launcher::App { id: format!("{name}.desktop"), name: name.into(), search: String::new(), categories: Vec::new() };
        let mut model = Launcher::new(vec![app("Notes"), app("Notebook"), app("Note Taker")]);
        model.type_text("note");
        let empty = HashMap::new();
        let paint = |hover: Option<usize>| {
            let mut data = vec![0u8; 800 * 600 * 4];
            let mut canvas = Canvas::scaled(&mut data, 800, 600, 1.0);
            let (rows, _) = paint_results(&mut canvas, (None, None), &model, (&empty, &empty), (100, 20, 600, 580), (hover, None));
            (data, rows)
        };
        let (rest, rows) = paint(None);
        let (lit, _) = paint(Some(rows[2].1));
        let at = |data: &[u8], r: Rect| -> [u8; 4] { data[(((r.y + r.h / 2) * 800 + r.x + r.w - 20) * 4) as usize..][..4].try_into().unwrap() };
        assert_ne!(at(&lit, rows[2].0), at(&rest, rows[2].0), "the hovered row is lit");
        assert_eq!(at(&lit, rows[0].0), at(&rest, rows[0].0), "the selection keeps its look");
        assert_ne!(at(&lit, rows[2].0), at(&lit, rows[0].0), "and differs from the hover");
        assert_eq!(at(&lit, rows[1].0), at(&rest, rows[1].0), "other rows stay");
    }

    #[test]
    fn dialog_defaults() {
        use theme::Kind as ButtonKind;
        let not_responding = DialogKind::NotResponding { id: 1, title: "Editor".into() };
        let remote = DialogKind::RemoteControl { app: "Remote".into(), devices: vec!["keyboard".into()], reply: None };
        let shortcuts = DialogKind::ShortcutsInhibit { id: 1, app_id: "vm".into(), app: "VM".into() };
        let extension = DialogKind::ApproveExtension { name: "Caffeine".into(), lines: Vec::new(), reply: None };
        // Enter never force-quits or grants a permission.
        for kind in [&not_responding, &remote, &shortcuts, &extension] {
            assert_eq!(kind.default_choice(), Choice::Canceled);
        }
        assert_eq!(not_responding.confirm_kind(), ButtonKind::Destructive);
        assert_eq!(remote.confirm_kind(), ButtonKind::Normal);
        // Ending the session and keeping display settings: Enter confirms.
        for kind in [end(0), end(1), DialogKind::DisplayChange] {
            assert_eq!(kind.default_choice(), Choice::Confirmed);
        }
        assert_eq!(end(0).confirm_kind(), ButtonKind::Suggested);
        assert_eq!(end(2).confirm_kind(), ButtonKind::Destructive, "Power Off Anyway");
        // Enter answers with the focused button; Tab moves to the other.
        let mut state = DialogState::new(not_responding, 0);
        assert_eq!(dialog_key_action(Keysym::Return, state.focused()), Some(DialogKey::Answer(Choice::Canceled)));
        assert_eq!(dialog_key_action(Keysym::Tab, state.focused()), Some(DialogKey::Focus(Choice::Confirmed)));
        state.focus = Some(Choice::Confirmed);
        assert_eq!(dialog_key_action(Keysym::KP_Enter, state.focused()), Some(DialogKey::Answer(Choice::Confirmed)), "an explicit choice");
        assert_eq!(dialog_key_action(Keysym::Escape, state.focused()), Some(DialogKey::Answer(Choice::Canceled)));
        assert_eq!(dialog_key_action(Keysym::Left, state.focused()), Some(DialogKey::Focus(Choice::Canceled)));
    }

    /// Every text color the shell paints, on every background it is
    /// painted on, reaches WCAG AA (4.5:1).
    #[test]
    fn text_contrast() {
        let opaque = |c: u32| c | 0xFF00_0000;
        let pairs = [
            (FG, theme::POPOVER, "text on popovers and dialogs"),
            (FG, opaque(PANEL_BG), "the panel"),
            (FG, theme::CONTROL, "text on controls"),
            (FG, theme::CONTROL_HOVER, "the selected menu row"),
            (DIM_FG, theme::POPOVER, "secondary text: notification bodies, percentages, weekdays, dialog messages"),
            (DIM_FG, opaque(PANEL_BG), "secondary text on the panel"),
            (DIM_FG, theme::WELL, "\"No Networks\""),
            (ERROR_TEXT, theme::POPOVER, "a prompt's error"),
            (ERROR_TEXT, opaque(PANEL_BG), "\"Sharing\", \"Recording\""),
        ];
        for (fg, bg, what) in pairs {
            let ratio = helix_draw::backdrop::contrast(fg, bg);
            assert!(ratio >= 4.5, "{what}: {fg:08x} on {bg:08x} is {ratio:.2}:1");
        }
        // The accent's, for every accent.
        for accent in helix_draw::accent::Accent::ALL {
            let p = accent.palette();
            let name = accent.name();
            for (fg, bg, what) in [
                (p.on_fill, p.fill, "switches on, selected rows, buttons"),
                (p.on_fill, p.fill_hover, "a hovered default button"),
                (p.text, theme::POPOVER, "links in notifications"),
                (p.text, theme::WELL, "\"Connected\""),
                (p.text, opaque(PANEL_BG), "accent words on the panel"),
            ] {
                let ratio = helix_draw::backdrop::contrast(fg, bg);
                assert!(ratio >= 4.5, "{name}: {what}: {fg:08x} on {bg:08x} is {ratio:.2}:1");
            }
            for (fg, bg, what) in [(p.color, theme::POPOVER, "focus rings on popovers"), (p.color, opaque(PANEL_BG), "panel indicators")] {
                let ratio = helix_draw::backdrop::contrast(fg, bg);
                assert!(ratio >= 3.0, "{name}: {what}: {fg:08x} on {bg:08x} is {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn early_keys_ignored() {
        let now = Instant::now();
        assert!(!dialog_takes_key(None, now), "not drawn yet");
        assert!(!dialog_takes_key(Some(now), now + Duration::from_millis(100)));
        assert!(!dialog_takes_key(Some(now), now + Duration::from_millis(299)));
        assert!(dialog_takes_key(Some(now), now + DIALOG_KEY_DELAY));
        assert!(dialog_takes_key(Some(now), now + Duration::from_secs(2)));
    }
}
