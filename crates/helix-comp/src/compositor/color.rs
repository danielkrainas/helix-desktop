//! `wp_color_manager_v1` (helix-desktop-spec `contracts/wayland/README.md`,
//! "Color management"): sRGB only, no color conversion.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1},
    wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::{self, Cause, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, ObjectId};
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak};

use super::Helix;

/// Version of the global.
const VERSION: u32 = 1;
/// sRGB luminances: minimum (cd/m² × 10000), maximum, reference white.
const SRGB_LUMINANCE: (u32, u32, u32) = (2000, 80, 80);
/// sRGB primaries and white point, × 1 000 000.
const SRGB_PRIMARIES: [i32; 8] = [640_000, 330_000, 300_000, 600_000, 150_000, 60_000, 312_700, 329_000];

/// A protocol error (logged: it ends the client).
fn error<R: Resource>(object: &R, code: impl Into<u32> + std::fmt::Debug + Copy, message: &str) {
    tracing::info!(interface = R::interface().name, ?code, message, "color management protocol error");
    object.post_error(code, message.to_owned());
}

/// What an image description describes: sRGB (everything Helix accepts
/// from apps), or a monitor in the BT.2100 color mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Params {
    tf: TransferFunction,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
    /// BT.2100 (PQ, BT.2020) with these luminances.
    hdr: Option<HdrLuminance>,
}

/// An HDR monitor's luminances: minimum (cd/m² × 10000), maximum and
/// reference white (cd/m²).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HdrLuminance {
    pub min: u32,
    pub max: u32,
    pub white: u32,
}

impl Params {
    const SRGB: Params = Params { tf: TransferFunction::Srgb, max_cll: None, max_fall: None, hdr: None };
}

/// BT.2020 primaries and white point, × 1 000 000.
const BT2020_PRIMARIES: [i32; 8] = [708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000];

/// An image description object's state.
#[derive(Debug)]
pub enum Description {
    Ready { identity: u32, params: Params, info: bool },
    Failed,
}

#[derive(Debug, Default)]
pub struct Creator {
    tf: Option<TransferFunction>,
    primaries: bool,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}

#[derive(Debug, Default)]
pub struct ColorState {
    identities: HashMap<Params, u32>,
    next_identity: u32,
    /// Surfaces with a `wp_color_management_surface_v1`.
    managed: HashSet<ObjectId>,
    /// Clients' monitor objects, to tell them when a description changes.
    outputs: Vec<(WpColorManagementOutputV1, Weak<WlOutput>)>,
}

impl ColorState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ()> + 'static,
    {
        dh.create_global::<D, WpColorManagerV1, ()>(VERSION, ());
        Self::default()
    }

    /// The identity of a description with `params` (never reused).
    fn identity(&mut self, params: Params) -> u32 {
        let next = &mut self.next_identity;
        *self.identities.entry(params).or_insert_with(|| {
            *next += 1;
            *next
        })
    }
}

impl Helix {
    /// A ready sRGB description (monitors, preferred) with information.
    fn srgb_description(&mut self, data_init: &mut DataInit<'_, Self>, id: New<WpImageDescriptionV1>) {
        self.ready_description(data_init, id, Params::SRGB);
    }

    fn ready_description(&mut self, data_init: &mut DataInit<'_, Self>, id: New<WpImageDescriptionV1>, params: Params) {
        let identity = self.color.identity(params);
        let desc = data_init.init(id, Description::Ready { identity, params, info: true });
        desc.ready(identity);
    }

    /// A monitor's description: BT.2100 in that color mode, else sRGB.
    fn output_params(&self, output: &WlOutput) -> Params {
        let hdr = smithay::output::Output::from_resource(output).and_then(|o| self.output_hdr(&o));
        match hdr {
            Some(hdr) => Params { tf: TransferFunction::St2084Pq, max_cll: None, max_fall: None, hdr: Some(hdr) },
            None => Params::SRGB,
        }
    }

    /// Tell clients that `output`'s description changed (its color mode).
    pub fn color_output_changed(&mut self, output: &smithay::output::Output) {
        self.color.outputs.retain(|(object, _)| object.is_alive());
        for (object, wl) in &self.color.outputs {
            if wl.upgrade().is_ok_and(|w| output.owns(&w)) {
                object.image_description_changed();
            }
        }
    }
}

