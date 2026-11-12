//! `wp_color_manager_v1` for Helix (helix-desktop-spec
//! `contracts/wayland/README.md`, "Color management"): the protocol is
//! `helix_comp::color_management`; here are the monitors' descriptions.

use helix_comp::color_management::{ColorManagementHandler, ColorManager, Description, ParamsData};
use helix_comp::colorimetry::{tf, Colorimetry, Tf, BT2020_PRIMARIES};
use smithay::reexports::wayland_protocols::wp::color_management::v1::server as cm;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch, Weak};

use super::Helix;

/// An HDR monitor's luminances: minimum (cd/m² × 10000), maximum and
/// reference white (cd/m²).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HdrLuminance {
    pub min: u32,
    pub max: u32,
    pub white: u32,
}

impl Helix {
    /// Tell clients that `output`'s description changed (its color mode).
    pub fn color_output_changed(&mut self, output: &smithay::output::Output) {
        let output = output.clone();
        ColorManager::output_changed(self, move |w| output.owns(w));
    }
}

impl ColorManagementHandler for Helix {
    fn color_manager(&mut self) -> &mut ColorManager {
        &mut self.color_manager
    }

    /// BT.2100 in that color mode (its EDID luminances), else sRGB.
    fn output_colorimetry(&self, output: &WlOutput) -> Colorimetry {
        let hdr = smithay::output::Output::from_resource(output).and_then(|o| self.output_hdr(&o));
        match hdr {
            Some(h) => {
                let mut c = Colorimetry::named(BT2020_PRIMARIES, Tf::Named(tf::ST2084_PQ));
                c.luminance = (f64::from(h.min) / 10000.0, f64::from(h.max), f64::from(h.white));
                c.target_luminance = Some((c.luminance.0, c.luminance.1));
                c
            }
            None => Colorimetry::srgb(),
        }
    }

    /// Helix composites in sRGB, so that is best for every surface.
    fn surface_preferred(&self, _surface: &WlSurface) -> Colorimetry {
        Colorimetry::srgb()
    }

    fn defer(&mut self, job: Box<dyn FnOnce() + 'static>) {
        self.loop_handle.insert_idle(move |_| job());
    }
}

delegate_global_dispatch!(Helix: [cm::wp_color_manager_v1::WpColorManagerV1: ()] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_color_manager_v1::WpColorManagerV1: ()] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_color_management_output_v1::WpColorManagementOutputV1: Weak<WlOutput>] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_color_management_surface_v1::WpColorManagementSurfaceV1: Weak<WlSurface>] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1: Weak<WlSurface>] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1: ParamsData] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1: ()] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_image_description_v1::WpImageDescriptionV1: Description] => ColorManager);
delegate_dispatch!(Helix: [cm::wp_image_description_info_v1::WpImageDescriptionInfoV1: ()] => ColorManager);
