//! Drawing X11 windows' title bars and handling clicks on them
//! (helix-desktop-spec `contracts/wayland/README.md`, "Window frames"),
//! and the scene every output shows: shell layers, windows with their
//! frames, in stacking order.

use std::cell::RefCell;

use helix_comp::frames::{self, Button, FrameConfig, Part, TitleAction, BUTTON, TITLE_H};
use helix_draw::{physical, Canvas, Style, Text};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Space, Window};
use smithay::input::pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale, Transform, SERIAL_COUNTER};
use smithay::wayland::shell::wlr_layer::Layer;

use super::grabs::{Edges, MoveGrab, ResizeGrab, ResizeTracking};
use super::wm::Takeover;
use super::Helix;

smithay::backend::renderer::element::render_elements! {
    pub SceneElement<=GlesRenderer>;
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Frame = MemoryRenderBufferRenderElement<GlesRenderer>,
    Preview = SolidColorRenderElement,
}

/// Whether Helix draws a title bar for `window`: managed X11 windows that
/// do not ask for none and are not fullscreen.
pub fn framed(window: &Window) -> bool {
    window
        .x11_surface()
        // Smithay's `is_decorated`: the Motif hints say the client draws its own.
        .is_some_and(|x| !x.is_override_redirect() && !x.is_decorated() && !x.is_fullscreen())
}

/// How far a window's frame reaches above its own area.
pub fn frame_top(window: &Window) -> i32 {
    if framed(window) {
        TITLE_H
    } else {
        0
    }
}

/// What a drawn title bar shows; it is drawn again when this changes.
#[derive(Debug, Clone, PartialEq)]
struct Look {
    title: String,
    width: i32,
    scale: u64,
    focused: bool,
    hover: Option<Button>,
    pressed: Option<Button>,
    maximized: bool,
    generation: u64,
}

/// Title bar state: settings, drawn title bars, and the pointer on them.
#[derive(Default)]
pub struct Frames {
    pub config: FrameConfig,
    text: Option<Text>,
    /// Bumped when the settings change, to draw everything again.
    generation: u64,
    cache: RefCell<Vec<(Window, Look, MemoryRenderBuffer)>>,
    /// The button under the pointer.
    pub hover: Option<(Window, Button)>,
    /// A button pressed with the left button, acted on at release.
    pub pressed: Option<(Window, Button)>,
    /// The last left click on a title (window, time), for double clicks.
    last_click: Option<(Window, u32)>,
    /// The edge-tiling preview (where a dragged window would go).
    preview: Option<(Rectangle<i32, Logical>, SolidColorBuffer)>,
    /// Visual bells: area, start, and the white drawn over it.
    flashes: Vec<(Rectangle<i32, Logical>, std::time::Instant, SolidColorBuffer)>,
    /// Locate pointer: where, and since when.
    locating: Option<(Point<f64, Logical>, std::time::Instant)>,
}

/// How long a visual bell flash lasts.
const FLASH: std::time::Duration = std::time::Duration::from_millis(250);
/// How long the locate-pointer rings show.
const LOCATE: std::time::Duration = std::time::Duration::from_millis(500);
/// The rings' largest radius (logical pixels).
const LOCATE_R: f64 = 64.0;

struct Colors {
    background: u32,
    title: u32,
    dim: u32,
    hover: u32,
    pressed: u32,
    line: u32,
}

fn colors(dark: bool) -> Colors {
    if dark {
        Colors { background: 0xFF2E2E32, title: 0xFFFFFFFF, dim: 0xFF9A9AA0, hover: 0x26FFFFFF, pressed: 0x40FFFFFF, line: 0xFF1C1C20 }
    } else {
        Colors { background: 0xFFEBEBED, title: 0xFF2E2E33, dim: 0xFF8A8A90, hover: 0x1F000000, pressed: 0x38000000, line: 0xFFD0D0D4 }
    }
}

impl Frames {
    /// New settings: load the title font and draw every title bar again.
    pub fn configure(&mut self, config: FrameConfig) {
        if self.text.is_some() && config == self.config {
            return;
        }
        if self.text.is_none() || config.family != self.config.family {
            self.text = Text::load_matching(&format!("{}:bold", config.family));
        }
        tracing::info!(family = config.family, dark = config.dark, "window frame settings");
        self.config = config;
        self.generation += 1;
        self.cache.borrow_mut().clear();
    }

