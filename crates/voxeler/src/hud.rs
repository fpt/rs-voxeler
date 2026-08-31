//! The 2D layer: the palette strip, the status line, and the help card.
//!
//! Laid out in framebuffer pixels rather than window pixels, so the whole UI
//! scales with the render buffer and stays the same physical size whatever the
//! display's density. [`palette_hit`] is the exact inverse of the strip's
//! layout and lives next to it for the reason `kessel`'s `window_to_console`
//! does: a click that lands one swatch away from the one under the cursor reads
//! as a broken editor.

use voxel_render::overlay::{self, text_height, text_width, ADVANCE};
use voxel_render::Framebuffer;

use crate::editor::{Editor, Tool};

/// Swatch edge, in framebuffer pixels.
const SWATCH: u32 = 12;
/// Palette columns. 255 colours in 16 columns is 16 rows, which is the square
/// arrangement MagicaVoxel uses and the one people can find a colour in.
const COLUMNS: u32 = 16;
const PAD: u32 = 8;
const TEXT_SCALE: u32 = 2;

const PANEL_BG: u32 = 0x14161C;
const TEXT: u32 = 0xE8E8EC;
const DIM: u32 = 0x9098A4;
const ACCENT: u32 = 0xFFD24A;

/// The editor's one highlight colour, shared with the 3D gizmos so a slice
/// plane and the palette selection read as the same UI.
pub fn accent() -> u32 {
    ACCENT
}

/// Width of the palette panel, including its padding.
pub fn panel_width() -> u32 {
    COLUMNS * SWATCH + PAD * 2
}

/// The panel's left edge for a framebuffer of this width.
fn panel_x(fb_width: u32) -> i32 {
    fb_width as i32 - panel_width() as i32
}

/// Where the swatch grid starts.
fn grid_origin(fb_width: u32) -> (i32, i32) {
    (panel_x(fb_width) + PAD as i32, PAD as i32)
}

/// The palette index under a framebuffer pixel, or `None` if the point is not
/// on a swatch. Index 0 is never returned: it is air, and not paintable.
pub fn palette_hit(fb_width: u32, x: f32, y: f32) -> Option<u8> {
    let (ox, oy) = grid_origin(fb_width);
    let (dx, dy) = (x - ox as f32, y - oy as f32);
    if dx < 0.0 || dy < 0.0 {
        return None;
    }
    let (col, row) = ((dx as u32) / SWATCH, (dy as u32) / SWATCH);
    if col >= COLUMNS {
        return None;
    }
    let index = row * COLUMNS + col + 1;
    (index <= 255).then_some(index as u8)
}

/// How tall the palette panel actually is.
fn panel_height() -> u32 {
    255u32.div_ceil(COLUMNS) * SWATCH + PAD * 2 + text_height(TEXT_SCALE) + PAD
}

/// Whether a framebuffer pixel is over the panel rather than the 3D view.
///
/// Bounded vertically as well as horizontally. Testing the column alone made
/// the whole right-hand strip of the window swallow clicks — the panel only
/// covers its top few hundred rows, and below that the viewport reaches the
/// window edge like anywhere else.
pub fn over_panel(fb_width: u32, x: f32, y: f32) -> bool {
    x >= panel_x(fb_width) as f32 && (0.0..panel_height() as f32).contains(&y)
}

/// How many glyphs fit in `pixels`.
fn fit_chars(pixels: u32) -> usize {
    (pixels / (ADVANCE * TEXT_SCALE)) as usize
}

/// Trim `s` to `max` characters, keeping the **end**.
///
/// The status line is mostly file paths, and the informative half of a path is
/// the file name. Truncating the head and marking it with `..` keeps that
/// visible where a plain cut would leave a column of identical directory
/// prefixes running off the edge of the bar and under the help hint.
fn fit(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 2 {
        return String::new();
    }
    let tail: String = s.chars().skip(n - (max - 2)).collect();
    format!("..{tail}")
}

pub fn draw(fb: &mut Framebuffer, editor: &Editor) {
    draw_palette(fb, editor);
    draw_status(fb, editor);
    if editor.show_help {
        draw_help(fb);
    }
}

