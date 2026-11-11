//! Gamma ramps and color transformation matrices per monitor (DisplayConfig
//! `GetCrtcGamma` / `SetCrtcGamma` / `SetOutputCTM`, helix-desktop-spec
//! `org.gnome.Mutter.DisplayConfig.md`, "Gamma" and "Color transformation
//! matrix"): sent to the CRTC on bare hardware, kept (without effect) when
//! nested.

use std::collections::HashMap;

use helix_comp::display_config::{identity_ramps, ColorError, ColorRequest, ColorSet, Ctm, Ramps};

use super::Helix;

/// Ramp length when nested (nothing to apply them to).
const NESTED_SIZE: usize = 256;

/// Ramps and matrices set by clients, by connector.
#[derive(Debug, Default)]
pub struct GammaState {
    ramps: HashMap<String, Ramps>,
    ctm: HashMap<String, Ctm>,
    /// `output-luminance` factors in effect (absent: 1).
    luminance: HashMap<String, f64>,
}

/// The factor for an `output-luminance` percentage (10–100 %).
pub fn luminance_factor(percent: f64) -> f64 {
    if percent.is_nan() {
        return 1.0;
    }
    percent.clamp(10.0, 100.0) / 100.0
}

/// `ramps` dimmed by `factor`.
pub fn scale_ramps(ramps: &Ramps, factor: f64) -> Ramps {
    let scale = |r: &Vec<u16>| r.iter().map(|v| (f64::from(*v) * factor).round().clamp(0.0, 65535.0) as u16).collect();
    (scale(&ramps.0), scale(&ramps.1), scale(&ramps.2))
}

impl Helix {
    /// The gamma ramp length of a monitor, if it exists.
    fn gamma_size(&self, connector: &str) -> Option<usize> {
        match &self.udev {
            Some(udev) => udev.gamma_size(connector),
            None => self.space.outputs().any(|o| o.name() == connector).then_some(NESTED_SIZE),
        }
    }

    pub fn gamma_request(&mut self, request: ColorRequest) {
        let ColorRequest { connector, set, reply } = request;
        let result = match self.gamma_size(&connector) {
            None => Err(ColorError::Invalid(format!("no monitor {connector}"))),
            Some(size) => {
                let current = self.gamma.ramps.get(&connector).cloned().unwrap_or_else(|| identity_ramps(size));
                match set {
                    None => Ok(current),
                    Some(ColorSet::Gamma(ramps)) if [ramps.0.len(), ramps.1.len(), ramps.2.len()] != [size; 3] => {
                        Err(ColorError::Invalid(format!("gamma ramps must have {size} entries")))
                    }
                    Some(ColorSet::Gamma(ramps)) => {
                        self.gamma.ramps.insert(connector.clone(), ramps.clone());
                        self.apply_gamma(&connector);
                        Ok(ramps)
                    }
                    Some(ColorSet::Ctm(ctm)) => self.set_ctm(&connector, ctm).map(|()| current),
                }
            }
        };
        let _ = reply.send(result);
    }

    fn apply_gamma(&mut self, connector: &str) {
        let factor = self.gamma.luminance.get(connector).copied();
        let ramps = match (self.gamma.ramps.get(connector), factor) {
            (Some(ramps), _) => ramps.clone(),
            (None, Some(_)) => match self.gamma_size(connector) {
                Some(size) => identity_ramps(size),
                None => return,
            },
            (None, None) => return,
        };
        let ramps = scale_ramps(&ramps, factor.unwrap_or(1.0));
        if let Some(udev) = self.udev.as_mut() {
            if let Err(err) = udev.set_gamma(connector, &ramps) {
                tracing::warn!(%err, connector, "cannot set gamma");
            }
        }
    }

    /// Apply `output-luminance` (color mode 0) to the monitors.
    pub fn apply_luminance(&mut self) {
        let entries = self.wm_settings.output_luminance.clone();
        for m in self.describe_outputs() {
            let percent = entries
                .iter()
                .find(|(c, v, p, s, mode, _)| (c, v, p, s) == (&m.connector, &m.vendor, &m.product, &m.serial) && *mode == 0)
                .map(|e| e.5);
            let old = self.gamma.luminance.get(&m.connector).copied();
            let new = percent.map(luminance_factor);
            if old != new {
                match new {
                    Some(f) => self.gamma.luminance.insert(m.connector.clone(), f),
                    None => self.gamma.luminance.remove(&m.connector),
                };
                tracing::info!(connector = m.connector, factor = new.unwrap_or(1.0), "output luminance");
                if new.is_none() && !self.gamma.ramps.contains_key(&m.connector) {
                    // Back to full: the identity ramp.
                    if let (Some(size), Some(udev)) = (self.gamma_size(&m.connector), self.udev.as_mut()) {
                        let _ = udev.set_gamma(&m.connector, &identity_ramps(size));
                    }
                } else {
                    self.apply_gamma(&m.connector);
                }
            }
        }
    }

    fn set_ctm(&mut self, connector: &str, ctm: Ctm) -> Result<(), ColorError> {
        if let Some(udev) = self.udev.as_mut() {
            udev.set_ctm(connector, &ctm)?;
        }
        self.gamma.ctm.insert(connector.to_owned(), ctm);
        Ok(())
    }

    /// Set the kept ramps and matrices again (after mode changes or a VT
    /// switch back); forget monitors that are gone.
    pub fn reapply_gamma(&mut self) {
        let mut connectors: Vec<String> =
            self.gamma.ramps.keys().chain(self.gamma.ctm.keys()).chain(self.gamma.luminance.keys()).cloned().collect();
        connectors.sort();
        connectors.dedup();
        for connector in connectors {
            if self.gamma_size(&connector).is_none() {
                self.gamma.ramps.remove(&connector);
                self.gamma.ctm.remove(&connector);
                self.gamma.luminance.remove(&connector);
                continue;
            }
            self.apply_gamma(&connector);
            if let (Some(ctm), Some(udev)) = (self.gamma.ctm.get(&connector).copied(), self.udev.as_mut()) {
                if let Err(err) = udev.set_ctm(&connector, &ctm) {
                    tracing::warn!(?err, connector, "cannot set color matrix");
                }
            }
        }
        // Newly connected monitors may have a luminance setting.
        self.apply_luminance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance() {
        assert_eq!(luminance_factor(50.0), 0.5);
        assert_eq!(luminance_factor(150.0), 1.0);
        assert_eq!(luminance_factor(1.0), 0.1);
        assert_eq!(luminance_factor(f64::NAN), 1.0);
        let r = scale_ramps(&identity_ramps(3), 0.5);
        assert_eq!(r.0, vec![0, 16384, 32768]);
    }
}
