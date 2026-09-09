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
use voxel_core::Span;

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
fn palette_height() -> u32 {
    255u32.div_ceil(COLUMNS) * SWATCH + PAD * 2 + text_height(TEXT_SCALE) + PAD
}

/// One row of the layer list.
const ROW: u32 = text_height(TEXT_SCALE) + 6;
/// The visibility box at the head of each row.
const EYE: u32 = 9;

/// How far one level of the tree steps in.
const INDENT: u32 = 9;
/// The fold triangle at the head of an object row.
const TWISTY: u32 = 7;

/// One line of the layer panel: an object, or a layer inside one.
///
/// The panel used to be a flat list, and `layer_hit` was a hand-written inverse
/// of the drawing code — correct, tested row by row, and a standing invitation
/// to drift. A tree makes that inverse harder, so there is no longer an inverse:
/// [`panel_rows`] is built once and *both* the drawing and the hit test index
/// into it. They cannot disagree about where row four is, because there is one
/// answer to that question.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelRow {
    Object {
        index: usize,
        depth: u32,
        /// Whether it has anything to fold — layers or child objects.
        holds: bool,
    },
    Layer {
        index: usize,
        depth: u32,
    },
}

/// The panel's rows, top to bottom.
///
/// Objects in arena order — parents before children, which the arena already
/// guarantees — with each object's own layers under it. Within an object the
/// layers are still drawn **top of the stack first**, the rule the flat list
/// followed, because a layer that covers another is above it on screen.
///
/// What this gives up is reading the *whole* stack's order off the panel: two
/// layers in different objects are drawn in tree order rather than stack order.
/// That is the trade for showing the tree at all, and it is the right way round
/// — the tree is what the file, the MCP surface and every "make the left arm
/// longer" is expressed in, and it was previously invisible at the window.
pub fn panel_rows(editor: &Editor) -> Vec<PanelRow> {
    let model = editor.model();
    let mut rows = Vec::new();
    fn walk(editor: &Editor, object: usize, depth: u32, rows: &mut Vec<PanelRow>) {
        let model = editor.model();
        let layers: Vec<usize> = model.object_layers(object);
        let children: Vec<usize> = (0..model.object_count())
            .filter(|i| model.objects()[*i].parent == Some(object))
            .collect();
        rows.push(PanelRow::Object {
            index: object,
            depth,
            holds: !layers.is_empty() || !children.is_empty(),
        });
        if editor.is_collapsed(object) {
            return;
        }
        for &n in layers.iter().rev() {
            rows.push(PanelRow::Layer {
                index: n,
                depth: depth + 1,
            });
        }
        for child in children {
            walk(editor, child, depth + 1, rows);
        }
    }
    if model.object_count() > 0 {
        walk(editor, 0, 0, &mut rows);
    }
    rows
}

/// The layer panel sits directly under the palette, same width, so the right
/// hand side is one column of controls rather than two things at different
/// margins.
fn layers_panel_height(rows: usize) -> u32 {
    text_height(TEXT_SCALE) + PAD + rows as u32 * ROW + PAD * 2
}

/// Top-left of the first *row*, past the panel's own heading.
fn layers_origin(fb_width: u32) -> (i32, i32) {
    (
        panel_x(fb_width) + PAD as i32,
        (palette_height() + PAD + text_height(TEXT_SCALE) + PAD) as i32,
    )
}

/// What a click on the layer panel means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LayerHit {
    /// The row that was hit, exactly as it was drawn.
    pub row: PanelRow,
    /// Whether the visibility box was hit rather than the row's name.
    pub on_eye: bool,
}

/// Where a row's visibility box starts, given its depth.
fn eye_x(ox: i32, depth: u32) -> i32 {
    ox + (depth * INDENT) as i32
}

/// The row under a framebuffer pixel, or `None` off the list.
///
/// Indexes the same [`panel_rows`] the drawing walks, so this is a lookup
/// rather than an inverse. Only the *horizontal* split is worked out here, and
/// it is measured from the row's own indent — the eye of a nested row is not
/// where the eye of a top-level row is.
pub fn layer_hit(fb_width: u32, rows: &[PanelRow], x: f32, y: f32) -> Option<LayerHit> {
    let (ox, oy) = layers_origin(fb_width);
    let (dx, dy) = (x - ox as f32, y - oy as f32);
    if dx < 0.0 || dy < 0.0 || dx >= (panel_width() - PAD * 2) as f32 {
        return None;
    }
    let row = *rows.get((dy as u32 / ROW) as usize)?;
    let depth = match row {
        PanelRow::Object { depth, .. } | PanelRow::Layer { depth, .. } => depth,
    };
    let eye = eye_x(ox, depth) - ox;
    Some(LayerHit {
        row,
        on_eye: (eye as f32..(eye + EYE as i32) as f32).contains(&dx),
    })
}

