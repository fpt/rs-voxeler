//! Flat 2D drawing on top of the rendered image: the HUD, the palette strip,
//! the cursor. Depth is ignored — the overlay is always on top by definition.
//!
//! The font is a 5×7 bitmap held in this file. A real text stack would be a
//! large dependency for what amounts to a status line, and an embedded table
//! means the editor's UI cannot break because a font is missing from the system
//! it happens to be running on.

use crate::framebuffer::Framebuffer;

/// Glyph cell size, before scaling.
pub const GLYPH_W: u32 = 5;
pub const GLYPH_H: u32 = 7;
/// One blank column between glyphs, so text at scale 1 is 6px per character.
pub const ADVANCE: u32 = GLYPH_W + 1;

/// The width in pixels a string will occupy at `scale`.
pub fn text_width(s: &str, scale: u32) -> u32 {
    (s.chars().count() as u32 * ADVANCE).saturating_sub(1) * scale
}

pub fn text_height(scale: u32) -> u32 {
    GLYPH_H * scale
}

/// Draw `s` with its top-left at `(x, y)`.
///
/// Lowercase is folded to uppercase and anything outside the table becomes a
/// blank: the table covers 0x20–0x5F, which is every character a status line
/// built from file names and numbers actually uses.
pub fn text(fb: &mut Framebuffer, x: i32, y: i32, s: &str, color: u32, scale: u32) {
    let scale = scale.max(1);
    let mut cx = x;
    for ch in s.chars() {
        glyph(fb, cx, y, ch, color, scale);
        cx += (ADVANCE * scale) as i32;
    }
}

/// Draw `s` with a one-pixel drop shadow, so it stays readable over both the
/// bright sky and the dark model.
pub fn text_shadowed(fb: &mut Framebuffer, x: i32, y: i32, s: &str, color: u32, scale: u32) {
    text(fb, x + scale as i32, y + scale as i32, s, 0x000000, scale);
    text(fb, x, y, s, color, scale);
}

fn glyph(fb: &mut Framebuffer, x: i32, y: i32, ch: char, color: u32, scale: u32) {
    let ch = ch.to_ascii_uppercase() as u32;
    if !(0x20..0x60).contains(&ch) {
        return;
    }
    let cols = &FONT_5X7[(ch - 0x20) as usize];
    for (col, bits) in cols.iter().enumerate() {
        for row in 0..GLYPH_H {
            if bits & (1 << row) == 0 {
                continue;
            }
            let px = x + (col as u32 * scale) as i32;
            let py = y + (row * scale) as i32;
            fill_rect(fb, px, py, scale, scale, color);
        }
    }
}

/// Filled rectangle, clipped to the framebuffer. Negative coordinates are
/// handled by the clip rather than by the caller.
pub fn fill_rect(fb: &mut Framebuffer, x: i32, y: i32, w: u32, h: u32, color: u32) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w as i32).max(0) as u32).min(fb.width());
    let y1 = ((y + h as i32).max(0) as u32).min(fb.height());
    for py in y0..y1 {
        for px in x0..x1 {
            fb.set(px, py, color);
        }
    }
}

/// One-pixel rectangle outline.
pub fn stroke_rect(fb: &mut Framebuffer, x: i32, y: i32, w: u32, h: u32, color: u32) {
    if w == 0 || h == 0 {
        return;
    }
    fill_rect(fb, x, y, w, 1, color);
    fill_rect(fb, x, y + h as i32 - 1, w, 1, color);
    fill_rect(fb, x, y, 1, h, color);
    fill_rect(fb, x + w as i32 - 1, y, 1, h, color);
}

/// A translucent rectangle, for panel backgrounds. `alpha` is 0–255.
///
/// Blended here rather than kept as a separate layer because the overlay is
/// drawn once, last, straight into the colour buffer — there is nothing to
/// composite against later.
pub fn blend_rect(fb: &mut Framebuffer, x: i32, y: i32, w: u32, h: u32, color: u32, alpha: u8) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w as i32).max(0) as u32).min(fb.width());
    let y1 = ((y + h as i32).max(0) as u32).min(fb.height());
    let a = alpha as u32;
    for py in y0..y1 {
        for px in x0..x1 {
            let dst = fb.color_at(px, py);
            let ch = |shift: u32| {
                let s = (color >> shift) & 0xFF;
                let d = (dst >> shift) & 0xFF;
                (s * a + d * (255 - a)) / 255
            };
            fb.set(px, py, ch(16) << 16 | ch(8) << 8 | ch(0));
        }
    }
}

