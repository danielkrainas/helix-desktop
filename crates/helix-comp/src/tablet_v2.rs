//! Server side of the tablet protocol, `zwp_tablet_manager_v2` version 1
//! (helix-desktop-spec `contracts/gsettings/input-settings.md`,
//! "Tablets"): tablets, their tools (pens, erasers…) with pressure, tilt
//! and the other axes, and pads with their buttons, rings, strips and
//! mode groups.
//!
//! The compositor tells [`TabletV2`] about devices and input; it keeps
//! every client's objects and sends the events. Not yet tried with real
//! tablet hardware: the tests drive it with a Wayland client in-process.

use std::collections::HashMap;

use smithay::reexports::wayland_protocols::wp::tablet::zv2::server::{
    zwp_tablet_manager_v2::{self, ZwpTabletManagerV2},
    zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
    zwp_tablet_pad_ring_v2::{self, ZwpTabletPadRingV2},
    zwp_tablet_pad_strip_v2::{self, ZwpTabletPadStripV2},
    zwp_tablet_pad_v2::{self, ZwpTabletPadV2},
    zwp_tablet_seat_v2::ZwpTabletSeatV2,
    zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
    zwp_tablet_v2::ZwpTabletV2,
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

/// The version offered (without dials).
pub const VERSION: u32 = 1;

/// A tablet: `id` is the compositor's own key for it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabletInfo {
    pub id: String,
    pub name: String,
    pub usb: Option<(u32, u32)>,
    /// Its device node, such as `/dev/input/event7`.
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolCaps {
    pub tilt: bool,
    pub pressure: bool,
    pub distance: bool,
    pub rotation: bool,
    pub slider: bool,
    pub wheel: bool,
}

/// A tool: `id` is the compositor's own key for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub id: u64,
    pub kind: zwp_tablet_tool_v2::Type,
    pub serial: u64,
    pub hardware_id: u64,
    pub caps: ToolCaps,
}

/// One of a pad's mode groups: the buttons, rings and strips in it (by
/// the pad's numbering), how many modes it has and the current one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PadGroup {
    pub buttons: Vec<u32>,
    pub rings: Vec<u32>,
    pub strips: Vec<u32>,
    pub modes: u32,
    pub mode: u32,
}

/// A pad: `tablet` is the id of the tablet it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct PadInfo {
    pub id: String,
    pub tablet: String,
    pub path: String,
    pub buttons: u32,
    pub groups: Vec<PadGroup>,
}

/// A tool's axes in one frame (`None`: unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Axes {
    /// 0–1.
    pub pressure: Option<f64>,
    /// 0–1.
    pub distance: Option<f64>,
    /// Degrees.
    pub tilt: Option<(f64, f64)>,
    /// Degrees.
    pub rotation: Option<f64>,
    /// -1–1.
    pub slider: Option<f64>,
    /// Degrees and clicks.
    pub wheel: Option<(f64, i32)>,
}

struct GroupRes {
    res: ZwpTabletPadGroupV2,
    rings: Vec<(u32, ZwpTabletPadRingV2)>,
    strips: Vec<(u32, ZwpTabletPadStripV2)>,
}

struct PadRes {
    pad: String,
    res: ZwpTabletPadV2,
    groups: Vec<GroupRes>,
}

/// Everything the protocol needs to remember.
#[derive(Default)]
pub struct TabletV2 {
    seats: Vec<ZwpTabletSeatV2>,
    tablets: Vec<TabletInfo>,
    tools: Vec<ToolInfo>,
    pads: Vec<PadInfo>,
    tablet_res: Vec<(String, ZwpTabletV2)>,
    tool_res: Vec<(u64, ZwpTabletToolV2)>,
    pad_res: Vec<PadRes>,
    /// Each tool's surface and the tablet it is on.
    tool_focus: HashMap<u64, (WlSurface, String)>,
    pad_focus: HashMap<String, WlSurface>,
}