/// Whether a framebuffer pixel is over either panel rather than the 3D view.
///
/// Bounded vertically as well as horizontally. Testing the column alone made
/// the whole right-hand strip of the window swallow clicks — the panels only
/// cover their top few hundred rows, and below that the viewport reaches the
/// window edge like anywhere else.
pub fn over_panel(fb_width: u32, rows: usize, x: f32, y: f32) -> bool {
    let bottom = palette_height() + layers_panel_height(rows);
    x >= panel_x(fb_width) as f32 && (0.0..bottom as f32).contains(&y)
}

/// How many glyphs fit in `pixels`.
fn fit_chars(pixels: u32) -> usize {
    (pixels / (ADVANCE * TEXT_SCALE)) as usize
}

/// Trim `s` to `max` characters, keeping the **end**.
///
/// For the status line, which is mostly file paths: the informative half of a
/// path is the file name. Truncating the head and marking it with `..` keeps
/// that visible where a plain cut would leave a column of identical directory
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

/// Trim `s` to `max` characters, keeping the **start**.
///
/// The summary line runs most-important-first — the size and the voxel count
/// before the undo depth — so it loses its tail, not its head.
fn fit_head(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 2 {
        return String::new();
    }
    let head: String = s.chars().take(max - 2).collect();
    format!("{head}..")
}

pub fn draw(fb: &mut Framebuffer, editor: &Editor) {
    draw_palette(fb, editor);
    draw_layers(fb, editor);
    draw_status(fb, editor);
    if let Some(name) = editor.renaming() {
        draw_rename(fb, editor, name);
    }
    if editor.show_help {
        draw_help(fb);
    }
}