impl GlobalDispatch<WpColorManagerV1, ()> for Helix {
    fn bind(_state: &mut Self, _dh: &DisplayHandle, _client: &Client, resource: New<WpColorManagerV1>, _data: &(), data_init: &mut DataInit<'_, Self>) {
        let manager = data_init.init(resource, ());
        manager.supported_intent(RenderIntent::Perceptual);
        manager.supported_feature(Feature::Parametric);
        manager.supported_tf_named(TransferFunction::Srgb);
        manager.supported_tf_named(TransferFunction::Gamma22);
        manager.supported_primaries_named(Primaries::Srgb);
        manager.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for Helix {
    fn request(state: &mut Self, _client: &Client, manager: &WpColorManagerV1, request: wp_color_manager_v1::Request, _data: &(), _dh: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        use wp_color_manager_v1::Request;
        match request {
            Request::GetOutput { id, output } => {
                let object = data_init.init(id, output.downgrade());
                state.color.outputs.push((object, output.downgrade()));
            }
            Request::GetSurface { id, surface } => {
                // A protocol error ends the client: then the new object is
                // left uninitialized (wayland-backend allows that only then).
                if state.color.managed.insert(surface.id()) {
                    data_init.init(id, surface.downgrade());
                } else {
                    error(manager, wp_color_manager_v1::Error::SurfaceExists, "the surface has color management already");
                }
            }
            Request::GetSurfaceFeedback { id, surface } => {
                data_init.init(id, surface.downgrade());
            }
            Request::CreateIccCreator { .. } => {
                error(manager, wp_color_manager_v1::Error::UnsupportedFeature, "ICC profiles are not supported");
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, Mutex::new(Creator::default()));
            }
            Request::CreateWindowsScrgb { .. } => {
                error(manager, wp_color_manager_v1::Error::UnsupportedFeature, "scRGB is not supported");
            }
            Request::Destroy => {}
            _ => error(manager, wp_color_manager_v1::Error::UnsupportedFeature, "not supported"),
        }
    }
}

impl Dispatch<WpColorManagementOutputV1, Weak<WlOutput>> for Helix {
    fn request(state: &mut Self, _client: &Client, _object: &WpColorManagementOutputV1, request: wp_color_management_output_v1::Request, output: &Weak<WlOutput>, _dh: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        if let wp_color_management_output_v1::Request::GetImageDescription { image_description } = request {
            if let Ok(output) = output.upgrade() {
                let params = state.output_params(&output);
                state.ready_description(data_init, image_description, params);
            } else {
                let desc = data_init.init(image_description, Description::Failed);
                desc.failed(Cause::NoOutput, "the monitor is gone".into());
            }
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, Weak<WlSurface>> for Helix {
    fn request(_state: &mut Self, _client: &Client, object: &WpColorManagementSurfaceV1, request: wp_color_management_surface_v1::Request, surface: &Weak<WlSurface>, _dh: &DisplayHandle, _data_init: &mut DataInit<'_, Self>) {
        use wp_color_management_surface_v1::{Error, Request};
        let inert = surface.upgrade().is_err();
        match request {
            Request::SetImageDescription { image_description, render_intent } => {
                if inert {
                    error(object, Error::Inert, "the surface is gone");
                } else if !matches!(image_description.data::<Description>(), Some(Description::Ready { .. })) {
                    error(object, Error::ImageDescription, "the image description is not ready");
                } else if render_intent != WEnum::Value(RenderIntent::Perceptual) {
                    error(object, Error::RenderIntent, "only the perceptual intent is supported");
                }
                // Accepted: every description Helix makes ready is drawn as is.
            }
            Request::UnsetImageDescription if inert => error(object, Error::Inert, "the surface is gone"),
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, _object: &WpColorManagementSurfaceV1, surface: &Weak<WlSurface>) {
        state.color.managed.remove(&surface.id());
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, Weak<WlSurface>> for Helix {
    fn request(state: &mut Self, _client: &Client, object: &WpColorManagementSurfaceFeedbackV1, request: wp_color_management_surface_feedback_v1::Request, surface: &Weak<WlSurface>, _dh: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        use wp_color_management_surface_feedback_v1::{Error, Request};
        let id = match request {
            Request::GetPreferred { image_description } | Request::GetPreferredParametric { image_description } => image_description,
            _ => return,
        };
        if surface.upgrade().is_err() {
            error(object, Error::Inert, "the surface is gone");
            return;
        }
        state.srgb_description(data_init, id);
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, Mutex<Creator>> for Helix {
    fn request(state: &mut Self, _client: &Client, object: &WpImageDescriptionCreatorParamsV1, request: wp_image_description_creator_params_v1::Request, creator: &Mutex<Creator>, _dh: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        use wp_image_description_creator_params_v1::{Error, Request};
        let mut c = creator.lock().unwrap();
        match request {
            Request::SetTfNamed { tf } => {
                if c.tf.is_some() {
                    error(object, Error::AlreadySet, "the transfer function is set already");
                    return;
                }
                match tf {
                    WEnum::Value(tf @ (TransferFunction::Srgb | TransferFunction::Gamma22)) => c.tf = Some(tf),
                    _ => error(object, Error::InvalidTf, "unsupported transfer function"),
                }
            }
            Request::SetPrimariesNamed { primaries } => {
                if c.primaries {
                    error(object, Error::AlreadySet, "the primaries are set already");
                } else if primaries != WEnum::Value(Primaries::Srgb) {
                    error(object, Error::InvalidPrimariesNamed, "unsupported primaries");
                } else {
                    c.primaries = true;
                }
            }
            Request::SetMaxCll { max_cll } => {
                if c.max_cll.replace(max_cll).is_some() {
                    error(object, Error::AlreadySet, "max_cll is set already");
                }
            }
            Request::SetMaxFall { max_fall } => {
                if c.max_fall.replace(max_fall).is_some() {
                    error(object, Error::AlreadySet, "max_fall is set already");
                }
            }
            Request::Create { image_description } => {
                let Some(tf) = c.tf.filter(|_| c.primaries) else {
                    let _ = image_description;
                    error(object, Error::IncompleteSet, "the transfer function and primaries are required");
                    return;
                };
                // Version 1: within the (sRGB) target luminance range.
                let (min, max) = (f64::from(SRGB_LUMINANCE.0) / 10000.0, SRGB_LUMINANCE.1);
                let in_range = |v: Option<u32>| v.is_none_or(|v| f64::from(v) > min && v <= max);
                let fall_ok = match (c.max_cll, c.max_fall) {
                    (Some(cll), Some(fall)) => fall <= cll,
                    _ => true,
                };
                if !fall_ok || !in_range(c.max_cll) || !in_range(c.max_fall) {
                    error(object, Error::InvalidLuminance, "invalid max_cll / max_fall");
                    return;
                }
                let params = Params { tf, max_cll: c.max_cll, max_fall: c.max_fall, hdr: None };
                let identity = state.color.identity(params);
                let desc = data_init.init(image_description, Description::Ready { identity, params, info: false });
                desc.ready(identity);
            }
            // Features Helix does not advertise.
            _ => error(object, Error::UnsupportedFeature, "not supported"),
        }
    }
}

impl Dispatch<WpImageDescriptionV1, Description> for Helix {
    fn request(state: &mut Self, _client: &Client, object: &WpImageDescriptionV1, request: wp_image_description_v1::Request, desc: &Description, _dh: &DisplayHandle, data_init: &mut DataInit<'_, Self>) {
        use wp_image_description_v1::{Error, Request};
        let Request::GetInformation { information } = request else { return };
        match desc {
            Description::Failed => error(object, Error::NotReady, "the image description is not ready"),
            Description::Ready { info: false, .. } => error(object, Error::NoInformation, "no information for this image description"),
            Description::Ready { params, .. } => {
                let info = data_init.init(information, ());
                let params = *params;
                // `done` destroys the object: send it once this request is
                // through (the backend still has to register the object).
                state.loop_handle.insert_idle(move |_| send_information(&info, params));
            }
        }
    }
}

/// The information events of a description with `params`.
fn send_information(info: &WpImageDescriptionInfoV1, params: Params) {
    if !info.is_alive() {
        return;
    }
    let (primaries, named, lum) = match params.hdr {
        Some(h) => (BT2020_PRIMARIES, Primaries::Bt2020, (h.min, h.max, h.white)),
        None => (SRGB_PRIMARIES, Primaries::Srgb, SRGB_LUMINANCE),
    };
    let [rx, ry, gx, gy, bx, by, wx, wy] = primaries;
    info.primaries(rx, ry, gx, gy, bx, by, wx, wy);
    info.primaries_named(named);
    info.tf_named(params.tf);
    info.luminances(lum.0, lum.1, lum.2);
    info.target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    info.target_luminance(lum.0, lum.1);
    if let Some(cll) = params.max_cll {
        info.target_max_cll(cll);
    }
    if let Some(fall) = params.max_fall {
        info.target_max_fall(fall);
    }
    info.done();
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for Helix {
    fn request(_: &mut Self, _: &Client, _: &WpImageDescriptionInfoV1, _: <WpImageDescriptionInfoV1 as Resource>::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<WpImageDescriptionCreatorIccV1, ()> for Helix {
    fn request(_: &mut Self, _: &Client, _: &WpImageDescriptionCreatorIccV1, _: <WpImageDescriptionCreatorIccV1 as Resource>::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_per_parameter_set() {
        let mut c = ColorState::default();
        let a = c.identity(Params::SRGB);
        let b = c.identity(Params { tf: TransferFunction::Gamma22, max_cll: None, max_fall: None, hdr: None });
        assert_ne!(a, 0);
        assert_ne!(a, b);
        assert_eq!(c.identity(Params::SRGB), a);
    }
}
