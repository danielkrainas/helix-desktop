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
        let Some(ramps) = self.gamma.ramps.get(connector) else { return };
        if let Some(udev) = self.udev.as_mut() {
            if let Err(err) = udev.set_gamma(connector, ramps) {
                tracing::warn!(%err, connector, "cannot set gamma");
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
        let connectors: Vec<String> = self.gamma.ramps.keys().chain(self.gamma.ctm.keys()).cloned().collect();
        for connector in connectors {
            if self.gamma_size(&connector).is_none() {
                self.gamma.ramps.remove(&connector);
                self.gamma.ctm.remove(&connector);
                continue;
            }
            self.apply_gamma(&connector);
            if let (Some(ctm), Some(udev)) = (self.gamma.ctm.get(&connector).copied(), self.udev.as_mut()) {
                if let Err(err) = udev.set_ctm(&connector, &ctm) {
                    tracing::warn!(?err, connector, "cannot set color matrix");
                }
            }
        }
    }
}