fn draw_palette(fb: &mut Framebuffer, editor: &Editor) {
    let width = panel_width();
    let x0 = panel_x(fb.width());
    let rows = 255u32.div_ceil(COLUMNS);
    overlay::blend_rect(fb, x0, 0, width, panel_height(), PANEL_BG, 220);

    let (ox, oy) = grid_origin(fb.width());
    for index in 1..=255u32 {
        let i = index - 1;
        let (col, row) = (i % COLUMNS, i / COLUMNS);
        let (x, y) = (
            ox + (col * SWATCH) as i32,
            oy + (row * SWATCH) as i32,
        );
        let color = editor.model().palette().get(index as u8).to_u32();
        overlay::fill_rect(fb, x, y, SWATCH, SWATCH, color);
        if index as u8 == editor.color {
            // Two rings, light over dark, so the selection is visible on both a
            // white swatch and a black one.
            overlay::stroke_rect(fb, x - 1, y - 1, SWATCH + 2, SWATCH + 2, 0x000000);
            overlay::stroke_rect(fb, x, y, SWATCH, SWATCH, ACCENT);
        }
    }

    let label = format!("COLOUR {}", editor.color);
    overlay::text(
        fb,
        ox,
        oy + (rows * SWATCH) as i32 + PAD as i32,
        &label,
        DIM,
        TEXT_SCALE,
    );
}

fn draw_status(fb: &mut Framebuffer, editor: &Editor) {
    let line_h = text_height(TEXT_SCALE) + 4;
    // The panel is anchored to the bottom edge and the text inset by `PAD`, so
    // the second line's descenders stay inside the window rather than being
    // clipped by it.
    let panel_h = line_h * 2 + PAD * 2;
    let panel_y = fb.height() as i32 - panel_h as i32;
    let y = panel_y + PAD as i32;
    // A window narrower than the palette panel leaves no room for the status
    // bar at all; saturating keeps that a zero-width draw rather than a wrap to
    // four billion pixels.
    let bar_w = fb.width().saturating_sub(panel_width());
    overlay::blend_rect(fb, 0, panel_y, bar_w, panel_h, PANEL_BG, 190);

    // A permanent nudge towards the help card. Discovering the keys should not
    // require reading the source.
    let hint = "H FOR KEYS";
    let hint_w = text_width(hint, TEXT_SCALE) + PAD * 2;
    let room = fit_chars(bar_w.saturating_sub(hint_w + PAD));

    overlay::text(fb, PAD as i32, y, &fit(&editor.summary(), room), TEXT, TEXT_SCALE);
    overlay::text(
        fb,
        PAD as i32,
        y + line_h as i32,
        &fit(editor.status(), room),
        if editor.is_dirty() { ACCENT } else { DIM },
        TEXT_SCALE,
    );
    overlay::text(
        fb,
        bar_w as i32 - text_width(hint, TEXT_SCALE) as i32 - PAD as i32,
        y + line_h as i32,
        hint,
        DIM,
        TEXT_SCALE,
    );
}

/// The tool row along the top-left, so the active tool is visible without
/// reading the status line.
pub fn draw_tools(fb: &mut Framebuffer, editor: &Editor) {
    let tools = [
        (Tool::Build, "B BUILD"),
        (Tool::Erase, "E ERASE"),
        (Tool::Paint, "P PAINT"),
        (Tool::Pick, "I PICK"),
    ];
    let mut x = PAD as i32;
    let h = text_height(TEXT_SCALE) + 8;
    for (tool, label) in tools {
        let w = text_width(label, TEXT_SCALE) + 12;
        let active = editor.tool == tool;
        overlay::blend_rect(fb, x, PAD as i32, w, h, PANEL_BG, if active { 235 } else { 170 });
        if active {
            overlay::stroke_rect(fb, x, PAD as i32, w, h, ACCENT);
        }
        overlay::text(
            fb,
            x + 6,
            PAD as i32 + 4,
            label,
            if active { ACCENT } else { DIM },
            TEXT_SCALE,
        );
        x += w as i32 + 6;
    }
}

const HELP: &[&str] = &[
    "VOXELER",
    "",
    "LMB          APPLY TOOL / PICK COLOUR",
    "RMB DRAG     ORBIT      MMB DRAG  PAN",
    "ALT+LMB      ORBIT      WHEEL     ZOOM",
    "",
    "B E P I      BUILD ERASE PAINT PICK",
    "[ ]          COLOUR -1 / +1",
    "- =          COLOUR -16 / +16",
    "M            MIRROR X       G  GRID",
    ", .          SLICE DOWN / UP",
    "\\            SLICE OFF",
    "F            FRAME MODEL    R  RESET VIEW",
    "",
    "CTRL+Z       UNDO      CTRL+Y  REDO",
    "CTRL+S       SAVE      CTRL+E  EXPORT VOX",
    "CTRL+R       RELOAD    CTRL+N  CLEAR",
    "CTRL+Q       QUIT",
    "",
    "H            CLOSE THIS",
];

