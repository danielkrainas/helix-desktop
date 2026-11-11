//! Gamma ramps per monitor (DisplayConfig `GetCrtcGamma` / `SetCrtcGamma`,
//! helix-desktop-spec `org.gnome.Mutter.DisplayConfig.md`, "Gamma"): sent
//! to the CRTC on bare hardware, kept (without effect) when nested.

use std::collections::HashMap;

use helix_comp::display_config::{identity_ramps, GammaRequest, Ramps};

use super::Helix;

/// Ramp length when nested (nothing to apply them to).
const NESTED_SIZE: usize = 256;

/// Ramps set by clients, by connector.
#[derive(Debug, Default)]
pub struct GammaState {
    ramps: HashMap<String, Ramps>,
}

impl Helix {
    /// The gamma ramp length of a monitor, if it exists.
    fn gamma_size(&self, connector: &str) -> Option<usize> {
        match &self.udev {
            Some(udev) => udev.gamma_size(connector),
            None => self.space.outputs().any(|o| o.name() == connector).then_some(NESTED_SIZE),
        }
    }

    pub fn gamma_request(&mut self, request: GammaRequest) {
        let GammaRequest { connector, set, reply } = request;
        let result = match self.gamma_size(&connector) {
            None => Err(format!("no monitor {connector}")),
            Some(size) => match set {
                None => Ok(self.gamma.ramps.get(&connector).cloned().unwrap_or_else(|| identity_ramps(size))),
                Some(ramps) if [ramps.0.len(), ramps.1.len(), ramps.2.len()] != [size; 3] => {
                    Err(format!("gamma ramps must have {size} entries"))
                }
                Some(ramps) => {
                    self.gamma.ramps.insert(connector.clone(), ramps.clone());
                    self.apply_gamma(&connector);
                    Ok(ramps)
                }
            },
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

    /// Set the kept ramps again (after mode changes or a VT switch back);
    /// forget monitors that are gone.
    pub fn reapply_gamma(&mut self) {
        let connectors: Vec<String> = self.gamma.ramps.keys().cloned().collect();
        for connector in connectors {
            if self.gamma_size(&connector).is_some() {
                self.apply_gamma(&connector);
            } else {
                self.gamma.ramps.remove(&connector);
            }
        }
    }
}