    /// Show (or, with `None`, hide) the edge-tiling preview.
    pub fn set_preview(&mut self, area: Option<Rectangle<i32, Logical>>, color: [f32; 4]) {
        match (area, self.preview.as_mut()) {
            (None, _) => self.preview = None,
            (Some(area), Some((shown, buffer))) => {
                *shown = area;
                buffer.update(area.size, color);
            }
            (Some(area), None) => self.preview = Some((area, SolidColorBuffer::new(area.size, color))),
        }
    }

    /// Flash `area` white (visual bell).
    pub fn flash(&mut self, area: Rectangle<i32, Logical>) {
        self.flashes.push((area, std::time::Instant::now(), SolidColorBuffer::new(area.size, [0.6, 0.6, 0.6, 0.6])));
    }

    /// Show shrinking rings around `pos`.
    pub fn locate(&mut self, pos: Point<f64, Logical>) {
        self.locating = Some((pos, std::time::Instant::now()));
    }

    /// Advance animations to `now`; true while one runs.
    pub fn animate(&mut self, now: std::time::Instant) -> bool {
        self.flashes.retain(|(_, start, _)| now.duration_since(*start) < FLASH);
        for (area, start, buffer) in &mut self.flashes {
            let a = 0.6 * (1.0 - now.duration_since(*start).as_secs_f32() / FLASH.as_secs_f32());
            buffer.update(area.size, [a, a, a, a]);
        }
        if self.locating.as_ref().is_some_and(|(_, start)| now.duration_since(*start) >= LOCATE) {
            self.locating = None;
        }
        !self.flashes.is_empty() || self.locating.is_some()
    }