/// The layer stack, top of the stack at the top of the list.
///
/// Each row is its visibility, its name, and how many voxels it alone holds —
/// the count being the one number that says whether a layer you cannot see is
/// empty or merely hidden.
fn draw_layers(fb: &mut Framebuffer, editor: &Editor) {
    let rows = panel_rows(editor);
    let model = editor.model();
    let x0 = panel_x(fb.width());
    let width = panel_width();
    let top = palette_height() as i32;
    overlay::blend_rect(
        fb,
        x0,
        top,
        width,
        layers_panel_height(rows.len()),
        PANEL_BG,
        220,
    );
    overlay::text(
        fb,
        x0 + PAD as i32,
        top + PAD as i32,
        "OBJECTS",
        DIM,
        TEXT_SCALE,
    );

    let (ox, oy) = layers_origin(fb.width());
    let inner = panel_width() - PAD * 2;
    for (n, row) in rows.iter().enumerate() {
        let y = oy + (n as u32 * ROW) as i32;
        match *row {
            PanelRow::Object {
                index,
                depth,
                holds,
            } => {
                let object = &model.objects()[index];
                let bx = eye_x(ox, depth);
                let box_y = y + (ROW as i32 - EYE as i32) / 2;
                // An object's switch is drawn hollow-or-filled and nothing
                // else: an object has no "hidden by something above it" state
                // of its own to show, because that *is* what it does to the
                // things under it.
                if object.visible {
                    overlay::fill_rect(fb, bx, box_y, EYE, EYE, DIM);
                } else {
                    overlay::stroke_rect(fb, bx, box_y, EYE, EYE, DIM);
                }
                // The fold marker, after the switch: a bar when open, a bar
                // and a stem when shut, so "there is more here" reads without
                // colour and without a glyph the 5x7 font does not have.
                let tx = bx + EYE as i32 + 4;
                if holds {
                    let ty = y + ROW as i32 / 2 - 1;
                    overlay::fill_rect(fb, tx, ty, TWISTY, 2, DIM);
                    if editor.is_collapsed(index) {
                        overlay::fill_rect(fb, tx + TWISTY as i32 / 2 - 1, ty - 3, 2, TWISTY, DIM);
                    }
                }
                let name_x = tx + TWISTY as i32 + 4;
                let room = fit_chars((ox + inner as i32 - name_x).max(0) as u32);
                overlay::text(
                    fb,
                    name_x,
                    y + 3,
                    &fit_head(&object.name, room),
                    if object.visible { TEXT } else { DIM },
                    TEXT_SCALE,
                );
            }
            PanelRow::Layer { index, depth } => {
                let layer = &model.layers()[index];
                let active = index == editor.active_layer();
                let bx = eye_x(ox, depth);
                if active {
                    overlay::blend_rect(fb, ox - 2, y - 1, inner + 4, ROW, ACCENT, 40);
                    overlay::stroke_rect(fb, ox - 2, y - 1, inner + 4, ROW, ACCENT);
                }

                // Four states, because a layer can be off for two different
                // reasons and only one of them is undone by pressing V on it —
                // and because one kind of layer is on screen and still not
                // somewhere you can draw:
                //
                //   filled          on screen
                //   filled + notch  on screen, an instance's copy, refuses writes
                //   hollow + pip    switched on, but an object above it is hidden
                //   hollow          switched off here
                //
                // Drawing the third case as filled would be a straight lie
                // about what is on screen; drawing it as plain hollow would
                // make V look broken. Drawing the second as an ordinary layer
                // would make a click on it look like a broken editor rather
                // than a rule.
                let box_y = y + (ROW as i32 - EYE as i32) / 2;
                if layer.is_generated() {
                    overlay::fill_rect(fb, bx, box_y, EYE, EYE, if active { ACCENT } else { DIM });
                    overlay::fill_rect(fb, bx + EYE as i32 - 3, box_y, 3, 3, PANEL_BG);
                } else if layer.shown() {
                    overlay::fill_rect(fb, bx, box_y, EYE, EYE, if active { ACCENT } else { TEXT });
                } else {
                    overlay::stroke_rect(fb, bx, box_y, EYE, EYE, DIM);
                    if layer.visible {
                        overlay::fill_rect(fb, bx + 2, box_y + 2, EYE - 4, EYE - 4, DIM);
                    }
                }

                // The count is right-aligned, so the name gets whatever is
                // left over rather than being cut to a fixed column that is
                // wrong at both ends.
                let count = layer.filled_count().to_string();
                let count_w = text_width(&count, TEXT_SCALE);
                let name_x = bx + EYE as i32 + 5;
                let room =
                    fit_chars((ox + inner as i32 - count_w as i32 - 6 - name_x).max(0) as u32);
                overlay::text(
                    fb,
                    name_x,
                    y + 3,
                    &fit_head(&layer.name, room),
                    if active { ACCENT } else { TEXT },
                    TEXT_SCALE,
                );
                overlay::text(
                    fb,
                    ox + inner as i32 - count_w as i32,
                    y + 3,
                    &count,
                    DIM,
                    TEXT_SCALE,
                );
            }
        }
    }
}

/// The rename prompt, over the viewport rather than in the panel: it is modal,
/// and a modal state that looks like part of the furniture is one you forget
/// you are in.
fn draw_rename(fb: &mut Framebuffer, editor: &Editor, name: &str) {
    let heading = format!("RENAME LAYER {}", editor.active_layer() + 1);
    let footer = "ENTER OK    ESC CANCEL";
    let line_h = text_height(TEXT_SCALE) + 4;
    // A caret, so an empty name still shows the prompt is taking keys.
    let typed = format!("{name}_");
    let w = [heading.as_str(), footer, typed.as_str()]
        .iter()
        .map(|l| text_width(l, TEXT_SCALE))
        .max()
        .unwrap_or(0)
        .max(200)
        + PAD * 4;
    let h = line_h * 3 + PAD * 3;
    let x = ((fb.width().saturating_sub(panel_width())) / 2).saturating_sub(w / 2) as i32;
    let y = (fb.height().saturating_sub(h + 80)) as i32;

    overlay::blend_rect(fb, x, y, w, h, 0x0A0C10, 245);
    overlay::stroke_rect(fb, x, y, w, h, ACCENT);
    let tx = x + (PAD * 2) as i32;
    overlay::text(fb, tx, y + PAD as i32, &heading, ACCENT, TEXT_SCALE);
    overlay::text(
        fb,
        tx,
        y + PAD as i32 + line_h as i32,
        &typed,
        TEXT,
        TEXT_SCALE,
    );
    overlay::text(
        fb,
        tx,
        y + PAD as i32 + (line_h * 2) as i32,
        footer,
        DIM,
        TEXT_SCALE,
    );
}

