//! `wp_cursor_shape_manager_v1` (helix-desktop-spec
//! `contracts/wayland/README.md`, "Cursor shapes"): apps name a cursor
//! shape instead of drawing one, for the pointer and for tablet tools
//! (Helix's own tablet protocol objects, `tablet_v2`).

use smithay::input::pointer::CursorIcon;
use smithay::reexports::wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_device_v1::{self, Shape, WpCursorShapeDeviceV1};
use smithay::reexports::wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_manager_v1::{self, WpCursorShapeManagerV1};
use smithay::reexports::wayland_protocols::wp::tablet::zv2::server::zwp_tablet_tool_v2::ZwpTabletToolV2;
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak};

/// What the compositor does with a shape.
pub trait CursorShapeHandler: 'static {
    /// `client` asks for `icon` as the pointer's cursor.
    fn pointer_shape(&mut self, client: ClientId, icon: CursorIcon);
    /// `client` asks for `icon` as tablet tool `tool`'s cursor.
    fn tool_shape(&mut self, client: ClientId, tool: u64, icon: CursorIcon);
}

/// The global.
#[derive(Debug)]
pub struct CursorShapeManager {
    _global: GlobalId,
}

/// What a shape device belongs to.
#[derive(Debug, Clone)]
pub enum Device {
    Pointer,
    /// A tablet tool (Helix's tool id), while its object lives.
    Tool(Weak<ZwpTabletToolV2>),
}

impl CursorShapeManager {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpCursorShapeManagerV1, ()> + Dispatch<WpCursorShapeManagerV1, ()> + Dispatch<WpCursorShapeDeviceV1, Device> + 'static,
    {
        Self { _global: dh.create_global::<D, WpCursorShapeManagerV1, ()>(2, ()) }
    }
}

/// The cursor for a protocol shape (names as in the CSS cursor list).
pub fn icon(shape: Shape) -> Option<CursorIcon> {
    use CursorIcon as C;
    Some(match shape {
        Shape::Default => C::Default,
        Shape::ContextMenu => C::ContextMenu,
        Shape::Help => C::Help,
        Shape::Pointer => C::Pointer,
        Shape::Progress => C::Progress,
        Shape::Wait => C::Wait,
        Shape::Cell => C::Cell,
        Shape::Crosshair => C::Crosshair,
        Shape::Text => C::Text,
        Shape::VerticalText => C::VerticalText,
        Shape::Alias => C::Alias,
        Shape::Copy => C::Copy,
        Shape::Move => C::Move,
        Shape::NoDrop => C::NoDrop,
        Shape::NotAllowed => C::NotAllowed,
        Shape::Grab => C::Grab,
        Shape::Grabbing => C::Grabbing,
        Shape::EResize => C::EResize,
        Shape::NResize => C::NResize,
        Shape::NeResize => C::NeResize,
        Shape::NwResize => C::NwResize,
        Shape::SResize => C::SResize,
        Shape::SeResize => C::SeResize,
        Shape::SwResize => C::SwResize,
        Shape::WResize => C::WResize,
        Shape::EwResize => C::EwResize,
        Shape::NsResize => C::NsResize,
        Shape::NeswResize => C::NeswResize,
        Shape::NwseResize => C::NwseResize,
        Shape::ColResize => C::ColResize,
        Shape::RowResize => C::RowResize,
        Shape::AllScroll => C::AllScroll,
        Shape::ZoomIn => C::ZoomIn,
        Shape::ZoomOut => C::ZoomOut,
        Shape::DndAsk => C::DndAsk,
        Shape::AllResize => C::AllResize,
        _ => return None,
    })
}

impl<D> GlobalDispatch<WpCursorShapeManagerV1, (), D> for CursorShapeManager
where
    D: Dispatch<WpCursorShapeManagerV1, ()> + 'static,
{
    fn bind(_state: &mut D, _dh: &DisplayHandle, _client: &Client, resource: New<WpCursorShapeManagerV1>, _data: &(), init: &mut DataInit<'_, D>) {
        init.init(resource, ());
    }
}

impl<D> Dispatch<WpCursorShapeManagerV1, (), D> for CursorShapeManager
where
    D: Dispatch<WpCursorShapeDeviceV1, Device> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &WpCursorShapeManagerV1,
        request: wp_cursor_shape_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_cursor_shape_manager_v1::Request::GetPointer { cursor_shape_device, .. } => {
                init.init(cursor_shape_device, Device::Pointer);
            }
            wp_cursor_shape_manager_v1::Request::GetTabletToolV2 { cursor_shape_device, tablet_tool } => {
                init.init(cursor_shape_device, Device::Tool(tablet_tool.downgrade()));
            }
            _ => {}
        }
    }
}

impl<D> Dispatch<WpCursorShapeDeviceV1, Device, D> for CursorShapeManager
where
    D: CursorShapeHandler,
{
    fn request(
        state: &mut D,
        client: &Client,
        resource: &WpCursorShapeDeviceV1,
        request: wp_cursor_shape_device_v1::Request,
        data: &Device,
        _dh: &DisplayHandle,
        _init: &mut DataInit<'_, D>,
    ) {
        let wp_cursor_shape_device_v1::Request::SetShape { shape, .. } = request else { return };
        let Some(icon) = (match shape {
            WEnum::Value(shape) => icon(shape),
            WEnum::Unknown(_) => None,
        }) else {
            resource.post_error(wp_cursor_shape_device_v1::Error::InvalidShape, "unknown shape");
            return;
        };
        match data {
            Device::Pointer => state.pointer_shape(client.id(), icon),
            Device::Tool(tool) => {
                // An inert device once its tool object is gone.
                let Some(tool) = tool.upgrade().ok() else { return };
                if let Some(id) = tool.data::<u64>() {
                    state.tool_shape(client.id(), *id, icon);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(icon(Shape::Text), Some(CursorIcon::Text));
        assert_eq!(icon(Shape::NwseResize), Some(CursorIcon::NwseResize));
        assert_eq!(icon(Shape::AllResize), Some(CursorIcon::AllResize));
    }
}
