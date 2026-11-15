//! Users' pictures (AccountsService `IconFile`): PNG or JPEG, shrunk for
//! the list's and the login's round avatars.

use std::path::Path;

/// Larger files are not read.
const MAX_BYTES: u64 = 4 << 20;
/// The longest side kept (the login avatar is 72 logical pixels; this
/// leaves room for scale 2).
const MAX_SIDE: usize = 160;

/// Straight (not premultiplied) RGBA.
struct Rgba {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

fn decode_png(data: &[u8]) -> Option<Rgba> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    to_rgba(info.width as usize, info.height as usize, &buf, info.color_type.samples())
}

fn decode_jpeg(data: &[u8]) -> Option<Rgba> {
    let options = zune_jpeg::zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(data), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    to_rgba(usize::from(info.width), usize::from(info.height), &pixels, 3)
}

fn to_rgba(width: usize, height: usize, data: &[u8], samples: usize) -> Option<Rgba> {
    if width == 0 || height == 0 || data.len() < width * height * samples {
        return None;
    }
    let mut pixels = Vec::with_capacity(width * height * 4);
    for px in data.chunks_exact(samples).take(width * height) {
        match samples {
            1 => pixels.extend_from_slice(&[px[0], px[0], px[0], 255]),
            2 => pixels.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            3 => pixels.extend_from_slice(&[px[0], px[1], px[2], 255]),
            _ => pixels.extend_from_slice(&px[..4]),
        }
    }
    Some(Rgba { width, height, pixels })
}

/// `image` shrunk (box average) to fit `max`×`max`, premultiplied.
fn shrink(image: &Rgba, max: usize) -> helix_draw::Image {
    let (w, h) = (image.width, image.height);
    let k = (max as f64 / w as f64).min(max as f64 / h as f64).min(1.0);
    let (tw, th) = (((w as f64 * k).round() as usize).max(1), ((h as f64 * k).round() as usize).max(1));
    let mut rgba = Vec::with_capacity(tw * th * 4);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    let a = u64::from(image.pixels[i + 3]);
                    for (c, total) in sum.iter_mut().take(3).enumerate() {
                        *total += u64::from(image.pixels[i + c]) * a / 255;
                    }
                    sum[3] += a;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)).max(1) as u64;
            rgba.extend(sum.iter().map(|v| (v / n) as u8));
        }
    }
    helix_draw::Image { width: tw, height: th, rgba }
}

/// The picture at `path`, ready to draw; `None` when it is missing, too
/// large or not a PNG or JPEG.
pub fn load(path: &Path) -> Option<helix_draw::Image> {
    let size = std::fs::metadata(path).ok()?.len();
    if size == 0 || size > MAX_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let decoded = if data.starts_with(b"\x89PNG") {
        decode_png(&data)
    } else if data.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(&data)
    } else {
        None
    };
    match decoded {
        Some(image) => Some(shrink(&image, MAX_SIDE)),
        None => {
            tracing::debug!(path = %path.display(), "cannot read the user's picture");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_pictures() {
        let dir = std::env::temp_dir().join(format!("helix-greeter-picture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ann");
        {
            let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            let mut encoder = png::Encoder::new(file, 400, 200);
            encoder.set_color(png::ColorType::Rgba);
            encoder.write_header().unwrap().write_image_data(&[255, 0, 0, 255].repeat(400 * 200)).unwrap();
        }
        let picture = load(&path).expect("decoded");
        assert_eq!((picture.width, picture.height), (MAX_SIDE, MAX_SIDE / 2), "shrunk, keeping its shape");
        assert_eq!(&picture.rgba[..4], &[255, 0, 0, 255]);
        std::fs::write(dir.join("text"), b"not a picture").unwrap();
        assert!(load(&dir.join("text")).is_none());
        assert!(load(&dir.join("missing")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