/// 5×7 glyphs for ASCII 0x20–0x5F, one byte per column, bit 0 the top row.
#[rustfmt::skip]
const FONT_5X7: [[u8; 5]; 64] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], // space
    [0x00, 0x00, 0x5F, 0x00, 0x00], // !
    [0x00, 0x07, 0x00, 0x07, 0x00], // "
    [0x14, 0x7F, 0x14, 0x7F, 0x14], // #
    [0x24, 0x2A, 0x7F, 0x2A, 0x12], // $
    [0x23, 0x13, 0x08, 0x64, 0x62], // %
    [0x36, 0x49, 0x55, 0x22, 0x50], // &
    [0x00, 0x05, 0x03, 0x00, 0x00], // '
    [0x00, 0x1C, 0x22, 0x41, 0x00], // (
    [0x00, 0x41, 0x22, 0x1C, 0x00], // )
    [0x14, 0x08, 0x3E, 0x08, 0x14], // *
    [0x08, 0x08, 0x3E, 0x08, 0x08], // +
    [0x00, 0x50, 0x30, 0x00, 0x00], // ,
    [0x08, 0x08, 0x08, 0x08, 0x08], // -
    [0x00, 0x60, 0x60, 0x00, 0x00], // .
    [0x20, 0x10, 0x08, 0x04, 0x02], // /
    [0x3E, 0x51, 0x49, 0x45, 0x3E], // 0
    [0x00, 0x42, 0x7F, 0x40, 0x00], // 1
    [0x42, 0x61, 0x51, 0x49, 0x46], // 2
    [0x21, 0x41, 0x45, 0x4B, 0x31], // 3
    [0x18, 0x14, 0x12, 0x7F, 0x10], // 4
    [0x27, 0x45, 0x45, 0x45, 0x39], // 5
    [0x3C, 0x4A, 0x49, 0x49, 0x30], // 6
    [0x01, 0x71, 0x09, 0x05, 0x03], // 7
    [0x36, 0x49, 0x49, 0x49, 0x36], // 8
    [0x06, 0x49, 0x49, 0x29, 0x1E], // 9
    [0x00, 0x36, 0x36, 0x00, 0x00], // :
    [0x00, 0x56, 0x36, 0x00, 0x00], // ;
    [0x00, 0x08, 0x14, 0x22, 0x41], // <
    [0x14, 0x14, 0x14, 0x14, 0x14], // =
    [0x41, 0x22, 0x14, 0x08, 0x00], // >
    [0x02, 0x01, 0x51, 0x09, 0x06], // ?
    [0x32, 0x49, 0x79, 0x41, 0x3E], // @
    [0x7E, 0x11, 0x11, 0x11, 0x7E], // A
    [0x7F, 0x49, 0x49, 0x49, 0x36], // B
    [0x3E, 0x41, 0x41, 0x41, 0x22], // C
    [0x7F, 0x41, 0x41, 0x22, 0x1C], // D
    [0x7F, 0x49, 0x49, 0x49, 0x41], // E
    [0x7F, 0x09, 0x09, 0x01, 0x01], // F
    [0x3E, 0x41, 0x41, 0x51, 0x32], // G
    [0x7F, 0x08, 0x08, 0x08, 0x7F], // H
    [0x00, 0x41, 0x7F, 0x41, 0x00], // I
    [0x20, 0x40, 0x41, 0x3F, 0x01], // J
    [0x7F, 0x08, 0x14, 0x22, 0x41], // K
    [0x7F, 0x40, 0x40, 0x40, 0x40], // L
    [0x7F, 0x02, 0x04, 0x02, 0x7F], // M
    [0x7F, 0x04, 0x08, 0x10, 0x7F], // N
    [0x3E, 0x41, 0x41, 0x41, 0x3E], // O
    [0x7F, 0x09, 0x09, 0x09, 0x06], // P
    [0x3E, 0x41, 0x51, 0x21, 0x5E], // Q
    [0x7F, 0x09, 0x19, 0x29, 0x46], // R
    [0x46, 0x49, 0x49, 0x49, 0x31], // S
    [0x01, 0x01, 0x7F, 0x01, 0x01], // T
    [0x3F, 0x40, 0x40, 0x40, 0x3F], // U
    [0x1F, 0x20, 0x40, 0x20, 0x1F], // V
    [0x7F, 0x20, 0x18, 0x20, 0x7F], // W
    [0x63, 0x14, 0x08, 0x14, 0x63], // X
    [0x03, 0x04, 0x78, 0x04, 0x03], // Y
    [0x61, 0x51, 0x49, 0x45, 0x43], // Z
    [0x00, 0x00, 0x7F, 0x41, 0x41], // [
    [0x02, 0x04, 0x08, 0x10, 0x20], // \
    [0x41, 0x41, 0x7F, 0x00, 0x00], // ]
    [0x04, 0x02, 0x01, 0x02, 0x04], // ^
    [0x40, 0x40, 0x40, 0x40, 0x40], // _
];