    /// The locate rings at this moment, drawn at `scale`.
    fn locate_element(&self, renderer: &mut GlesRenderer, origin: Point<i32, Logical>, scale: f64) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let (pos, start) = self.locating.as_ref()?;
        let t = (start.elapsed().as_secs_f64() / LOCATE.as_secs_f64()).min(1.0);
        let size = (2.0 * LOCATE_R) as usize;
        let (pw, ph) = (physical(size, scale), physical(size, scale));
        let mut data = vec![0u8; pw * ph * 4];
        let mut canvas = Canvas::scaled(&mut data, size, size, scale);
        let c = LOCATE_R;
        // Two rings closing in on the pointer.
        for offset in [0.0, 0.35] {
            let k = (t + offset).fract();
            let r = LOCATE_R * (1.0 - k) + 4.0;
            let alpha = ((1.0 - k) * 200.0) as u32;
            let color = (alpha << 24) | 0x003584E4;
            let steps = 48;
            for i in 0..steps {
                let a0 = i as f64 / steps as f64 * std::f64::consts::TAU;
                let a1 = (i + 1) as f64 / steps as f64 * std::f64::consts::TAU;
                canvas.line((c + r * a0.cos(), c + r * a0.sin()), (c + r * a1.cos(), c + r * a1.sin()), 3.0, color);
            }
        }
        let buffer = MemoryRenderBuffer::from_slice(&data, Fourcc::Argb8888, (pw as i32, ph as i32), 1, Transform::Normal, None);
        let at = Point::<f64, Logical>::from((pos.x - LOCATE_R, pos.y - LOCATE_R)) - origin.to_f64();
        MemoryRenderBufferRenderElement::from_buffer(renderer, at.to_physical(scale), &buffer, None, None, Some((size as i32, size as i32).into()), Kind::Unspecified).ok()
    }

    /// Forget a window that went away.
    pub fn forget(&mut self, window: &Window) {
        self.cache.borrow_mut().retain(|(w, _, _)| w != window);
        if self.hover.as_ref().is_some_and(|(w, _)| w == window) {
            self.hover = None;
        }
        if self.pressed.as_ref().is_some_and(|(w, _)| w == window) {
            self.pressed = None;
        }
    }

    fn draw(&self, look: &Look, scale: f64) -> MemoryRenderBuffer {
        let (w, h) = (look.width.max(1) as usize, TITLE_H as usize);
        let (pw, ph) = (physical(w, scale), physical(h, scale));
        let mut data = vec![0u8; pw * ph * 4];
        let mut canvas = Canvas::scaled(&mut data, w, h, scale);
        let c = colors(self.config.dark);
        canvas.fill(c.background);
        canvas.rect(0, TITLE_H - 1, look.width, 1, c.line);
        let fg = if look.focused { c.title } else { c.dim };
        let (buttons, (start, end)) = frames::layout(&self.config, look.width);
        for (button, (x, y, bw, bh)) in &buttons {
            let (cx, cy) = (f64::from(x + bw / 2), f64::from(y + bh / 2));
            if look.pressed == Some(*button) {
                canvas.circle(cx, cy, f64::from(BUTTON) / 2.0, c.pressed);
            } else if look.hover == Some(*button) {
                canvas.circle(cx, cy, f64::from(BUTTON) / 2.0, c.hover);
            }
            let r = 4.5;
            match button {
                Button::Close => {
                    canvas.line((cx - r, cy - r), (cx + r, cy + r), 1.5, fg);
                    canvas.line((cx - r, cy + r), (cx + r, cy - r), 1.5, fg);
                }
                Button::Minimize => canvas.line((cx - r, cy + r - 0.5), (cx + r, cy + r - 0.5), 1.5, fg),
                Button::Maximize => {
                    // A square; two overlapping ones when maximized.
                    let square = |canvas: &mut Canvas, x0: f64, y0: f64, s: f64| {
                        for (a, b) in [((x0, y0), (x0 + s, y0)), ((x0 + s, y0), (x0 + s, y0 + s)), ((x0 + s, y0 + s), (x0, y0 + s)), ((x0, y0 + s), (x0, y0))] {
                            canvas.line(a, b, 1.5, fg);
                        }
                    };
                    if look.maximized {
                        square(&mut canvas, cx - r + 2.0, cy - r, 2.0 * r - 2.0);
                        square(&mut canvas, cx - r, cy - r + 2.0, 2.0 * r - 2.0);
                    } else {
                        square(&mut canvas, cx - r, cy - r, 2.0 * r);
                    }
                }
            }
        }
        if let Some(text) = &self.text {
            let px = self.config.px;
            let room = (end - start - 16).max(0) as f32;
            let shown = text.width(&look.title, px).min(room);
            // Centered on the whole bar when that fits, else in the room left.
            let centered = (look.width as f32 - shown) / 2.0;
            let x = centered.clamp(start as f32 + 8.0, (end as f32 - 8.0 - shown).max(start as f32 + 8.0));
            let base = (TITLE_H as f32 + text.ascent(px) * 0.75) / 2.0;
            text.draw(&mut canvas, &look.title, x, base, Style { px, color: fg, max_width: room });
        }
        MemoryRenderBuffer::from_slice(&data, Fourcc::Argb8888, (pw as i32, ph as i32), 1, Transform::Normal, None)
    }

    /// The title bar of `window` (its own area `client`) at `scale`.
    fn element(
        &self,
        renderer: &mut GlesRenderer,
        window: &Window,
        client: Rectangle<i32, Logical>,
        origin: Point<i32, Logical>,
        scale: f64,
        focused: bool,
    ) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let x11 = window.x11_surface()?;
        let state = |slot: &Option<(Window, Button)>| slot.as_ref().filter(|(w, _)| w == window).map(|(_, b)| *b);
        let look = Look {
            title: x11.title(),
            width: client.size.w,
            scale: scale.to_bits(),
            // Activated: what Helix marks the focused window with (keyboard
            // focus waits for the X11 window's surface).
            focused: focused || x11.is_activated(),
            hover: state(&self.hover),
            pressed: state(&self.pressed),
            maximized: x11.is_maximized(),
            generation: self.generation,
        };
        let mut cache = self.cache.borrow_mut();
        let buffer = match cache.iter().position(|(w, _, _)| w == window) {
            Some(i) if cache[i].1 == look => cache[i].2.clone(),
            found => {
                let buffer = self.draw(&look, scale);
                if let Some(i) = found {
                    cache.remove(i);
                }
                cache.push((window.clone(), look, buffer.clone()));
                buffer
            }
        };
        let at = Point::<i32, Logical>::from((client.loc.x, client.loc.y - TITLE_H)) - origin;
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            at.to_physical_precise_round::<f64, i32>(scale).to_f64(),
            &buffer,
            None,
            None,
            Some((client.size.w, TITLE_H).into()),
            Kind::Unspecified,
        )
        .map_err(|err| tracing::warn!(?err, "cannot draw a title bar"))
        .ok()
    }
}