fn draw_help(fb: &mut Framebuffer) {
    let line_h = text_height(TEXT_SCALE) + 3;
    let w = HELP
        .iter()
        .map(|l| text_width(l, TEXT_SCALE))
        .max()
        .unwrap_or(0)
        + PAD * 4;
    let h = HELP.len() as u32 * line_h + PAD * 4;
    let x = ((fb.width().saturating_sub(panel_width())) / 2).saturating_sub(w / 2) as i32;
    let y = (fb.height() / 2).saturating_sub(h / 2) as i32;

    overlay::blend_rect(fb, x, y, w, h, 0x0A0C10, 240);
    overlay::stroke_rect(fb, x, y, w, h, ACCENT);
    for (i, line) in HELP.iter().enumerate() {
        overlay::text(
            fb,
            x + (PAD * 2) as i32,
            y + (PAD * 2) as i32 + (i as u32 * line_h) as i32,
            line,
            if i == 0 { ACCENT } else { TEXT },
            TEXT_SCALE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inverse has to be exact: the swatch drawn at a pixel must be the one
    /// a click there selects.
    #[test]
    fn a_click_selects_the_swatch_drawn_under_it() {
        let fb_w = 800;
        let (ox, oy) = grid_origin(fb_w);
        for index in 1..=255u32 {
            let i = index - 1;
            let (col, row) = (i % COLUMNS, i / COLUMNS);
            // The swatch's centre.
            let x = ox as f32 + (col * SWATCH) as f32 + SWATCH as f32 / 2.0;
            let y = oy as f32 + (row * SWATCH) as f32 + SWATCH as f32 / 2.0;
            assert_eq!(palette_hit(fb_w, x, y), Some(index as u8), "index {index}");
        }
    }

    #[test]
    fn clicks_outside_the_grid_select_nothing() {
        let fb_w = 800;
        let (ox, oy) = grid_origin(fb_w);
        assert_eq!(palette_hit(fb_w, ox as f32 - 1.0, oy as f32 + 1.0), None);
        assert_eq!(palette_hit(fb_w, ox as f32 + 1.0, oy as f32 - 1.0), None);
        // Past the right-hand column.
        assert_eq!(
            palette_hit(fb_w, ox as f32 + (COLUMNS * SWATCH) as f32 + 1.0, oy as f32 + 1.0),
            None
        );
        // Past the last row: 255 colours in 16 columns leaves the 256th cell
        // empty, and it must not resolve to air.
        let last_row = (255u32 / COLUMNS) as f32;
        assert_eq!(
            palette_hit(
                fb_w,
                ox as f32 + (15 * SWATCH) as f32 + 1.0,
                oy as f32 + last_row * SWATCH as f32 + 1.0
            ),
            None
        );
    }

    /// The panel is a box, not a column. Claiming the whole right-hand strip
    /// left a tall dead zone where clicks in the viewport did nothing.
    #[test]
    fn the_panel_covers_its_own_box_and_nothing_else() {
        let fb_w = 800;
        assert!(over_panel(fb_w, 799.0, 10.0));
        assert!(!over_panel(fb_w, (fb_w - panel_width()) as f32 - 1.0, 10.0));
        assert!(
            !over_panel(fb_w, 799.0, panel_height() as f32 + 1.0),
            "below the panel is viewport"
        );
    }

    #[test]
    fn a_long_path_is_trimmed_from_the_front_so_the_name_survives() {
        assert_eq!(fit("short", 20), "short");
        let cut = fit("/a/very/long/path/robot.vxm", 15);
        assert_eq!(cut, "..ath/robot.vxm");
        assert_eq!(cut.chars().count(), 15, "the result must fill exactly the room given");
        assert_eq!(fit("abcdef", 6), "abcdef");
        // No room at all is empty, not a panic or a lone marker.
        assert_eq!(fit("abcdef", 2), "");
        assert_eq!(fit("abcdef", 0), "");
    }

    /// Every HUD path must survive a window smaller than the panels it wants to
    /// draw, which is what an aggressively resized window produces.
    #[test]
    fn drawing_into_a_tiny_framebuffer_does_not_panic() {
        use crate::editor::Editor;
        use std::path::PathBuf;
        use voxel_core::VoxelModel;

        let mut editor = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        editor.show_help = true;
        for (w, h) in [(1u32, 1u32), (40, 30), (200, 60)] {
            let mut fb = Framebuffer::new(w, h);
            fb.clear(0);
            draw(&mut fb, &editor);
            draw_tools(&mut fb, &editor);
        }
    }
}
