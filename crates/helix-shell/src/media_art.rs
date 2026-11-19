//! Album art for the lock screen's media card (helixdesktop/spec
//! `contracts/dbus/org.gnome.ScreenSaver.md`, "Lock screen (Helix)"):
//! the picture an MPRIS player names in `mpris:artUrl`, read off the UI
//! thread with limits, decoded and shrunk.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// Art larger than this is not read.
const MAX_BYTES: u64 = 8 << 20;
/// How long a download may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Art is kept this large at most (pixels): the card's 84 px at scale 2.
const ART_PX: u32 = 168;

/// The bytes at `url`: a `file://` path or an `http(s)://` address (at
/// most [`MAX_BYTES`], within [`TIMEOUT`]).
fn read(url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = crate::wallpaper::uri_path(url).filter(|_| url.starts_with("file://")) {
        return crate::wallpaper::read_regular(&path, MAX_BYTES);
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("not a file or web address".into());
    }
    let response = ureq::AgentBuilder::new().timeout(TIMEOUT).user_agent(concat!("helix-desktop/", env!("CARGO_PKG_VERSION"))).build().get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response.into_reader().take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("too large".into());
    }
    Ok(bytes)
}

/// The art at `url`, decoded and shrunk; `None` when it cannot be had
/// (the card shows the player's icon instead). Blocks: call it off the
/// UI thread.
pub fn fetch(url: &str) -> Option<helix_draw::Image> {
    let decoded = read(url).and_then(|bytes| crate::wallpaper::decode_bytes_within(&bytes, Path::new("art"), crate::wallpaper::Limits::ART));
    match decoded {
        Ok(image) => Some(crate::wallpaper::thumbnail(&image, ART_PX, ART_PX)),
        Err(err) => {
            tracing::debug!(url, %err, "no album art");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_art() {
        let dir = std::env::temp_dir().join(format!("helix-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cover.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 400, 400);
            encoder.set_color(png::ColorType::Rgb);
            encoder.write_header().unwrap().write_image_data(&[90u8; 400 * 400 * 3]).unwrap();
        }
        let art = fetch(&format!("file://{}", path.display())).expect("decoded");
        assert_eq!((art.width, art.height), (ART_PX as usize, ART_PX as usize), "shrunk");
        assert_eq!(fetch(&format!("file://{}", dir.join("none.png").display())), None);
        assert_eq!(fetch("ftp://example.invalid/a.png"), None, "only files and the web");
        // Larger than allowed: refused from the header.
        let small = crate::wallpaper::Limits { side: 100, pixels: 100 * 100, bytes: MAX_BYTES };
        let err = crate::wallpaper::decode_bytes_within(&std::fs::read(&path).unwrap(), &path, small).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // Devices and FIFOs are not read (a FIFO without a writer would
        // block, /dev/zero never ends).
        let fifo = dir.join("fifo.png");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(fetch(&format!("file://{}", fifo.display())), None);
        assert_eq!(fetch("file:///dev/zero"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
