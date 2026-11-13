//! `wp_color_manager_v1`, version 1 (helix-desktop-spec
//! `contracts/wayland/README.md`, "Color management"): apps learn the
//! outputs' image descriptions and tell the compositor theirs; the
//! compositor converts described surfaces into its composition space
//! (`colorimetry`).

use std::sync::{Arc, Mutex};

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak};
use smithay::wayland::compositor::with_states;

use crate::colorimetry::{named_primaries, Chromaticities, Colorimetry, Tf, NAMED_PRIMARIES, NAMED_TFS};

/// What the compositor tells about its outputs.
pub trait ColorManagementHandler: 'static {
    fn color_manager(&mut self) -> &mut ColorManager;
    /// How `output` is driven.
    fn output_colorimetry(&self, output: &WlOutput) -> Colorimetry;
    /// What `surface` should best be drawn as (its output's).
    fn surface_preferred(&self, surface: &WlSurface) -> Colorimetry;
    /// Run `job` once the current request is through (events that destroy
    /// an object created by that request must wait for it).
    fn defer(&mut self, job: Box<dyn FnOnce() + 'static>);
}

/// Rendering intents taken: perceptual and relative (both drawn the same).
pub const INTENTS: [u32; 2] = [0, 1];
/// Features offered: icc_v2_v4, parametric, set_primaries,
/// set_tf_power, set_luminances, set_mastering_display_primaries,
/// extended_target_volume, windows_scrgb.
pub const FEATURES: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// The largest ICC profile taken (the protocol's limit).
const ICC_MAX: u32 = 32 * 1024 * 1024;

/// An ICC creator's profile: read, or why reading failed (not the
/// client's fault).
#[derive(Debug, Default)]
pub struct IccState {
    profile: Option<Result<Vec<u8>, String>>,
}

pub type IccData = Mutex<IccState>;

/// The global and the objects told about output changes.
#[derive(Debug, Default)]
pub struct ColorManager {
    _global: Option<GlobalId>,
    outputs: Vec<WpColorManagementOutputV1>,
    /// Surface feedbacks and the identity of their last preferred
    /// description.
    feedbacks: Vec<(WpColorManagementSurfaceFeedbackV1, u32)>,
}

impl ColorManager {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ()> + 'static,
    {
        Self { _global: Some(dh.create_global::<D, WpColorManagerV1, ()>(1, ())), ..Default::default() }
    }

    /// `output`'s image description changed (e.g. its color mode): tell
    /// its output objects, and surface feedbacks whose preferred
    /// description changed.
    pub fn output_changed<D: ColorManagementHandler>(state: &mut D, owns: impl Fn(&WlOutput) -> bool) {
        let (outputs, feedbacks) = {
            let m = state.color_manager();
            m.outputs.retain(|o| o.is_alive());
            m.feedbacks.retain(|(f, _)| f.is_alive());
            (m.outputs.clone(), m.feedbacks.clone())
        };
        for o in outputs {
            if o.data::<Weak<WlOutput>>().and_then(|w| w.upgrade().ok()).is_some_and(|w| owns(&w)) {
                o.image_description_changed();
            }
        }
        let mut now = Vec::new();
        for (f, last) in feedbacks {
            let Some(surface) = f.data::<Weak<WlSurface>>().and_then(|s| s.upgrade().ok()) else { continue };
            let id = identity(&state.surface_preferred(&surface));
            if id != last {
                f.preferred_changed(id);
            }
            now.push((f, id));
        }
        state.color_manager().feedbacks = now;
    }
}

/// Identities: equal descriptions share one (for the client's caching).
fn identity(c: &Colorimetry) -> u32 {
    static KNOWN: Mutex<Vec<(Colorimetry, u32)>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, id)) = known.iter().find(|(k, _)| k == c) {
        return *id;
    }
    let id = known.len() as u32 + 1;
    known.push((c.clone(), id));
    id
}

/// A surface's color state: what is set (current) and what the next
/// commit sets (pending: `Some(None)` unsets).
#[derive(Debug, Default)]
struct SurfaceColor {
    current: Option<Arc<Colorimetry>>,
    pending: Option<Option<Arc<Colorimetry>>>,
    /// A `wp_color_management_surface_v1` exists for it.
    taken: bool,
}

