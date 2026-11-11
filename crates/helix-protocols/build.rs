//! The protocol XML is read by `wayland_scanner` macros, which cargo does
//! not track: rebuild when it changes.
fn main() {
    println!("cargo:rerun-if-changed=protocols");
}
