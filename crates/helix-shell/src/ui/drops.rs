//! Files dropped on the dock (helix-desktop-spec
//! `contracts/extensions/README.md`, "Dock items"): extensions' dock
//! items with `accepts-drops` take `text/uri-list`.

use std::io::Read;

use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device::WlDataDevice;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;

use super::*;
use crate::dock::Item;

const URI_LIST: &str = "text/uri-list";
/// The most read from a drop.
const DROP_MAX: u64 = 1 << 20;

/// `text/uri-list`: one URI per line, `#` lines are comments.
pub fn parse_uri_list(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect()
}

impl Ui {
    /// The extension dock item (index into the extensions' dock items)
    /// under `x`, `y` on `surface` that takes drops now.
    fn drop_target(&self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> Option<usize> {
        match self.dock_item_at(surface, x, y) {
            Some(Item::Ext(e)) if self.ext.dock.get(e).is_some_and(|d| d.accepts_drops && d.running) => Some(e),
            _ => None,
        }
    }

    /// Accept the drag where it is, or refuse it.
    fn update_drop(&mut self, device: &WlDataDevice) {
        let Some(offer) = self.data_device.as_ref().filter(|d| d.inner() == device).and_then(|d| d.data().drag_offer()) else { return };
        let uris = offer.with_mime_types(|m| m.iter().any(|t| t == URI_LIST));
        let target = if uris { self.drop_target(&offer.surface, offer.x, offer.y) } else { None };
        if target.is_some() {
            offer.accept_mime_type(offer.serial, Some(URI_LIST.into()));
            offer.set_actions(DndAction::Copy, DndAction::Copy);
        } else {
            offer.accept_mime_type(offer.serial, None);
            offer.set_actions(DndAction::empty(), DndAction::empty());
        }
        self.drop_at = target;
    }

    fn take_drop(&mut self, offer: DragOffer, target: usize) {
        let Some(full_id) = self.ext.dock.get(target).map(|d| d.full_id.clone()) else { return };
        let pipe = match offer.receive(URI_LIST.into()) {
            Ok(pipe) => pipe,
            Err(err) => {
                tracing::warn!(%err, "cannot read the drop");
                return;
            }
        };
        let Some(handle) = self.loop_handle.clone() else { return };
        // Read it off the UI thread; finish the offer back on it.
        let (tx, rx) = channel::channel::<Vec<String>>();
        let _ = handle.insert_source(rx, move |event, _, ui: &mut Ui| {
            if let channel::Event::Msg(uris) = event {
                offer.finish();
                tracing::info!(item = full_id, count = uris.len(), "files dropped on an extension's dock item");
                if !uris.is_empty() {
                    let _ = ui.requests.send(UiRequest::ExtAction(full_id.clone(), crate::ext_host::Action::DockDrop(uris)));
                }
            }
        });
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.take(DROP_MAX).read_to_string(&mut text);
            let _ = tx.send(parse_uri_list(&text));
        });
    }
}

impl DataDeviceHandler for Ui {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {
        self.update_drop(device);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.drop_at = None;
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice, _: f64, _: f64) {
        self.update_drop(device);
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        let offer = self.data_device.as_ref().filter(|d| d.inner() == device).and_then(|d| d.data().drag_offer());
        match (offer, self.drop_at.take()) {
            (Some(offer), Some(target)) => self.take_drop(offer, target),
            (Some(offer), None) => offer.finish(),
            _ => {}
        }
    }
}

impl DataOfferHandler for Ui {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}

    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

/// The shell offers no data of its own.
impl DataSourceHandler for Ui {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}

    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: String, _: WritePipe) {}

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

/// The data device, made when the seat has a pointer.
pub(super) type Device = DataDevice;

#[cfg(test)]
mod tests {
    #[test]
    fn uri_lists() {
        assert_eq!(super::parse_uri_list("# comment\r\nfile:///a\r\n\r\nfile:///b c\n"), ["file:///a", "file:///b c"]);
    }
}
