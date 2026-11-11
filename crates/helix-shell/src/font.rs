//! A tiny 5x7 bitmap font: enough for a clock and percentages, without
//! pulling in a font stack.

pub const GLYPH_W: usize = 5;
pub const GLYPH_H: usize = 7;

/// Rows of a glyph, top to bottom; the low 5 bits are pixels, MSB = left.
fn glyph(c: char) -> Option<[u8; GLYPH_H]> {
    Some(match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000],
        '%' => [0b11000, 0b11001, 0b00010, 0b00100, 0b01000, 0b10011, 0b00011],
        ' ' => [0; GLYPH_H],
        _ => return None,
    })
}

/// Width in pixels of `text` at `scale`, with one blank column between
/// glyphs.
pub fn text_width(text: &str, scale: usize) -> usize {
    let n = text.chars().filter(|c| glyph(*c).is_some()).count();
    if n == 0 {
        0
    } else {
        (n * (GLYPH_W + 1) - 1) * scale
    }
}

/// Call `plot(x, y)` for every lit pixel of `text` drawn at `scale`.
pub fn render(text: &str, scale: usize, mut plot: impl FnMut(usize, usize)) {
    let mut x0 = 0;
    for rows in text.chars().filter_map(glyph) {
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_W {
                if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            plot(x0 + col * scale + dx, row * scale + dy);
                        }
                    }
                }
            }
        }
        x0 += (GLYPH_W + 1) * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_and_pixels() {
        assert_eq!(text_width("12:34", 1), 5 * 6 - 1);
        assert_eq!(text_width("12:34", 2), (5 * 6 - 1) * 2);
        assert_eq!(text_width("", 2), 0);
        let mut pixels = Vec::new();
        render("1", 1, |x, y| pixels.push((x, y)));
        // The "1" glyph has 3 lit pixels in its bottom row.
        assert_eq!(pixels.iter().filter(|(_, y)| *y == 6).count(), 3);
        assert!(pixels.iter().all(|(x, y)| *x < GLYPH_W && *y < GLYPH_H));
    }
}
