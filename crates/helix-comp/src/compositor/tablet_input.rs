//! Touchscreens mapped to their monitor, and drawing tablets driving the
//! pointer (helix-desktop-spec `contracts/gsettings/input-settings.md`,
//! "Touchscreens and tablets").

use std::collections::HashMap;

use helix_comp::edid::Identity;
use helix_comp::tablet::{self, ButtonAction, Tablet};
use smithay::backend::input::{ButtonState, Device, Event, InputBackend, ProximityState, TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle};

use super::Helix;

const BTN_LEFT: u32 = 0x110;
/// Fractions of the tablet are read at this resolution.
const FRACTION: i32 = 1 << 20;

/// A pen in proximity: its tablet's settings, read when it came near.
struct Pen {
    device: String,
    tablet: Tablet,
    buttons: [(u32, ButtonAction); 3],
    tip_down: bool,
}

#[derive(Default)]
pub struct TabletInput {
    /// Tablets' sizes (mm), by device id, from libinput.
    pub sizes: HashMap<String, (f64, f64)>,
    /// Monitors stepped past with `switch-monitor`, by device id.
    switched: HashMap<String, usize>,
    pen: Option<Pen>,
    /// Touchscreens' `output` setting, by device id (read at each touch down).
    touch_outputs: HashMap<String, Option<Vec<String>>>,
}

impl Helix {
    /// The output named by a [vendor, product, serial] setting.
    fn output_for_spec(&self, spec: &[String]) -> Option<Output> {
        self.space.outputs().find(|o| o.user_data().get::<Identity>().is_some_and(|id| id.matches(spec))).cloned()
    }

    /// Where a touchscreen's touches land: its `output` setting's monitor,
    /// else the built-in panel, else the primary monitor.
    pub(super) fn touch_area_for(&mut self, device: &impl Device, refresh: bool) -> Option<Rectangle<i32, Logical>> {
        let id = device.id();
        if refresh || !self.tablet.touch_outputs.contains_key(&id) {
            let spec = tablet::touchscreen_output(device.usb_id());
            self.tablet.touch_outputs.insert(id.clone(), spec);
        }
        let named = self.tablet.touch_outputs.get(&id).cloned().flatten().and_then(|spec| self.output_for_spec(&spec));
        match named {
            Some(output) => self.space.output_geometry(&output),
            None => self.touch_area(),
        }
    }

    /// The monitor a tablet maps to: its `output` setting's, else the
    /// primary; `switch-monitor` presses step on through the monitors.
    fn tablet_output(&self, device: &str, tablet: &Tablet) -> Option<Rectangle<i32, Logical>> {
        let base = tablet.output.as_ref().and_then(|spec| self.output_for_spec(spec)).or_else(|| self.primary())?;
        let mut outputs: Vec<(Output, Rectangle<i32, Logical>)> = self.space.outputs().filter_map(|o| Some((o.clone(), self.space.output_geometry(o)?))).collect();
        outputs.sort_by_key(|(_, g)| (g.loc.x, g.loc.y));
        let start = outputs.iter().position(|(o, _)| *o == base).unwrap_or(0);
        let step = self.tablet.switched.get(device).copied().unwrap_or(0);
        outputs.get((start + step) % outputs.len().max(1)).map(|(_, g)| *g)
    }

    fn pen_position<B: InputBackend>(&self, pen: &Pen, event: &impl TabletToolEvent<B>) -> Option<Point<f64, Logical>> {
        let area = self.tablet_output(&pen.device, &pen.tablet)?;
        let frac = (event.x_transformed(FRACTION) / f64::from(FRACTION), event.y_transformed(FRACTION) / f64::from(FRACTION));
        let size = self.tablet.sizes.get(&pen.device).copied();
        let (x, y) = tablet::map_point(frac, &pen.tablet, size, (f64::from(area.size.w), f64::from(area.size.h)))?;
        Some(area.loc.to_f64() + Point::from((x, y)))
    }

    /// Move the pointer with the pen.
    fn pen_moved<B: InputBackend>(&mut self, event: &impl TabletToolEvent<B>) {
        let Some(pen) = self.tablet.pen.take() else { return };
        if pen.tablet.relative {
            let d = event.delta();
            let delta = Point::from((d.x * tablet::RELATIVE_PX_PER_MM, d.y * tablet::RELATIVE_PX_PER_MM));
            self.pointer_moved_by(delta, delta, Event::time(event), event.time_msec());
        } else if let Some(pos) = self.pen_position(&pen, event) {
            self.pointer_moved_to(pos, Event::time(event), event.time_msec());
        }
        self.tablet.pen = Some(pen);
        self.device_moved(event.time_msec());
    }

    pub(super) fn tablet_proximity<B: InputBackend>(&mut self, event: &B::TabletToolProximityEvent) {
        let device = event.device();
        match event.state() {
            ProximityState::In => {
                let usb = device.usb_id();
                let tool = event.tool();
                self.tablet.pen = Some(Pen { device: device.id(), tablet: tablet::read_tablet(usb), buttons: tablet::read_stylus(tool.hardware_serial, usb), tip_down: false });
                self.pen_moved(event);
            }
            ProximityState::Out => {
                if let Some(pen) = self.tablet.pen.take() {
                    if pen.tip_down {
                        self.device_button(BTN_LEFT, ButtonState::Released, event.time_msec());
                    }
                }
            }
        }
    }

    pub(super) fn tablet_axis<B: InputBackend>(&mut self, event: &impl TabletToolEvent<B>) {
        self.pen_moved(event);
    }

    pub(super) fn tablet_tip<B: InputBackend>(&mut self, event: &B::TabletToolTipEvent) {
        let down = event.tip_state() == TabletToolTipState::Down;
        let Some(pen) = self.tablet.pen.as_mut() else { return };
        if pen.tip_down == down {
            return;
        }
        pen.tip_down = down;
        let state = if down { ButtonState::Pressed } else { ButtonState::Released };
        self.device_button(BTN_LEFT, state, event.time_msec());
    }

    pub(super) fn tablet_button<B: InputBackend>(&mut self, event: &B::TabletToolButtonEvent) {
        let Some(pen) = self.tablet.pen.as_ref() else { return };
        let action = pen.buttons.iter().find(|(code, _)| *code == event.button()).map(|(_, a)| *a);
        match action {
            Some(ButtonAction::Button(code)) => self.device_button(code, event.button_state(), event.time_msec()),
            Some(ButtonAction::SwitchMonitor) if event.button_state() == ButtonState::Pressed => {
                let device = pen.device.clone();
                *self.tablet.switched.entry(device.clone()).or_default() += 1;
                tracing::info!(device, "tablet switched to the next monitor");
            }
            _ => {}
        }
    }
}