/// The window's own area in the space.
pub fn client_rect(space: &Space<Window>, window: &Window) -> Option<Rectangle<i32, Logical>> {
    Some(Rectangle::new(space.element_location(window)?, window.geometry().size))
}

/// What `output` shows, front to back: shell layers above windows, windows
/// (each with its title bar), shell layers below.
pub fn scene(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    frames: &Frames,
    output: &Output,
    focused: Option<&Window>,
) -> Vec<SceneElement> {
    let Some(output_geo) = space.output_geometry(output) else { return Vec::new() };
    let scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();
    let layers = layer_map_for_output(output);
    let (lower, upper): (Vec<_>, Vec<_>) =
        layers.layers().rev().partition(|s| matches!(s.layer(), Layer::Background | Layer::Bottom));
    let push_layers = |elements: &mut Vec<SceneElement>, renderer: &mut GlesRenderer, list: &[&smithay::desktop::LayerSurface]| {
        for surface in list {
            let Some(geo) = layers.layer_geometry(surface) else { continue };
            elements.extend(
                AsRenderElements::<GlesRenderer>::render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                    *surface,
                    renderer,
                    geo.loc.to_physical_precise_round(scale),
                    Scale::from(scale),
                    1.0,
                )
                .into_iter()
                .map(SceneElement::Surface),
            );
        }
    };
    if let Some(rings) = frames.locate_element(renderer, output_geo.loc, scale) {
        elements.push(SceneElement::Frame(rings));
    }
    for (area, _, buffer) in frames.flashes.iter().filter(|(a, _, _)| a.overlaps(output_geo)) {
        let at = (area.loc - output_geo.loc).to_physical_precise_round(scale);
        elements.push(SceneElement::Preview(SolidColorRenderElement::from_buffer(buffer, at, scale, 1.0, Kind::Unspecified)));
    }
    push_layers(&mut elements, renderer, &upper);
    if let Some((area, buffer)) = frames.preview.as_ref().filter(|(a, _)| a.overlaps(output_geo)) {
        let at = (area.loc - output_geo.loc).to_physical_precise_round(scale);
        elements.push(SceneElement::Preview(SolidColorRenderElement::from_buffer(buffer, at, scale, 1.0, Kind::Unspecified)));
    }
    for window in space.elements().rev() {
        let Some(location) = space.element_location(window) else { continue };
        let client = Rectangle::new(location, window.geometry().size);
        let top = frame_top(window);
        let bbox = space.element_bbox(window).unwrap_or(client);
        let reach = Rectangle::new((bbox.loc.x, bbox.loc.y - top).into(), (bbox.size.w, bbox.size.h + top).into());
        if !output_geo.overlaps(reach) {
            continue;
        }
        let at = (location - window.geometry().loc - output_geo.loc).to_physical_precise_round(scale);
        elements.extend(
            window
                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(renderer, at, Scale::from(scale), 1.0)
                .into_iter()
                .map(SceneElement::Surface),
        );
        if top > 0 {
            if let Some(frame) = frames.element(renderer, window, client, output_geo.loc, scale, focused == Some(window)) {
                elements.push(SceneElement::Frame(frame));
            }
        }
    }
    push_layers(&mut elements, renderer, &lower);
    elements
}

/// The title bar of `window` alone, for window screenshots with the frame.
pub fn frame_element(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    frames: &Frames,
    window: &Window,
    origin: Point<i32, Logical>,
    scale: f64,
    focused: bool,
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    if !framed(window) {
        return None;
    }
    let client = client_rect(space, window)?;
    frames.element(renderer, window, client, origin, scale, focused)
}