fn draw_palette(fb: &mut Framebuffer, editor: &Editor) {
    let width = panel_width();
    let x0 = panel_x(fb.width());
    let rows = 255u32.div_ceil(COLUMNS);
    overlay::blend_rect(fb, x0, 0, width, palette_height(), PANEL_BG, 220);

    let (ox, oy) = grid_origin(fb.width());
    for index in 1..=255u32 {
        let i = index - 1;
        let (col, row) = (i % COLUMNS, i / COLUMNS);
        let (x, y) = (ox + (col * SWATCH) as i32, oy + (row * SWATCH) as i32);
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

    overlay::text(
        fb,
        PAD as i32,
        y,
        &fit_head(&editor.summary(), room),
        TEXT,
        TEXT_SCALE,
    );
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

/// One labelled chip of the tool rows. Returns the width it used.
fn chip(fb: &mut Framebuffer, x: i32, y: i32, label: &str, active: bool) -> u32 {
    let w = text_width(label, TEXT_SCALE) + 12;
    let h = text_height(TEXT_SCALE) + 8;
    overlay::blend_rect(fb, x, y, w, h, PANEL_BG, if active { 235 } else { 170 });
    if active {
        overlay::stroke_rect(fb, x, y, w, h, ACCENT);
    }
    overlay::text(
        fb,
        x + 6,
        y + 4,
        label,
        if active { ACCENT } else { DIM },
        TEXT_SCALE,
    );
    w
}

/// The two rows along the top-left: what the tool does, and how far it reaches.
///
/// Two rows rather than one long one because they are two independent choices —
/// every tool can be had at every span, and a single row of eleven chips would
/// read as eleven tools and hide that. The row on screen runs in the same order
/// as the keys `1`–`4`, so finding the key from the label needs no lookup.
pub fn draw_tools(fb: &mut Framebuffer, editor: &Editor) {
    let tools = [
        (Tool::Build, "B BUILD"),
        (Tool::Erase, "E ERASE"),
        (Tool::Paint, "P PAINT"),
        (Tool::Pick, "I PICK"),
    ];
    let h = text_height(TEXT_SCALE) + 8;
    let mut x = PAD as i32;
    for (tool, label) in tools {
        x += chip(fb, x, PAD as i32, label, editor.tool == tool) as i32 + 6;
    }

    let y = PAD as i32 + h as i32 + 6;
    let mut x = PAD as i32;
    for (i, span) in Span::ALL.iter().enumerate() {
        let label = format!("{} {}", i + 1, span.name());
        x += chip(fb, x, y, &label, editor.span == *span) as i32 + 6;
    }
    // The brush is the voxel span's shape, so it sits at the end of that row
    // rather than in one of its own — and only once it is bigger than the one
    // cell every other span is measured from.
    if editor.brush.radius > 0 {
        let e = editor.brush.edge();
        let label = format!("{} {e}x{e}x{e}", editor.brush.shape.name());
        chip(fb, x, y, &label, editor.span == Span::Voxel);
    }

    // A third row for the mirror planes.
    //
    // They were only ever in the help card, and a setting you have to go
    // looking for is one people ask how to reach. Three chips say both things
    // at once: that mirroring is per axis, and which key each axis is on.
    let y = y + h as i32 + 6;
    let mut x = PAD as i32;
    overlay::text(fb, x, y + 4, "MIRROR", DIM, TEXT_SCALE);
    x += text_width("MIRROR", TEXT_SCALE) as i32 + 8;
    for (axis, key) in ["X", "Y", "Z"].iter().enumerate() {
        x += chip(fb, x, y, key, editor.mirror[axis]) as i32 + 4;
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
    "1 2 3 4      VOXEL AXIS PLANE VOLUME",
    "9 0          BRUSH SMALLER / BIGGER",
    "C            BRUSH CUBE / BALL",
    "",
    "[ ]          COLOUR -1 / +1",
    "- =          COLOUR -16 / +16",
    "X Y Z        MIRROR THE EDIT (M = X)",
    "G            GRID",
    ", .          SLICE DOWN / UP",
    "\\            SLICE OFF",
    "F            FRAME MODEL    R  RESET VIEW",
    "",
    "CTRL+Z       UNDO      CTRL+Y  REDO",
    "CTRL+S       SAVE      CTRL+E  EXPORT VOX",
    "CTRL+R       RELOAD    CTRL+N  CLEAR",
    "CTRL+D       SUBDIVIDE x2",
    "CTRL+Q       QUIT",
    "",
    "L SHIFT+L    NEXT / PREVIOUS LAYER",
    "A D          ADD / DELETE LAYER",
    "V N          SHOW-HIDE / RENAME LAYER",
    "K J          MOVE LAYER UP / DOWN",
    "U T          MERGE DOWN / TRIM TO FIT",
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
            palette_hit(
                fb_w,
                ox as f32 + (COLUMNS * SWATCH) as f32 + 1.0,
                oy as f32 + 1.0
            ),
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

    /// A scene with a nested part, two layers in it and one at the root —
    /// enough shape that a tree and a flat list disagree about every row.
    fn tree_editor() -> Editor {
        use std::path::PathBuf;
        use voxel_core::VoxelModel;
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.add_object(0, "ROBOT");
        e.add_object(1, "ARM");
        e.add_named_layer("BODY", Default::default());
        e.add_named_layer("HAND", Default::default());
        e.set_layer_object(1, 1);
        e.set_layer_object(2, 2);
        e
    }

    /// The panel is a box, not a column. Claiming the whole right-hand strip
    /// left a tall dead zone where clicks in the viewport did nothing.
    #[test]
    fn the_panel_covers_its_own_box_and_nothing_else() {
        let fb_w = 800;
        assert!(over_panel(fb_w, 1, 799.0, 10.0));
        assert!(!over_panel(
            fb_w,
            1,
            (fb_w - panel_width()) as f32 - 1.0,
            10.0
        ));
        let bottom = (palette_height() + layers_panel_height(1)) as f32;
        assert!(
            over_panel(fb_w, 1, 799.0, bottom - 1.0),
            "the layer list is panel"
        );
        assert!(
            !over_panel(fb_w, 1, 799.0, bottom + 1.0),
            "below it is viewport"
        );
        // And it grows with the rows, or the lower ones swallow no clicks and
        // pass them to the model behind.
        assert!(over_panel(fb_w, 8, 799.0, bottom + 1.0));
    }

    /// The tree the panel draws: objects in arena order, each one's layers
    /// under it, and within an object still top of the stack first.
    #[test]
    fn the_panel_lists_objects_with_their_layers_under_them() {
        let e = tree_editor();
        assert_eq!(
            panel_rows(&e),
            vec![
                PanelRow::Object {
                    index: 0,
                    depth: 0,
                    holds: true
                },
                PanelRow::Layer { index: 0, depth: 1 },
                PanelRow::Object {
                    index: 1,
                    depth: 1,
                    holds: true
                },
                PanelRow::Layer { index: 1, depth: 2 },
                PanelRow::Object {
                    index: 2,
                    depth: 2,
                    holds: true
                },
                PanelRow::Layer { index: 2, depth: 3 },
            ],
        );
    }

    /// Folding removes rows from the panel and from the hit test at once,
    /// because they are the same list.
    #[test]
    fn folding_an_object_hides_everything_under_it() {
        let mut e = tree_editor();
        e.toggle_collapsed(1);
        assert_eq!(
            panel_rows(&e),
            vec![
                PanelRow::Object {
                    index: 0,
                    depth: 0,
                    holds: true
                },
                PanelRow::Layer { index: 0, depth: 1 },
                PanelRow::Object {
                    index: 1,
                    depth: 1,
                    holds: true
                },
            ],
            "ROBOT is shut, so its layer and the ARM under it are gone"
        );
        e.toggle_collapsed(1);
        assert_eq!(panel_rows(&e).len(), 6, "and open again puts them back");
    }

    /// The inverse has to be exact here too: rows are twenty pixels apart, and
    /// a hit test one row out edits something the user was not pointing at.
    /// There is no inverse any more — this walks the rows the panel draws and
    /// asks for the middle of each one back.
    #[test]
    fn a_click_lands_on_the_row_drawn_under_it() {
        let fb_w = 800;
        let e = tree_editor();
        let rows = panel_rows(&e);
        let (ox, oy) = layers_origin(fb_w);
        for (n, want) in rows.iter().enumerate() {
            let y = oy as f32 + (n as u32 * ROW) as f32 + ROW as f32 / 2.0;
            // Well to the right of any indent, so this is a name hit whatever
            // depth the row sits at.
            let x = ox as f32 + (panel_width() - PAD * 2) as f32 - 2.0;
            let hit = layer_hit(fb_w, &rows, x, y).unwrap_or_else(|| panic!("row {n}"));
            assert_eq!(&hit.row, want, "row {n}");
            assert!(!hit.on_eye, "row {n} is a name hit");
        }
    }

    /// The switch is a target of its own, and it moves with the indent — the
    /// eye of a nested row is not where a top-level row's eye is, and testing
    /// only the first would have missed that.
    #[test]
    fn the_visibility_box_follows_the_indent() {
        let fb_w = 800;
        let e = tree_editor();
        let rows = panel_rows(&e);
        let (ox, oy) = layers_origin(fb_w);
        for (n, row) in rows.iter().enumerate() {
            let depth = match *row {
                PanelRow::Object { depth, .. } | PanelRow::Layer { depth, .. } => depth,
            };
            let y = oy as f32 + (n as u32 * ROW) as f32 + ROW as f32 / 2.0;
            let eye = eye_x(ox, depth) as f32;
            assert!(
                layer_hit(fb_w, &rows, eye + 1.0, y).unwrap().on_eye,
                "row {n} at depth {depth}"
            );
            // Just left of it is indent, not switch — which is what stops a
            // click on a nested row's blank space toggling something. Only
            // meaningful once there *is* an indent: at depth 0 the switch
            // starts at the panel's edge and two pixels left of it is not the
            // panel at all.
            if depth > 0 {
                assert!(
                    !layer_hit(fb_w, &rows, eye - 2.0, y).unwrap().on_eye,
                    "row {n}: the indent is not the switch"
                );
            }
            assert!(
                !layer_hit(fb_w, &rows, eye + EYE as f32 + 1.0, y)
                    .unwrap()
                    .on_eye,
                "row {n}: past the switch is the name"
            );
        }
    }

    #[test]
    fn clicks_outside_the_panel_rows_select_nothing() {
        let fb_w = 800;
        let e = tree_editor();
        let rows = panel_rows(&e);
        let (ox, oy) = layers_origin(fb_w);
        assert_eq!(
            layer_hit(fb_w, &rows, ox as f32 - 1.0, oy as f32 + 4.0),
            None
        );
        assert_eq!(
            layer_hit(fb_w, &rows, ox as f32 + 4.0, oy as f32 - 1.0),
            None
        );
        // Past the last row: the panel is taller than its rows when the tree is
        // short, and the space below them is not the root object.
        let below = oy as f32 + (rows.len() as u32 * ROW) as f32 + 1.0;
        assert_eq!(layer_hit(fb_w, &rows, ox as f32 + 4.0, below), None);
    }

    /// A palette click and a layer click must not both fire: the two hit tests
    /// share the panel column, and only their rows keep them apart.
    #[test]
    fn the_two_panels_do_not_claim_each_others_clicks() {
        let fb_w = 800;
        let (lx, ly) = layers_origin(fb_w);
        assert_eq!(palette_hit(fb_w, lx as f32 + 4.0, ly as f32 + 4.0), None);

        let rows = panel_rows(&tree_editor());
        let (px, py) = grid_origin(fb_w);
        assert_eq!(
            layer_hit(fb_w, &rows, px as f32 + 4.0, py as f32 + 4.0),
            None
        );
    }

    #[test]
    fn a_long_path_is_trimmed_from_the_front_so_the_name_survives() {
        assert_eq!(fit("short", 20), "short");
        let cut = fit("/a/very/long/path/robot.vxm", 15);
        assert_eq!(cut, "..ath/robot.vxm");
        assert_eq!(
            cut.chars().count(),
            15,
            "the result must fill exactly the room given"
        );
        assert_eq!(fit("abcdef", 6), "abcdef");
        // No room at all is empty, not a panic or a lone marker.
        assert_eq!(fit("abcdef", 2), "");
        assert_eq!(fit("abcdef", 0), "");
    }

    /// The summary loses its tail, not its head: "32X32X32  9 VOX" is the part
    /// worth keeping when the bar is narrow.
    #[test]
    fn the_summary_is_trimmed_from_the_back() {
        assert_eq!(fit_head("32X32X32  9 VOX  BUILD", 12), "32X32X32  ..");
        assert_eq!(fit_head("short", 20), "short");
        assert_eq!(fit_head("abcdef", 2), "");
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
        editor.add_layer();
        editor.begin_rename();
        for (w, h) in [(1u32, 1u32), (40, 30), (200, 60)] {
            let mut fb = Framebuffer::new(w, h);
            fb.clear(0);
            draw(&mut fb, &editor);
            draw_tools(&mut fb, &editor);
        }
    }
}
