//! The compositor's own pointer image, used on bare hardware when a client
//! has not set a cursor surface. (Nested, the host window shows the cursor.)

/// A left-pointing arrow; `#` is the outline, `.` the fill, space is clear.
/// The hotspot is the top-left pixel.
const ARROW: &[&str] = &[
    "#",
    "##",
    "#.#",
    "#..#",
    "#...#",
    "#....#",
    "#.....#",
    "#......#",
    "#.......#",
    "#........#",
    "#.........#",
    "#..........#",
    "#......#####",
    "#...#..#",
    "#..##..#",
    "#.#  #..#",
    "##   #..#",
    "#     #..#",
    "      #..#",
    "       ##",
];

/// Scale of the arrow in pixels per art cell.
pub const SCALE: usize = 1;

/// The arrow as little-endian ARGB8888 (B, G, R, A per pixel), with its size.
pub fn arrow() -> (Vec<u8>, usize, usize) {
    let width = ARROW.iter().map(|row| row.len()).max().unwrap_or(0) * SCALE;
    let height = ARROW.len() * SCALE;
    let mut pixels = vec![0u8; width * height * 4];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, cell) in row.bytes().enumerate() {
            let bgra = match cell {
                b'#' => [0x00, 0x00, 0x00, 0xFF],
                b'.' => [0xFF, 0xFF, 0xFF, 0xFF],
                _ => continue,
            };
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let i = ((y * SCALE + dy) * width + x * SCALE + dx) * 4;
                    pixels[i..i + 4].copy_from_slice(&bgra);
                }
            }
        }
    }
    (pixels, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_shape() {
        let (pixels, w, h) = arrow();
        assert_eq!((w, h), (12, 20));
        assert_eq!(pixels.len(), w * h * 4);
        // Hotspot (0,0) is opaque outline; the far top-right is transparent.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0xFF]);
        assert_eq!(pixels[(w - 1) * 4 + 3], 0);
        // There is white fill inside.
        let white = pixels.as_chunks::<4>().0.iter().filter(|p| **p == [0xFF; 4]).count();
        assert!(white > 30);
    }
}
