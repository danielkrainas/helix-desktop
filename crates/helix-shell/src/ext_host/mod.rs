//! Helix's own extensions (helix-desktop-spec `contracts/extensions/`):
//! finding them, running each in its own process with a private D-Bus
//! connection, and following the items they show.

pub mod manifest;
pub mod store;
pub mod manager;
pub mod runner;

pub use runner::{Action, ExtHost, Info, MenuEntry, PanelItemView, SliderView, Snapshot, State, ToggleView};