impl std::fmt::Debug for TabletV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabletV2").field("tablets", &self.tablets).field("tools", &self.tools).field("pads", &self.pads).finish()
    }
}

/// What the compositor provides.
pub trait TabletV2Handler: Sized + 'static {
    fn tablet_v2(&mut self) -> &mut TabletV2;
    /// A client set the cursor for `tool` (`None`: hide it).
    fn tool_cursor(&mut self, tool: u64, cursor: Option<(WlSurface, (i32, i32))>);
}

fn same_client(a: &impl Resource, b: &impl Resource) -> bool {
    a.client().map(|c| c.id()) == b.client().map(|c| c.id())
}

/// The bytes of a `zwp_tablet_pad_group_v2.buttons` array.
pub fn button_array(buttons: &[u32]) -> Vec<u8> {
    buttons.iter().flat_map(|b| b.to_ne_bytes()).collect()
}

impl TabletV2 {
    pub fn global<D>(dh: &DisplayHandle) -> GlobalId
    where
        D: GlobalDispatch<ZwpTabletManagerV2, ()> + 'static,
    {
        dh.create_global::<D, ZwpTabletManagerV2, ()>(VERSION, ())
    }

    pub fn tablets(&self) -> &[TabletInfo] {
        &self.tablets
    }

    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    pub fn pads(&self) -> &[PadInfo] {
        &self.pads
    }

    /// Whether `client` asked for tablet events (has a tablet seat).
    pub fn has_seat(&self, client: &ClientId) -> bool {
        self.seats.iter().any(|s| s.client().is_some_and(|c| c.id() == *client))
    }