type SurfaceColorData = Mutex<SurfaceColor>;

fn with_color<T>(surface: &WlSurface, f: impl FnOnce(&mut SurfaceColor) -> T) -> T {
    with_states(surface, |states| {
        states.data_map.insert_if_missing_threadsafe(SurfaceColorData::default);
        let data = states.data_map.get::<SurfaceColorData>().expect("inserted");
        let mut data = data.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut data)
    })
}

/// `surface` was committed: its pending image description takes effect.
pub fn commit(surface: &WlSurface) {
    with_color(surface, |c| {
        if let Some(pending) = c.pending.take() {
            tracing::debug!(described = ?pending.as_deref().map(describe), "surface image description");
            c.current = pending;
        }
    });
}

/// The image description set on `surface`, if any.
pub fn surface_colorimetry(surface: &WlSurface) -> Option<Arc<Colorimetry>> {
    with_states(surface, colorimetry_of)
}

/// The image description set on a surface, from its state (for use where
/// the state is already at hand, as in a surface tree walk: locking it
/// again would deadlock).
pub fn colorimetry_of(states: &smithay::wayland::compositor::SurfaceData) -> Option<Arc<Colorimetry>> {
    states.data_map.get::<SurfaceColorData>().and_then(|d| d.lock().ok()?.current.clone())
}

/// An image description object: its colorimetry and whether
/// `get_information` is allowed.
#[derive(Debug)]
pub struct Description {
    colorimetry: Arc<Colorimetry>,
    info: bool,
}

/// What a parametric creator collected.
#[derive(Debug, Default)]
pub struct Params {
    tf: Option<Tf>,
    primaries: Option<(Chromaticities, Option<u32>)>,
    luminance: Option<(f64, f64, f64)>,
    target_primaries: Option<Chromaticities>,
    target_luminance: Option<(f64, f64)>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}

/// A parametric creator's state.
pub type ParamsData = Mutex<Params>;

/// Complete parameters as colorimetry, or the error to raise.
pub fn finish(p: &Params) -> Result<Colorimetry, (wp_image_description_creator_params_v1::Error, &'static str)> {
    use wp_image_description_creator_params_v1::Error as E;
    let (Some(tf), Some((primaries, named))) = (p.tf, p.primaries) else { return Err((E::IncompleteSet, "the transfer function and the primaries are required")) };
    if let (Some(cll), Some(fall)) = (p.max_cll, p.max_fall) {
        if fall > cll {
            return Err((E::InvalidLuminance, "max_fall is above max_cll"));
        }
    }
    if let Some((min, max)) = p.target_luminance {
        for v in [p.max_cll, p.max_fall].into_iter().flatten() {
            if f64::from(v) <= min || f64::from(v) > max {
                return Err((E::InvalidLuminance, "max_cll and max_fall must be within the mastering luminance range"));
            }
        }
    }
    let mut luminance = p.luminance.unwrap_or_else(|| Colorimetry::default_luminance(tf));
    if tf == Tf::Named(crate::colorimetry::tf::ST2084_PQ) {
        // The maximum follows from PQ's swing.
        luminance.1 = luminance.0 + 10000.0;
    }
    Ok(Colorimetry {
        primaries,
        primaries_named: named,
        tf,
        luminance,
        target_primaries: p.target_primaries,
        target_luminance: p.target_luminance,
        max_cll: p.max_cll,
        max_fall: p.max_fall,
    })
}