#[cfg(test)]
mod tests {
    use super::*;

    fn lit_pixels(fb: &Framebuffer) -> usize {
        (0..fb.height())
            .flat_map(|y| (0..fb.width()).map(move |x| (x, y)))
            .filter(|(x, y)| fb.color_at(*x, *y) != 0)
            .count()
    }

    #[test]
    fn text_draws_something_and_a_space_draws_nothing() {
        let mut fb = Framebuffer::new(64, 16);
        fb.clear(0);
        text(&mut fb, 1, 1, "A", 0xFFFFFF, 1);
        assert!(lit_pixels(&fb) > 0);

        fb.clear(0);
        text(&mut fb, 1, 1, "   ", 0xFFFFFF, 1);
        assert_eq!(lit_pixels(&fb), 0);
    }

    /// Case folding is what lets the HUD print a file name without a lowercase
    /// table; if it regressed, half of every path would vanish.
    #[test]
    fn lowercase_renders_as_uppercase() {
        let mut a = Framebuffer::new(64, 16);
        let mut b = Framebuffer::new(64, 16);
        a.clear(0);
        b.clear(0);
        text(&mut a, 0, 0, "abc", 0xFFFFFF, 1);
        text(&mut b, 0, 0, "ABC", 0xFFFFFF, 1);
        assert_eq!(a.color(), b.color());
    }

    /// Every glyph must fit its 5×7 cell — a stray bit 7 would draw an eighth
    /// row and smear text into the line below.
    #[test]
    fn no_glyph_sets_a_bit_outside_seven_rows() {
        for (i, g) in FONT_5X7.iter().enumerate() {
            for (c, bits) in g.iter().enumerate() {
                assert_eq!(bits & 0x80, 0, "glyph {i} column {c}");
            }
        }
    }

    #[test]
    fn text_width_matches_what_is_drawn() {
        let mut fb = Framebuffer::new(64, 16);
        fb.clear(0);
        text(&mut fb, 0, 0, "MM", 0xFFFFFF, 1);
        let rightmost = (0..fb.width())
            .filter(|x| (0..fb.height()).any(|y| fb.color_at(*x, y) != 0))
            .max()
            .unwrap();
        assert_eq!(rightmost + 1, text_width("MM", 1));
    }

    #[test]
    fn drawing_off_the_edges_clips_instead_of_panicking() {
        let mut fb = Framebuffer::new(16, 16);
        fb.clear(0);
        text(&mut fb, -20, -20, "CLIPPED", 0xFFFFFF, 2);
        text(&mut fb, 200, 200, "CLIPPED", 0xFFFFFF, 2);
        fill_rect(&mut fb, -5, -5, 100, 100, 0x111111);
        stroke_rect(&mut fb, -5, -5, 100, 100, 0x222222);
        blend_rect(&mut fb, -5, -5, 100, 100, 0x333333, 128);
    }

    #[test]
    fn a_full_alpha_blend_is_the_source_colour() {
        let mut fb = Framebuffer::new(4, 4);
        fb.clear(0x102030);
        blend_rect(&mut fb, 0, 0, 4, 4, 0xAABBCC, 255);
        assert_eq!(fb.color_at(0, 0), 0xAABBCC);
        blend_rect(&mut fb, 0, 0, 4, 4, 0x000000, 0);
        assert_eq!(fb.color_at(0, 0), 0xAABBCC);
    }

    #[test]
    fn stroke_rect_draws_the_border_and_not_the_middle() {
        let mut fb = Framebuffer::new(8, 8);
        fb.clear(0);
        stroke_rect(&mut fb, 1, 1, 5, 5, 0xFFFFFF);
        assert_eq!(fb.color_at(1, 1), 0xFFFFFF);
        assert_eq!(fb.color_at(5, 5), 0xFFFFFF);
        assert_eq!(fb.color_at(3, 3), 0);
    }
}