    fn announce_tablet<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, seat: &ZwpTabletSeatV2, info: &TabletInfo) {
        let Some(client) = seat.client() else { return };
        let Ok(res) = client.create_resource::<ZwpTabletV2, String, D>(dh, seat.version(), info.id.clone()) else { return };
        seat.tablet_added(&res);
        res.name(info.name.clone());
        if let Some((vid, pid)) = info.usb {
            res.id(vid, pid);
        }
        res.path(info.path.clone());
        res.done();
        self.tablet_res.push((info.id.clone(), res));
    }

    fn announce_tool<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, seat: &ZwpTabletSeatV2, info: &ToolInfo) {
        let Some(client) = seat.client() else { return };
        let Ok(res) = client.create_resource::<ZwpTabletToolV2, u64, D>(dh, seat.version(), info.id) else { return };
        seat.tool_added(&res);
        res._type(info.kind);
        res.hardware_serial((info.serial >> 32) as u32, info.serial as u32);
        if info.hardware_id != 0 {
            res.hardware_id_wacom((info.hardware_id >> 32) as u32, info.hardware_id as u32);
        }
        use zwp_tablet_tool_v2::Capability as C;
        for (has, cap) in [(info.caps.tilt, C::Tilt), (info.caps.pressure, C::Pressure), (info.caps.distance, C::Distance), (info.caps.rotation, C::Rotation), (info.caps.slider, C::Slider), (info.caps.wheel, C::Wheel)] {
            if has {
                res.capability(cap);
            }
        }
        res.done();
        self.tool_res.push((info.id, res));
    }

    fn announce_pad<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, seat: &ZwpTabletSeatV2, info: &PadInfo) {
        let Some(client) = seat.client() else { return };
        let version = seat.version();
        let Ok(res) = client.create_resource::<ZwpTabletPadV2, String, D>(dh, version, info.id.clone()) else { return };
        seat.pad_added(&res);
        let mut groups = Vec::new();
        for (index, group) in info.groups.iter().enumerate() {
            let Ok(gres) = client.create_resource::<ZwpTabletPadGroupV2, (String, usize), D>(dh, version, (info.id.clone(), index)) else { continue };
            res.group(&gres);
            gres.buttons(button_array(&group.buttons));
            let mut rings = Vec::new();
            for &ring in &group.rings {
                if let Ok(r) = client.create_resource::<ZwpTabletPadRingV2, (String, u32), D>(dh, version, (info.id.clone(), ring)) {
                    gres.ring(&r);
                    rings.push((ring, r));
                }
            }
            let mut strips = Vec::new();
            for &strip in &group.strips {
                if let Ok(s) = client.create_resource::<ZwpTabletPadStripV2, (String, u32), D>(dh, version, (info.id.clone(), strip)) {
                    gres.strip(&s);
                    strips.push((strip, s));
                }
            }
            gres.modes(group.modes.max(1));
            gres.done();
            groups.push(GroupRes { res: gres, rings, strips });
        }
        res.path(info.path.clone());
        res.buttons(info.buttons);
        res.done();
        self.pad_res.push(PadRes { pad: info.id.clone(), res, groups });
    }

    fn announce_all<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, seat: &ZwpTabletSeatV2) {
        for info in self.tablets.clone() {
            self.announce_tablet::<D>(dh, seat, &info);
        }
        for info in self.tools.clone() {
            self.announce_tool::<D>(dh, seat, &info);
        }
        for info in self.pads.clone() {
            self.announce_pad::<D>(dh, seat, &info);
        }
    }

    pub fn add_tablet<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, info: TabletInfo) {
        if self.tablets.iter().any(|t| t.id == info.id) {
            return;
        }
        for seat in self.seats.clone() {
            self.announce_tablet::<D>(dh, &seat, &info);
        }
        self.tablets.push(info);
    }

    pub fn remove_tablet(&mut self, id: &str) {
        self.tablets.retain(|t| t.id != id);
        self.tablet_res.retain(|(t, res)| {
            if t == id {
                res.removed();
            }
            t != id
        });
        let tools: Vec<u64> = self.tool_focus.iter().filter(|(_, (_, t))| t == id).map(|(k, _)| *k).collect();
        for tool in tools {
            self.tool_focus.remove(&tool);
        }
    }

    pub fn add_tool<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, info: ToolInfo) {
        if self.tools.iter().any(|t| t.id == info.id) {
            return;
        }
        for seat in self.seats.clone() {
            self.announce_tool::<D>(dh, &seat, &info);
        }
        self.tools.push(info);
    }

    pub fn has_tool(&self, id: u64) -> bool {
        self.tools.iter().any(|t| t.id == id)
    }

    pub fn remove_tool(&mut self, id: u64) {
        self.tools.retain(|t| t.id != id);
        self.tool_focus.remove(&id);
        self.tool_res.retain(|(t, res)| {
            if *t == id {
                res.removed();
            }
            *t != id
        });
    }

    pub fn add_pad<D: TabletV2Dispatch>(&mut self, dh: &DisplayHandle, info: PadInfo) {
        if self.pads.iter().any(|p| p.id == info.id) {
            return;
        }
        for seat in self.seats.clone() {
            self.announce_pad::<D>(dh, &seat, &info);
        }
        self.pads.push(info);
    }

    pub fn remove_pad(&mut self, id: &str) {
        self.pads.retain(|p| p.id != id);
        self.pad_focus.remove(id);
        self.pad_res.retain(|p| {
            if p.pad == id {
                p.res.removed();
            }
            p.pad != id
        });
    }

    fn tool_res_for(&self, tool: u64, surface: &WlSurface) -> Option<&ZwpTabletToolV2> {
        self.tool_res.iter().find(|(t, res)| *t == tool && same_client(res, surface)).map(|(_, r)| r)
    }

    fn tablet_res_for(&self, tablet: &str, surface: &WlSurface) -> Option<&ZwpTabletV2> {
        self.tablet_res.iter().find(|(t, res)| t == tablet && same_client(res, surface)).map(|(_, r)| r)
    }

    /// The surface `tool` is over, if its client gets tablet events.
    pub fn tool_surface(&self, tool: u64) -> Option<&WlSurface> {
        self.tool_focus.get(&tool).map(|(s, _)| s)
    }

    /// `tool`, on `tablet`, is over `target` (a surface and the tool's
    /// position on it) or over none. Sends `proximity_out` and
    /// `proximity_in` as the surface changes, and `motion`. Returns whether
    /// the surface's client gets the tool's events (else the compositor
    /// should treat the tool as a pointer there).
    pub fn tool_over(&mut self, tool: u64, tablet: &str, target: Option<(&WlSurface, (f64, f64))>, serial: u32, time: u32) -> bool {
        let same = match (self.tool_focus.get(&tool), target) {
            (Some((current, _)), Some((surface, _))) => current == surface,
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some((old, _)) = self.tool_focus.remove(&tool) {
                if let Some(res) = self.tool_res_for(tool, &old) {
                    res.proximity_out();
                    res.frame(time);
                }
            }
            if let Some((surface, _)) = target {
                let (Some(res), Some(tablet_res)) = (self.tool_res_for(tool, surface), self.tablet_res_for(tablet, surface)) else { return false };
                res.proximity_in(serial, tablet_res, surface);
                self.tool_focus.insert(tool, (surface.clone(), tablet.to_owned()));
            }
        }
        let Some((surface, (x, y))) = target else { return false };
        let Some(res) = self.tool_res_for(tool, surface) else { return false };
        if !self.tool_focus.contains_key(&tool) {
            return false;
        }
        res.motion(x, y);
        true
    }

    /// The tool left the tablet.
    pub fn tool_out(&mut self, tool: u64, time: u32) {
        if let Some((surface, _)) = self.tool_focus.remove(&tool) {
            if let Some(res) = self.tool_res_for(tool, &surface) {
                res.proximity_out();
                res.frame(time);
            }
        }
    }

    fn focused_tool(&self, tool: u64) -> Option<&ZwpTabletToolV2> {
        let (surface, _) = self.tool_focus.get(&tool)?;
        self.tool_res_for(tool, surface)
    }

    pub fn tool_axes(&self, tool: u64, axes: &Axes) {
        let Some(res) = self.focused_tool(tool) else { return };
        let scale = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
        if let Some(p) = axes.pressure {
            res.pressure(scale(p));
        }
        if let Some(d) = axes.distance {
            res.distance(scale(d));
        }
        if let Some((x, y)) = axes.tilt {
            res.tilt(x, y);
        }
        if let Some(r) = axes.rotation {
            res.rotation(r);
        }
        if let Some(s) = axes.slider {
            res.slider((s.clamp(-1.0, 1.0) * 65535.0).round() as i32);
        }
        if let Some((degrees, clicks)) = axes.wheel {
            res.wheel(degrees, clicks);
        }
    }

    pub fn tool_tip(&self, tool: u64, down: bool, serial: u32) {
        if let Some(res) = self.focused_tool(tool) {
            if down {
                res.down(serial);
            } else {
                res.up();
            }
        }
    }

    pub fn tool_button(&self, tool: u64, button: u32, pressed: bool, serial: u32) {
        if let Some(res) = self.focused_tool(tool) {
            let state = if pressed { zwp_tablet_tool_v2::ButtonState::Pressed } else { zwp_tablet_tool_v2::ButtonState::Released };
            res.button(serial, button, state);
        }
    }

    pub fn tool_frame(&self, tool: u64, time: u32) {
        if let Some(res) = self.focused_tool(tool) {
            res.frame(time);
        }
    }

    /// The pad's events go to `surface` (the keyboard focus) from now on:
    /// `leave` and `enter` as it changes, then each group's mode.
    pub fn pad_focus(&mut self, pad: &str, surface: Option<&WlSurface>, serial: u32) {
        if self.pad_focus.get(pad) == surface {
            return;
        }
        if let Some(old) = self.pad_focus.remove(pad) {
            if let Some(p) = self.pad_res.iter().find(|p| p.pad == pad && same_client(&p.res, &old)) {
                p.res.leave(serial, &old);
            }
        }
        let Some(surface) = surface else { return };
        let Some(tablet) = self.pads.iter().find(|p| p.id == pad).map(|p| p.tablet.clone()) else { return };
        let Some(tablet_res) = self.tablet_res_for(&tablet, surface).cloned() else { return };
        let modes: Vec<u32> = self.pads.iter().find(|p| p.id == pad).map(|p| p.groups.iter().map(|g| g.mode).collect()).unwrap_or_default();
        let Some(p) = self.pad_res.iter().find(|p| p.pad == pad && same_client(&p.res, surface)) else { return };
        p.res.enter(serial, &tablet_res, surface);
        for (group, mode) in p.groups.iter().zip(modes) {
            group.res.mode_switch(0, serial, mode);
        }
        self.pad_focus.insert(pad.to_owned(), surface.clone());
    }

    fn focused_pad(&self, pad: &str) -> Option<&PadRes> {
        let surface = self.pad_focus.get(pad)?;
        self.pad_res.iter().find(|p| p.pad == pad && same_client(&p.res, surface))
    }

    pub fn pad_button(&self, pad: &str, time: u32, button: u32, pressed: bool) {
        if let Some(p) = self.focused_pad(pad) {
            let state = if pressed { zwp_tablet_pad_v2::ButtonState::Pressed } else { zwp_tablet_pad_v2::ButtonState::Released };
            p.res.button(time, button, state);
        }
    }

    /// Ring `ring` (the pad's numbering) at `angle` degrees, or let go.
    pub fn pad_ring(&self, pad: &str, ring: u32, angle: Option<f64>, finger: bool, time: u32) {
        let Some(p) = self.focused_pad(pad) else { return };
        for (_, res) in p.groups.iter().flat_map(|g| g.rings.iter()).filter(|(n, _)| *n == ring) {
            if finger {
                res.source(zwp_tablet_pad_ring_v2::Source::Finger);
            }
            match angle {
                Some(a) => res.angle(a),
                None => res.stop(),
            }
            res.frame(time);
        }
    }

    /// Strip `strip` (the pad's numbering) at `position` (0–1), or let go.
    pub fn pad_strip(&self, pad: &str, strip: u32, position: Option<f64>, finger: bool, time: u32) {
        let Some(p) = self.focused_pad(pad) else { return };
        for (_, res) in p.groups.iter().flat_map(|g| g.strips.iter()).filter(|(n, _)| *n == strip) {
            if finger {
                res.source(zwp_tablet_pad_strip_v2::Source::Finger);
            }
            match position {
                Some(v) => res.position(crate::tablet::strip_value(v)),
                None => res.stop(),
            }
            res.frame(time);
        }
    }

    /// Group `group` of the pad switched to `mode`.
    pub fn pad_mode(&mut self, pad: &str, group: usize, mode: u32, time: u32, serial: u32) {
        let Some(info) = self.pads.iter_mut().find(|p| p.id == pad) else { return };
        let Some(g) = info.groups.get_mut(group) else { return };
        if g.mode == mode {
            return;
        }
        g.mode = mode;
        if let Some(p) = self.focused_pad(pad) {
            if let Some(g) = p.groups.get(group) {
                g.res.mode_switch(time, serial, mode);
            }
        }
    }

    /// A client went away: forget what it had.
    fn forget(&mut self, id: &smithay::reexports::wayland_server::backend::ObjectId) {
        self.seats.retain(|s| Resource::id(s) != *id);
        self.tablet_res.retain(|(_, r)| Resource::id(r) != *id);
        self.tool_res.retain(|(_, r)| Resource::id(r) != *id);
        self.pad_res.retain(|p| Resource::id(&p.res) != *id);
        for p in &mut self.pad_res {
            p.groups.retain(|g| Resource::id(&g.res) != *id);
            for g in &mut p.groups {
                g.rings.retain(|(_, r)| Resource::id(r) != *id);
                g.strips.retain(|(_, s)| Resource::id(s) != *id);
            }
        }
    }
}