fn init_description<D>(init: &mut DataInit<'_, D>, id: New<WpImageDescriptionV1>, colorimetry: Colorimetry, info: bool)
where
    D: Dispatch<WpImageDescriptionV1, Description> + 'static,
{
    let ident = identity(&colorimetry);
    let res = init.init(id, Description { colorimetry: Arc::new(colorimetry), info });
    res.ready(ident);
}

impl<D> GlobalDispatch<WpColorManagerV1, (), D> for ColorManager
where
    D: Dispatch<WpColorManagerV1, ()> + 'static,
{
    fn bind(_: &mut D, _: &DisplayHandle, _: &Client, resource: New<WpColorManagerV1>, _: &(), init: &mut DataInit<'_, D>) {
        let m = init.init(resource, ());
        for i in INTENTS {
            m.supported_intent(wp_color_manager_v1::RenderIntent::try_from(i).expect("intent"));
        }
        for f in FEATURES {
            m.supported_feature(wp_color_manager_v1::Feature::try_from(f).expect("feature"));
        }
        for t in NAMED_TFS {
            m.supported_tf_named(wp_color_manager_v1::TransferFunction::try_from(t).expect("tf"));
        }
        for p in NAMED_PRIMARIES {
            m.supported_primaries_named(wp_color_manager_v1::Primaries::try_from(p).expect("primaries"));
        }
        m.done();
    }
}

impl<D> Dispatch<WpColorManagerV1, (), D> for ColorManager
where
    D: ColorManagementHandler
        + Dispatch<WpColorManagementOutputV1, Weak<WlOutput>>
        + Dispatch<WpColorManagementSurfaceV1, Weak<WlSurface>>
        + Dispatch<WpColorManagementSurfaceFeedbackV1, Weak<WlSurface>>
        + Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData>
        + Dispatch<WpImageDescriptionCreatorIccV1, IccData>
        + Dispatch<WpImageDescriptionV1, Description>
        + 'static,
{
    fn request(state: &mut D, _: &Client, resource: &WpColorManagerV1, request: wp_color_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        use wp_color_manager_v1::Request as R;
        match request {
            R::GetOutput { id, output } => {
                let o = init.init(id, output.downgrade());
                state.color_manager().outputs.push(o);
            }
            R::GetSurface { id, surface } => {
                let taken = with_color(&surface, |c| std::mem::replace(&mut c.taken, true));
                if taken {
                    resource.post_error(wp_color_manager_v1::Error::SurfaceExists, "the surface already has a color management surface");
                    return;
                }
                init.init(id, surface.downgrade());
            }
            R::GetSurfaceFeedback { id, surface } => {
                let preferred = identity(&state.surface_preferred(&surface));
                let f = init.init(id, surface.downgrade());
                state.color_manager().feedbacks.push((f, preferred));
            }
            // On protocol errors the new object is left uninitialized: the
            // client is gone anyway.
            R::CreateIccCreator { obj } => {
                init.init(obj, IccData::default());
            }
            R::CreateParametricCreator { obj } => {
                init.init(obj, ParamsData::default());
            }
            R::CreateWindowsScrgb { image_description } => init_description(init, image_description, Colorimetry::windows_scrgb(), false),
            _ => {}
        }
    }
}

impl<D> Dispatch<WpColorManagementOutputV1, Weak<WlOutput>, D> for ColorManager
where
    D: ColorManagementHandler + Dispatch<WpImageDescriptionV1, Description> + 'static,
{
    fn request(state: &mut D, _: &Client, _: &WpColorManagementOutputV1, request: wp_color_management_output_v1::Request, output: &Weak<WlOutput>, _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        if let wp_color_management_output_v1::Request::GetImageDescription { image_description } = request {
            match output.upgrade() {
                Ok(output) => {
                    let c = state.output_colorimetry(&output);
                    init_description(init, image_description, c, true);
                }
                Err(_) => {
                    let res = init.init(image_description, Description { colorimetry: Arc::new(Colorimetry::srgb()), info: false });
                    res.failed(wp_image_description_v1::Cause::NoOutput, "the output is gone".into());
                }
            }
        }
    }
}

impl<D> Dispatch<WpColorManagementSurfaceV1, Weak<WlSurface>, D> for ColorManager
where
    D: ColorManagementHandler + 'static,
{
    fn request(_: &mut D, _: &Client, resource: &WpColorManagementSurfaceV1, request: wp_color_management_surface_v1::Request, surface: &Weak<WlSurface>, _: &DisplayHandle, _: &mut DataInit<'_, D>) {
        use wp_color_management_surface_v1::{Error as E, Request as R};
        let Ok(surface) = surface.upgrade() else {
            if !matches!(request, R::Destroy) {
                resource.post_error(E::Inert, "the surface is gone");
            }
            return;
        };
        match request {
            R::SetImageDescription { image_description, render_intent } => {
                let intent = match render_intent {
                    WEnum::Value(i) => i as u32,
                    WEnum::Unknown(i) => i,
                };
                if !INTENTS.contains(&intent) {
                    resource.post_error(E::RenderIntent, "unsupported rendering intent");
                    return;
                }
                let Some(desc) = image_description.data::<Description>() else {
                    resource.post_error(E::ImageDescription, "not a ready image description");
                    return;
                };
                let c = desc.colorimetry.clone();
                with_color(&surface, |s| s.pending = Some(Some(c)));
            }
            R::UnsetImageDescription => with_color(&surface, |s| s.pending = Some(None)),
            R::Destroy => with_color(&surface, |s| {
                s.pending = Some(None);
                s.taken = false;
            }),
            _ => {}
        }
    }
}

impl<D> Dispatch<WpColorManagementSurfaceFeedbackV1, Weak<WlSurface>, D> for ColorManager
where
    D: ColorManagementHandler + Dispatch<WpImageDescriptionV1, Description> + 'static,
{
    fn request(state: &mut D, _: &Client, resource: &WpColorManagementSurfaceFeedbackV1, request: wp_color_management_surface_feedback_v1::Request, surface: &Weak<WlSurface>, _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        if let wp_color_management_surface_feedback_v1::Request::GetPreferred { image_description } = request {
            let Ok(surface) = surface.upgrade() else {
                let _ = image_description;
                resource.post_error(wp_color_management_surface_feedback_v1::Error::Inert, "the surface is gone");
                return;
            };
            let c = state.surface_preferred(&surface);
            init_description(init, image_description, c, true);
        }
    }
}

impl<D> Dispatch<WpImageDescriptionCreatorIccV1, IccData, D> for ColorManager
where
    D: Dispatch<WpImageDescriptionV1, Description> + 'static,
{
    fn request(_: &mut D, _: &Client, resource: &WpImageDescriptionCreatorIccV1, request: wp_image_description_creator_icc_v1::Request, data: &IccData, _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        use wp_image_description_creator_icc_v1::{Error as E, Request as R};
        let mut state = data.lock().unwrap_or_else(|e| e.into_inner());
        match request {
            R::SetIccFile { icc_profile, offset, length } => {
                if state.profile.is_some() {
                    return resource.post_error(E::AlreadySet, "the ICC file is already set");
                }
                if length == 0 || length > ICC_MAX {
                    return resource.post_error(E::BadSize, "an ICC profile is 1 byte to 32 MB");
                }
                let file = std::fs::File::from(icc_profile);
                let size = match file.metadata() {
                    Ok(m) if m.is_file() => m.len(),
                    _ => return resource.post_error(E::BadFd, "the ICC fd is not a seekable file"),
                };
                if u64::from(offset) + u64::from(length) > size {
                    return resource.post_error(E::OutOfFile, "the ICC profile runs past the end of the file");
                }
                let mut buf = vec![0u8; length as usize];
                use std::os::unix::fs::FileExt;
                state.profile = Some(file.read_exact_at(&mut buf, u64::from(offset)).map(|()| buf).map_err(|e| e.to_string()));
            }
            R::Create { image_description } => {
                let Some(profile) = state.profile.take() else {
                    let _ = image_description;
                    return resource.post_error(E::IncompleteSet, "no ICC file was set");
                };
                match profile {
                    Ok(bytes) => match crate::colorimetry::from_icc(&bytes) {
                        Ok(c) => init_description(init, image_description, c, false),
                        Err(err) => {
                            tracing::info!(err, "ICC profile not usable");
                            let res = init.init(image_description, Description { colorimetry: Arc::new(Colorimetry::srgb()), info: false });
                            res.failed(wp_image_description_v1::Cause::Unsupported, err);
                        }
                    },
                    Err(err) => {
                        let res = init.init(image_description, Description { colorimetry: Arc::new(Colorimetry::srgb()), info: false });
                        res.failed(wp_image_description_v1::Cause::OperatingSystem, err);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Chromaticities from the protocol's millionths.
fn chroma(v: [i32; 8]) -> Chromaticities {
    v.map(|c| f64::from(c) / 1_000_000.0)
}

impl<D> Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData, D> for ColorManager
where
    D: Dispatch<WpImageDescriptionV1, Description> + 'static,
{
    fn request(_: &mut D, _: &Client, resource: &WpImageDescriptionCreatorParamsV1, request: wp_image_description_creator_params_v1::Request, data: &ParamsData, _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        use wp_image_description_creator_params_v1::{Error as E, Request as R};
        let mut p = data.lock().unwrap_or_else(|e| e.into_inner());
        let error = |code: E, msg: &str| resource.post_error(code, msg.to_owned());
        match request {
            R::SetTfNamed { tf } => {
                let tf = match tf {
                    WEnum::Value(t) => t as u32,
                    WEnum::Unknown(t) => t,
                };
                if p.tf.is_some() {
                    return error(E::AlreadySet, "the transfer function is already set");
                }
                if !NAMED_TFS.contains(&tf) {
                    return error(E::InvalidTf, "unsupported transfer function");
                }
                p.tf = Some(Tf::Named(tf));
            }
            R::SetTfPower { eexp } => {
                if p.tf.is_some() {
                    return error(E::AlreadySet, "the transfer function is already set");
                }
                let exp = f64::from(eexp) / 10000.0;
                if !(1.0..=10.0).contains(&exp) {
                    return error(E::InvalidTf, "the exponent must be between 1.0 and 10.0");
                }
                p.tf = Some(Tf::Power(exp));
            }
            R::SetPrimariesNamed { primaries } => {
                let n = match primaries {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(v) => v,
                };
                if p.primaries.is_some() {
                    return error(E::AlreadySet, "the primaries are already set");
                }
                let Some(c) = named_primaries(n) else { return error(E::InvalidPrimariesNamed, "unsupported primaries") };
                p.primaries = Some((c, Some(n)));
            }
            R::SetPrimaries { r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y } => {
                if p.primaries.is_some() {
                    return error(E::AlreadySet, "the primaries are already set");
                }
                p.primaries = Some((chroma([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]), None));
            }
            R::SetLuminances { min_lum, max_lum, reference_lum } => {
                if p.luminance.is_some() {
                    return error(E::AlreadySet, "the luminances are already set");
                }
                let min = f64::from(min_lum) / 10000.0;
                if f64::from(max_lum) <= min || f64::from(reference_lum) <= min {
                    return error(E::InvalidLuminance, "the maximum and reference white must be above the minimum");
                }
                p.luminance = Some((min, f64::from(max_lum), f64::from(reference_lum)));
            }
            R::SetMasteringDisplayPrimaries { r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y } => {
                if p.target_primaries.is_some() {
                    return error(E::AlreadySet, "the mastering primaries are already set");
                }
                p.target_primaries = Some(chroma([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]));
            }
            R::SetMasteringLuminance { min_lum, max_lum } => {
                if p.target_luminance.is_some() {
                    return error(E::AlreadySet, "the mastering luminance is already set");
                }
                let min = f64::from(min_lum) / 10000.0;
                if f64::from(max_lum) <= min {
                    return error(E::InvalidLuminance, "the mastering maximum must be above its minimum");
                }
                p.target_luminance = Some((min, f64::from(max_lum)));
            }
            R::SetMaxCll { max_cll } => {
                if p.max_cll.replace(max_cll).is_some() {
                    error(E::AlreadySet, "max_cll is already set");
                }
            }
            R::SetMaxFall { max_fall } => {
                if p.max_fall.replace(max_fall).is_some() {
                    error(E::AlreadySet, "max_fall is already set");
                }
            }
            R::Create { image_description } => match finish(&p) {
                Ok(c) => init_description(init, image_description, c, false),
                Err((code, msg)) => {
                    let _ = image_description;
                    error(code, msg);
                }
            },
            _ => {}
        }
    }
}

impl<D> Dispatch<WpImageDescriptionV1, Description, D> for ColorManager
where
    D: ColorManagementHandler + Dispatch<WpImageDescriptionInfoV1, ()> + 'static,
{
    fn request(state: &mut D, _: &Client, resource: &WpImageDescriptionV1, request: wp_image_description_v1::Request, data: &Description, _: &DisplayHandle, init: &mut DataInit<'_, D>) {
        if let wp_image_description_v1::Request::GetInformation { information } = request {
            if !data.info {
                let _ = information;
                resource.post_error(wp_image_description_v1::Error::NoInformation, "this image description gives no information");
                return;
            }
            let info = init.init(information, ());
            let c = data.colorimetry.clone();
            // `done` destroys the object: send the events once this request
            // is through.
            state.defer(Box::new(move || send_information(&info, &c)));
        }
    }
}

/// The information events of `c`, then `done`.
fn send_information(info: &WpImageDescriptionInfoV1, c: &Colorimetry) {
    if !info.is_alive() {
        return;
    }
    let m = |v: f64| (v * 1_000_000.0).round() as i32;
    let [rx, ry, gx, gy, bx, by, wx, wy] = c.primaries.map(m);
    info.primaries(rx, ry, gx, gy, bx, by, wx, wy);
    if let Some(n) = c.primaries_named.and_then(|n| wp_color_manager_v1::Primaries::try_from(n).ok()) {
        info.primaries_named(n);
    }
    match c.tf {
        Tf::Named(t) => {
            if let Ok(t) = wp_color_manager_v1::TransferFunction::try_from(t) {
                info.tf_named(t);
            }
        }
        Tf::Power(p) => info.tf_power((p * 10000.0).round() as u32),
        // ICC descriptions give no information.
        Tf::Parametric(_) => {}
    }
    let (min, max, reference) = c.luminance;
    info.luminances((min * 10000.0).round() as u32, max.round() as u32, reference.round() as u32);
    let [rx, ry, gx, gy, bx, by, wx, wy] = c.target_primaries.unwrap_or(c.primaries).map(m);
    info.target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    let (tmin, tmax) = c.target_luminance.unwrap_or((min, max));
    info.target_luminance((tmin * 10000.0).round() as u32, tmax.round() as u32);
    if let Some(v) = c.max_cll {
        info.target_max_cll(v);
    }
    if let Some(v) = c.max_fall {
        info.target_max_fall(v);
    }
    info.done();
}

impl<D> Dispatch<WpImageDescriptionInfoV1, (), D> for ColorManager
where
    D: 'static,
{
    fn request(_: &mut D, _: &Client, _: &WpImageDescriptionInfoV1, _: <WpImageDescriptionInfoV1 as Resource>::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, D>) {}
}

/// For logs.
pub fn describe(c: &Colorimetry) -> String {
    format!("{:?}/{:?}", c.primaries_named, c.tf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colorimetry::tf;

    #[test]
    fn params() {
        let mut p = Params::default();
        assert!(finish(&p).is_err(), "incomplete");
        p.tf = Some(Tf::Named(tf::ST2084_PQ));
        p.primaries = Some((named_primaries(6).unwrap(), Some(6)));
        let c = finish(&p).unwrap();
        assert_eq!(c.luminance, (0.005, 10000.005, 203.0), "PQ defaults; the maximum from PQ's swing");
        p.max_cll = Some(400);
        p.max_fall = Some(800);
        assert!(finish(&p).is_err(), "max_fall above max_cll");
        p.max_fall = Some(200);
        p.target_luminance = Some((0.01, 300.0));
        assert!(finish(&p).is_err(), "max_cll above the mastering maximum");
        p.target_luminance = Some((0.01, 1000.0));
        assert!(finish(&p).is_ok());
        assert_eq!(identity(&Colorimetry::srgb()), identity(&Colorimetry::srgb()), "equal descriptions share an identity");
        assert_ne!(identity(&Colorimetry::srgb()), identity(&Colorimetry::windows_scrgb()));
    }
}