fn resize_cursor(edges: u32) -> CursorIcon {
    match edges {
        1 => CursorIcon::NResize,
        2 => CursorIcon::SResize,
        4 => CursorIcon::WResize,
        8 => CursorIcon::EResize,
        5 => CursorIcon::NwResize,
        9 => CursorIcon::NeResize,
        6 => CursorIcon::SwResize,
        10 => CursorIcon::SeResize,
        _ => CursorIcon::Default,
    }
}

impl Helix {
    pub fn apply_frame_settings(&mut self) {
        self.frames.configure(frames::read());
    }

    /// The window with keyboard focus.
    pub fn focused_window(&self) -> Option<Window> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        self.window_for_focus(&focus)
    }

    /// The frame part under `pos`, if a frame is the topmost thing there.
    pub fn frame_under(&self, pos: Point<f64, Logical>) -> Option<(Window, Part)> {
        if self.lock.locked || self.over_panel(pos) {
            return None;
        }
        for window in self.space.elements().rev() {
            let Some(client) = client_rect(&self.space, window) else { continue };
            let bbox = self.space.element_bbox(window).unwrap_or(client);
            if client.to_f64().contains(pos) || (!framed(window) && bbox.to_f64().contains(pos)) {
                return None;
            }
            if framed(window) {
                let rect = (client.loc.x, client.loc.y, client.size.w, client.size.h);
                if let Some(part) = frames::hit(&self.frames.config, rect, (pos.x, pos.y)) {
                    return Some((window.clone(), part));
                }
            }
        }
        None
    }

    /// The pointer moved to `pos`, over `part` of a frame (or not):
    /// highlight buttons and show the matching cursor. True when on a frame.
    pub fn frame_hover(&mut self, under: Option<&(Window, Part)>) -> bool {
        let hover = under.and_then(|(w, part)| match part {
            Part::Button(b) => Some((w.clone(), *b)),
            _ => None,
        });
        if hover != self.frames.hover {
            self.frames.hover = hover;
        }
        let Some((_, part)) = under else { return false };
        let icon = match part {
            Part::Edge(edges) => resize_cursor(*edges),
            _ => CursorIcon::Default,
        };
        if self.cursor_status != CursorImageStatus::Named(icon) {
            self.cursor = Some(icon);
            self.cursor_status = CursorImageStatus::Named(icon);
        }
        true
    }

    /// A button press on a frame. Returns true when it started a grab (no
    /// further handling).
    pub fn frame_press(&mut self, window: Window, part: Part, button: u32, time: u32, pos: Point<f64, Logical>) -> bool {
        self.focus_window(&window);
        match (button, part) {
            (super::input::BTN_LEFT, Part::Button(b)) => {
                self.frames.pressed = Some((window, b));
                false
            }
            (super::input::BTN_LEFT, Part::Title) => {
                let double = self.frames.last_click.take().is_some_and(|(w, at)| {
                    w == window && time.wrapping_sub(at) <= self.frames.config.double_click_ms
                });
                if double {
                    self.title_action(&window, self.frames.config.double_click);
                    return false;
                }
                self.frames.last_click = Some((window.clone(), time));
                self.start_frame_move(window, pos)
            }
            (super::input::BTN_LEFT, Part::Edge(edges)) => self.start_frame_resize(window, Edges::from_bits(edges), pos),
            (super::input::BTN_MIDDLE, Part::Title | Part::Button(_)) => {
                self.title_action(&window, self.frames.config.middle_click);
                false
            }
            (super::input::BTN_RIGHT, Part::Title | Part::Button(_)) => {
                self.title_action(&window, self.frames.config.right_click);
                false
            }
            _ => false,
        }
    }

    /// The left button went up: act on a pressed frame button if the
    /// pointer is still on it.
    pub fn frame_release(&mut self, pos: Point<f64, Logical>) {
        let Some((window, button)) = self.frames.pressed.take() else { return };
        let still = self.frame_under(pos).is_some_and(|(w, part)| w == window && part == Part::Button(button));
        if !still {
            return;
        }
        match button {
            Button::Close => {
                if let Some(x11) = window.x11_surface() {
                    let _ = x11.close();
                }
            }
            Button::Maximize => self.title_action(&window, TitleAction::ToggleMaximize),
            Button::Minimize => self.minimize(&window),
        }
    }

    fn title_action(&mut self, window: &Window, action: TitleAction) {
        match action {
            TitleAction::ToggleMaximize => {
                if self.is_taken_over(window, Takeover::Maximized) {
                    self.restore_window(window);
                } else {
                    self.maximize(window);
                }
            }
            TitleAction::ToggleMaximizeHorizontally => self.toggle_maximize_direction(window, true),
            TitleAction::ToggleMaximizeVertically => self.toggle_maximize_direction(window, false),
            TitleAction::Minimize => self.minimize(window),
            TitleAction::Lower => self.lower_window(window),
            TitleAction::Menu => {
                let pos = self.seat.get_pointer().unwrap().current_location().to_i32_round();
                self.send_window_menu(window, pos);
            }
            TitleAction::None => {}
        }
    }

    /// Restore a maximized window being dragged so it comes back under
    /// the pointer, keeping where along its top it was grabbed.
    pub fn restore_under_pointer(&mut self, window: &Window, pos: Point<f64, Logical>) {
        let before = client_rect(&self.space, window);
        self.restore_window(window);
        if let (Some(before), Some(after)) = (before, client_rect(&self.space, window)) {
            if before != after {
                let fraction = (pos.x - f64::from(before.loc.x)) / f64::from(before.size.w.max(1));
                let x = (pos.x - fraction * f64::from(after.size.w)) as i32;
                let loc: Point<i32, Logical> = (x, before.loc.y).into();
                self.space.map_element(window.clone(), loc, true);
                self.restack_above();
                if let Some(x11) = window.x11_surface() {
                    let _ = x11.configure(Rectangle::new(loc, after.size));
                }
            }
        }
    }

    fn start_frame_move(&mut self, window: Window, pos: Point<f64, Logical>) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        // A maximized window is restored once the pointer really drags
        // (so a double click can restore it instead).
        let unmaximize = [Takeover::Maximized, Takeover::TiledLeft, Takeover::TiledRight].into_iter().any(|k| self.is_taken_over(&window, k));
        let initial_location = self.space.element_location(&window).unwrap_or_default();
        let start_data = GrabStartData { focus: None, button: super::input::BTN_LEFT, location: pos };
        pointer.set_grab(
            self,
            MoveGrab { start_data, window, initial_location, follow: false, unmaximize },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
        true
    }

    fn start_frame_resize(&mut self, window: Window, edges: Edges, pos: Point<f64, Logical>) -> bool {
        if self.is_taken_over(&window, Takeover::Maximized) {
            return false;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let location = self.space.element_location(&window).unwrap_or_default();
        let initial = Rectangle::new(location, window.geometry().size);
        self.resize = Some(ResizeTracking { window: window.clone(), edges, initial, finished: false });
        let start_data = GrabStartData { focus: None, button: super::input::BTN_LEFT, location: pos };
        pointer.set_grab(
            self,
            ResizeGrab { start_data, window, edges, initial, last_size: initial.size, follow: false },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
        true
    }

    /// Fill the work area in one direction, or restore.
    pub(super) fn toggle_maximize_direction(&mut self, window: &Window, horizontal: bool) {
        if self.is_taken_over(window, Takeover::Maximized) {
            self.restore_window(window);
            return;
        }
        let (Some(area), Some(current)) = (self.maximized_area(window), client_rect(&self.space, window)) else { return };
        let rect = if horizontal {
            Rectangle::new((area.loc.x, current.loc.y).into(), (area.size.w, current.size.h).into())
        } else {
            Rectangle::new((current.loc.x, area.loc.y).into(), (current.size.w, area.size.h).into())
        };
        self.take_over(window, Takeover::Maximized, rect);
    }

    /// Put `window` below every other window; the new top one gets focus.
    pub(super) fn lower_window(&mut self, window: &Window) {
        let others: Vec<Window> = self.space.elements().filter(|w| *w != window).cloned().collect();
        for other in others {
            self.space.raise_element(&other, false);
        }
        if let Some(top) = self.top_window() {
            self.focus_window(&top);
        }
    }
}