/// The dispatch implementations a compositor's state needs.
pub trait TabletV2Dispatch:
    TabletV2Handler
    + GlobalDispatch<ZwpTabletManagerV2, ()>
    + Dispatch<ZwpTabletManagerV2, ()>
    + Dispatch<ZwpTabletSeatV2, ()>
    + Dispatch<ZwpTabletV2, String>
    + Dispatch<ZwpTabletToolV2, u64>
    + Dispatch<ZwpTabletPadV2, String>
    + Dispatch<ZwpTabletPadGroupV2, (String, usize)>
    + Dispatch<ZwpTabletPadRingV2, (String, u32)>
    + Dispatch<ZwpTabletPadStripV2, (String, u32)>
{
}

impl<T> TabletV2Dispatch for T where
    T: TabletV2Handler
        + GlobalDispatch<ZwpTabletManagerV2, ()>
        + Dispatch<ZwpTabletManagerV2, ()>
        + Dispatch<ZwpTabletSeatV2, ()>
        + Dispatch<ZwpTabletV2, String>
        + Dispatch<ZwpTabletToolV2, u64>
        + Dispatch<ZwpTabletPadV2, String>
        + Dispatch<ZwpTabletPadGroupV2, (String, usize)>
        + Dispatch<ZwpTabletPadRingV2, (String, u32)>
        + Dispatch<ZwpTabletPadStripV2, (String, u32)>
{
}

