//! Applying `ApplyMonitorsConfig` configurations (checked beforehand by
//! `helix_comp::display_config::Layout::parse`) to the outputs.

use helix_comp::display_config::{Monitor, MonitorsConfig};
use smithay::desktop::layer_map_for_output;
use smithay::output::Scale;
use smithay::utils::Transform;

use super::Helix;

/// DisplayConfig transform values are wl_output's.
pub fn to_transform(value: u32) -> Transform {
    match value {
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

pub fn from_transform(transform: Transform) -> u32 {
    match transform {
        Transform::Normal => 0,
        Transform::_90 => 1,
        Transform::_180 => 2,
        Transform::_270 => 3,
        Transform::Flipped => 4,
        Transform::Flipped90 => 5,
        Transform::Flipped180 => 6,
        Transform::Flipped270 => 7,
    }
}

/// Whole-number scales stay integers (`wl_output` reports them exactly).
pub fn to_scale(scale: f64) -> Scale {
    if scale.fract() == 0.0 {
        Scale::Integer(scale.max(1.0) as i32)
    } else {
        Scale::Fractional(scale)
    }
}

impl Helix {
    /// Apply `config`; returns the outputs as they are now.
    pub fn apply_monitors(&mut self, config: &MonitorsConfig) -> Result<Vec<Monitor>, String> {
        let result = if self.udev.is_some() {
            self.apply_drm(config)
        } else {
            self.apply_nested(config)
        };
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for output in &outputs {
            layer_map_for_output(output).arrange();
        }
        if result.is_ok() {
            // Monitors that are now off lose their shell surfaces.
            let off: Vec<_> = self.udev.as_ref().map(|u| u.outputs_off()).unwrap_or_default();
            for output in &off {
                self.close_layers(output);
            }
            // A mode change may reset the CRTC's gamma.
            self.reapply_gamma();
            self.primary_output = config
                .logical
                .iter()
                .find(|l| l.primary)
                .and_then(|l| l.monitors.first())
                .map(|c| c.connector.clone());
        }
        if result.is_ok() {
            self.rescue_windows();
            self.update_surface_scales();
            // The automatic Xwayland scale follows the monitors.
            self.apply_x11_settings();
            self.capture_zones_changed();
        }
        result
    }

    /// The nested window: one output whose only mode is the window size and
    /// which can't rotate; only the scale can change.
    fn apply_nested(&mut self, config: &MonitorsConfig) -> Result<Vec<Monitor>, String> {
        let output = self.space.outputs().next().cloned().ok_or("no output")?;
        let logical = config.logical.first().ok_or("no monitor")?;
        output.change_current_state(None, None, Some(to_scale(logical.scale)), None);
        let underscan = logical.monitors.first().is_some_and(|c| c.underscan);
        super::winit::TEST_UNDERSCAN.store(underscan, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(scale = logical.scale, underscan, "nested output scale changed");
        Ok(super::winit::describe(&output))
    }
}