impl<D: TabletV2Dispatch> GlobalDispatch<ZwpTabletManagerV2, (), D> for TabletV2 {
    fn bind(_: &mut D, _: &DisplayHandle, _: &Client, resource: New<ZwpTabletManagerV2>, _: &(), data_init: &mut DataInit<'_, D>) {
        data_init.init(resource, ());
    }
}

impl<D: TabletV2Dispatch> Dispatch<ZwpTabletManagerV2, (), D> for TabletV2 {
    fn request(state: &mut D, _: &Client, _: &ZwpTabletManagerV2, request: zwp_tablet_manager_v2::Request, _: &(), dh: &DisplayHandle, data_init: &mut DataInit<'_, D>) {
        match request {
            zwp_tablet_manager_v2::Request::GetTabletSeat { tablet_seat, .. } => {
                let seat = data_init.init(tablet_seat, ());
                let tablet = state.tablet_v2();
                tablet.seats.push(seat.clone());
                tablet.announce_all::<D>(dh, &seat);
            }
            zwp_tablet_manager_v2::Request::Destroy => {}
            _ => {}
        }
    }
}

macro_rules! forgetting {
    ($iface:ty, $data:ty) => {
        impl<D: TabletV2Dispatch> Dispatch<$iface, $data, D> for TabletV2 {
            fn request(_: &mut D, _: &Client, _: &$iface, _: <$iface as Resource>::Request, _: &$data, _: &DisplayHandle, _: &mut DataInit<'_, D>) {}

            fn destroyed(state: &mut D, _: ClientId, resource: &$iface, _: &$data) {
                state.tablet_v2().forget(&Resource::id(resource));
            }
        }
    };
}

forgetting!(ZwpTabletSeatV2, ());
forgetting!(ZwpTabletV2, String);
forgetting!(ZwpTabletPadV2, String);
forgetting!(ZwpTabletPadGroupV2, (String, usize));
forgetting!(ZwpTabletPadRingV2, (String, u32));
forgetting!(ZwpTabletPadStripV2, (String, u32));

impl<D: TabletV2Dispatch> Dispatch<ZwpTabletToolV2, u64, D> for TabletV2 {
    fn request(state: &mut D, _: &Client, _: &ZwpTabletToolV2, request: zwp_tablet_tool_v2::Request, tool: &u64, _: &DisplayHandle, _: &mut DataInit<'_, D>) {
        if let zwp_tablet_tool_v2::Request::SetCursor { surface, hotspot_x, hotspot_y, .. } = request {
            state.tool_cursor(*tool, surface.map(|s| (s, (hotspot_x, hotspot_y))));
        }
    }

    fn destroyed(state: &mut D, _: ClientId, resource: &ZwpTabletToolV2, _: &u64) {
        state.tablet_v2().forget(&Resource::id(resource));
    }
}
