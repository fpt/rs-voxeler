//! The tools an agent drives the editor with, and what they report back.
//!
//! Every tool runs on the **editor thread**, against the same `Editor` the
//! window is drawing — see [`super::Bridge`]. That is the point of the SSE
//! transport: the agent edits, the user watches it happen, and neither has to
//! ask the other for a turn.
//!
//! # Coordinates
//!
//! Model space: integers in `0..size` on each axis, **+Y up**, the same
//! coordinates the file stores. Not the centred world space the camera orbits —
//! an agent naming a cell should not have to know where the volume was put.
//!
//! # Every tool reports what changed
//!
//! An agent cannot see the screen, so a tool that says "ok" has told it
//! nothing. Each edit returns a [`Report`]: how many cells it looked at, how
//! many were added, removed, repainted, and left alone, and what the layer and
//! the model hold afterwards. Those five numbers sum to `targeted`, which is
//! what makes them checkable rather than merely reassuring.

use serde_json::{json, Value};
use voxel_core::region::{self, Brush, Reach, Span};
use voxel_core::{Face, VoxelModel};

use super::wire::{CallResult, ToolInfo};
use crate::editor::Editor;

/// What one edit did, cell by cell.
///
/// The four outcomes are exclusive and exhaustive: a cell the tool touched went
/// from air to solid, solid to air, one colour to another, or nowhere. An agent
/// that asked for a 5×5×5 box and reads `targeted: 125, added: 0` knows it
/// aimed at solid rock, without seeing anything.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Report {
    pub targeted: usize,
    pub added: usize,
    pub removed: usize,
    pub repainted: usize,
    pub unchanged: usize,
}

impl Report {
    fn record(&mut self, before: u8, after: u8) {
        self.targeted += 1;
        match (before, after) {
            (0, 0) => self.unchanged += 1,
            (0, _) => self.added += 1,
            (_, 0) => self.removed += 1,
            (b, a) if b == a => self.unchanged += 1,
            _ => self.repainted += 1,
        }
    }
}

/// Everything `tools/list` advertises.
///
/// Hand-authored schemas rather than derived ones: the descriptions are what an
/// agent reads to decide *which* tool to reach for, and they carry the
/// coordinate convention and the active-layer rule that no type could.
pub fn list() -> Vec<ToolInfo> {
    let color = json!({
        "type": "integer", "minimum": 0, "maximum": 255,
        "description": "Palette index. 0 is air, so it erases. Omit to use the colour \
                        currently selected in the editor."
    });
    let coord = |what: &str| {
        json!({
            "type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3,
            "description": format!("{what} as [x, y, z] in model space, +Y up.")
        })
    };
    let layer = json!({
        "description": "A layer, by its index (0 is the bottom of the stack) or by its name.",
        "type": ["integer", "string"]
    });

    vec![
        ToolInfo {
            name: "describe_model",
            description:
                "The state of the model: its size, how many voxels it holds, its bounding box, \
                 the layer stack, the active layer and the selected colour. Call this first — \
                 every other tool takes coordinates inside the size it reports.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolInfo {
            name: "put_voxel",
            description:
                "Set one voxel on the active layer. A colour of 0 erases it. Reports whether the \
                 cell was added, repainted, removed or already what you asked for.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer"}, "y": {"type": "integer"}, "z": {"type": "integer"},
                    "color": color,
                },
                "required": ["x", "y", "z"],
            }),
        },
        ToolInfo {
            name: "put_rect",
            description:
                "Fill an axis-aligned box solid on the active layer, corners inclusive. Give the \
                 same value twice on an axis for a flat rectangle, or a single cell. A colour of \
                 0 clears the box instead.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": coord("One corner"), "to": coord("The opposite corner"),
                    "color": color,
                },
                "required": ["from", "to"],
            }),
        },
        ToolInfo {
            name: "paint",
            description:
                "Recolour the voxels inside a box on the active layer, creating none: air stays \
                 air. Use put_rect to add material, paint to change what is already there.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": coord("One corner"), "to": coord("The opposite corner"),
                    "color": color,
                },
                "required": ["from", "to"],
            }),
        },
        ToolInfo {
            name: "fill",
            description:
                "Flood fill from one cell, over every cell of the ACTIVE LAYER connected to it \
                 that holds the same palette index. Seeded on one of that layer's voxels it \
                 recolours the connected part; seeded on air it fills the cavity. Refuses a cell \
                 another layer owns — select that layer first. Stops at the volume's edge.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer"}, "y": {"type": "integer"}, "z": {"type": "integer"},
                    "color": color,
                },
                "required": ["x", "y", "z"],
            }),
        },
        ToolInfo {
            name: "set_color",
            description:
                "Select the palette index later edits use by default. This is the same selection \
                 the user sees highlighted in the palette strip.",
            input_schema: json!({
                "type": "object",
                "properties": {"color": {"type": "integer", "minimum": 1, "maximum": 255}},
                "required": ["color"],
            }),
        },
        ToolInfo {
            name: "find_color",
            description:
                "The palette index closest to an RGB value, with the colour actually at that \
                 index. The palette is fixed at 255 entries, so an exact match is not guaranteed \
                 — the reported distance says how close it came.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "r": {"type": "integer", "minimum": 0, "maximum": 255},
                    "g": {"type": "integer", "minimum": 0, "maximum": 255},
                    "b": {"type": "integer", "minimum": 0, "maximum": 255},
                },
                "required": ["r", "g", "b"],
            }),
        },
        ToolInfo {
            name: "add_layer",
            description:
                "Add an empty layer above the active one and select it. Layers composite top \
                 down, so a new layer covers what is below without consuming it.",
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
            }),
        },
        ToolInfo {
            name: "select_layer",
            description:
                "Choose the layer that edits are written to. Every editing tool writes to this \
                 layer and no other.",
            input_schema: json!({
                "type": "object",
                "properties": {"layer": layer},
                "required": ["layer"],
            }),
        },
        ToolInfo {
            name: "set_layer_visible",
            description:
                "Show or hide a layer. A hidden layer keeps its voxels and is still saved; it is \
                 simply not drawn, and not counted in the model's visible voxel total.",
            input_schema: json!({
                "type": "object",
                "properties": {"layer": layer, "visible": {"type": "boolean"}},
                "required": ["layer", "visible"],
            }),
        },
    ]
}

/// Run one tool against the editor.
///
/// Errors come back as [`CallResult::failure`] rather than as JSON-RPC errors:
/// a bad coordinate is something the agent should read and correct, not a
/// protocol fault that tears down its session.
pub fn call(editor: &mut Editor, name: &str, args: &Value) -> CallResult {
    match dispatch(editor, name, args) {
        Ok(result) => result,
        Err(message) => CallResult::failure(message),
    }
}

fn dispatch(editor: &mut Editor, name: &str, args: &Value) -> Result<CallResult, String> {
    match name {
        "describe_model" => Ok(CallResult::text(describe(editor))),
        "put_voxel" => {
            let p = point_in(args, editor.model())?;
            let color = color_arg(editor, args)?;
            edit(editor, "mcp put", color, move |_, _| vec![p])
        }
        "put_rect" => {
            let (lo, hi) = range(args, editor.model())?;
            let color = color_arg(editor, args)?;
            edit(editor, "mcp rect", color, move |_, _| box_cells(lo, hi))
        }
        "paint" => {
            let (lo, hi) = range(args, editor.model())?;
            let color = color_arg(editor, args)?;
            if color == 0 {
                return Err("paint cannot take colour 0; use put_rect to clear a box".into());
            }
            // Paint changes what is there and creates nothing, so the cells it
            // takes are the solid ones of the active layer — not the box.
            edit(editor, "mcp paint", color, move |model, layer| {
                box_cells(lo, hi)
                    .into_iter()
                    .filter(|c| model.get_in(layer, c[0], c[1], c[2]) != 0)
                    .collect()
            })
        }
        "fill" => {
            let p = point_in(args, editor.model())?;
            let color = color_arg(editor, args)?;
            let active = editor.active_layer();
            // A fill reads a grid to decide its region and then writes to the
            // active layer. If those are different grids it can copy another
            // layer's shape onto this one — 100 cells "added", the original
            // untouched, and nothing on screen to explain it. An agent naming a
            // coordinate cannot see that happen, so it is refused here rather
            // than reported afterwards.
            if let Some(owner) = editor.model().owner_at(p[0], p[1], p[2]) {
                if owner != active {
                    let name = &editor.model().layers()[owner].name;
                    return Err(format!(
                        "the voxel at {p:?} belongs to layer {owner} ({name:?}), and fill writes \
                         to the active layer {active}. Call select_layer to move there, or \
                         put_rect to add material on this layer instead."
                    ));
                }
            }
            edit(editor, "mcp fill", color, move |model, layer| {
                region::cells(
                    model,
                    Span::Volume,
                    Reach {
                        seed: p,
                        // Volume growth never consults the face; any of the six
                        // gives the same region.
                        face: Face::PosY,
                        matches: model.get_in(layer, p[0], p[1], p[2]),
                        brush: Brush::default(),
                        grounded: false,
                        y_limit: u16::MAX,
                        // This layer's own grid, so the region is a shape this
                        // layer actually has.
                        layer: Some(layer),
                    },
                )
            })
        }
        "set_color" => {
            let index = args
                .get("color")
                .and_then(Value::as_u64)
                .ok_or("set_color needs a colour")?;
            if !(1..=255).contains(&index) {
                return Err(format!("colour {index} is outside 1..=255; 0 is air"));
            }
            editor.color = index as u8;
            editor.set_status(format!("colour {index}"));
            Ok(CallResult::text(format!(
                "selected colour {index}\n{}",
                json!({"color": index, "rgb": rgb_of(editor, index as u8)})
            )))
        }
        "find_color" => {
            let want = [
                channel(args, "r")?,
                channel(args, "g")?,
                channel(args, "b")?,
            ];
            let (index, dist) = nearest_color(editor.model(), want);
            Ok(CallResult::text(format!(
                "colour {index} is the closest match\n{}",
                json!({
                    "color": index,
                    "rgb": rgb_of(editor, index),
                    "requested": want,
                    "distance": dist,
                })
            )))
        }
        "add_layer" => {
            let before = editor.model().layer_count();
            match args.get("name").and_then(Value::as_str) {
                Some(name) => editor.add_named_layer(name),
                None => editor.add_layer(),
            }
            if editor.model().layer_count() == before {
                return Err(editor.status().to_string());
            }
            Ok(CallResult::text(format!(
                "added a layer\n{}",
                layers_json(editor)
            )))
        }
        "select_layer" => {
            let i = layer_arg(editor, args)?;
            editor.select_layer(i);
            Ok(CallResult::text(format!(
                "editing layer {i}\n{}",
                layers_json(editor)
            )))
        }
        "set_layer_visible" => {
            let i = layer_arg(editor, args)?;
            let visible = args
                .get("visible")
                .and_then(Value::as_bool)
                .ok_or("set_layer_visible needs `visible`")?;
            editor.set_layer_visible(i, visible);
            Ok(CallResult::text(format!(
                "layer {i} is now {}\n{}",
                if visible { "shown" } else { "hidden" },
                layers_json(editor)
            )))
        }
        other => Err(format!("no tool named {other}")),
    }
}

/// Apply a write to a set of cells as **one undo step**, and report it.
///
/// One step because the user shares this history: an agent that filled a box
/// should cost them one `ctrl+Z`, not five hundred. The cells are chosen by a
/// closure taking the model, so a tool can select against the grid it is about
/// to change — paint needs the solid cells, fill needs a flood — without this
/// function knowing which.
fn edit(
    editor: &mut Editor,
    label: &'static str,
    color: u8,
    cells: impl FnOnce(&VoxelModel, usize) -> Vec<[i32; 3]>,
) -> Result<CallResult, String> {
    let mut report = Report::default();
    editor.apply_batch(label, color, cells, |before, after| report.record(before, after));
    Ok(CallResult::text(format!(
        "{}\n{}",
        summary(&report),
        json!({
            "targeted": report.targeted,
            "added": report.added,
            "removed": report.removed,
            "repainted": report.repainted,
            "unchanged": report.unchanged,
            "layer": editor.model().layers()[editor.active_layer()].name,
            "layer_voxels": editor.model().layers()[editor.active_layer()].filled_count(),
            "model_voxels": editor.model().filled_count(),
        })
    )))
}

fn summary(r: &Report) -> String {
    if r.targeted == 0 {
        return "nothing in range".into();
    }
    format!(
        "{} cells: {} added, {} removed, {} repainted, {} unchanged",
        r.targeted, r.added, r.removed, r.repainted, r.unchanged
    )
}

fn describe(editor: &Editor) -> String {
    let model = editor.model();
    let [sx, sy, sz] = model.size();
    let bounds = model
        .occupied_bounds()
        .map(|(lo, hi)| json!({"min": lo, "max": hi}));
    format!(
        "{sx}x{sy}x{sz}, {} voxels, {} layers\n{}",
        model.filled_count(),
        model.layer_count(),
        json!({
            "size": [sx, sy, sz],
            "coordinates": "0..size on each axis, +Y up; the same coordinates the file stores",
            "voxels": model.filled_count(),
            "bounds": bounds,
            "color": editor.color,
            "color_rgb": rgb_of(editor, editor.color),
            "active_layer": editor.active_layer(),
            "layers": layer_rows(editor),
            "note": "every editing tool writes to the active layer only",
        })
    )
}

fn layer_rows(editor: &Editor) -> Vec<Value> {
    editor
        .model()
        .layers()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({
                "index": i,
                "name": l.name,
                "visible": l.visible,
                "voxels": l.filled_count(),
                "active": i == editor.active_layer(),
            })
        })
        .collect()
}

fn layers_json(editor: &Editor) -> Value {
    json!({"active_layer": editor.active_layer(), "layers": layer_rows(editor)})
}

fn rgb_of(editor: &Editor, index: u8) -> [u8; 3] {
    let c = editor.model().palette().get(index);
    [c.r, c.g, c.b]
}

/// The palette entry nearest an RGB value, by squared distance.
///
/// Air is skipped: index 0 has a colour slot so that indexing needs no offset,
/// but returning it would hand back "erase" as the answer to "what is black".
fn nearest_color(model: &VoxelModel, want: [u8; 3]) -> (u8, u32) {
    let mut best = (1u8, u32::MAX);
    for i in 1..=255u8 {
        let c = model.palette().get(i);
        let d = [
            (c.r as i32 - want[0] as i32),
            (c.g as i32 - want[1] as i32),
            (c.b as i32 - want[2] as i32),
        ]
        .iter()
        .map(|v| (v * v) as u32)
        .sum();
        if d < best.1 {
            best = (i, d);
        }
    }
    best
}

// -- argument parsing ----------------------------------------------------

fn channel(args: &Value, name: &str) -> Result<u8, String> {
    let v = args
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing `{name}`"))?;
    u8::try_from(v).map_err(|_| format!("`{name}` is {v}, outside 0..=255"))
}

fn axis(args: &Value, name: &str) -> Result<i32, String> {
    args.get(name)
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| format!("missing or out-of-range `{name}`"))
}

/// One cell, checked against the volume.
///
/// Refused rather than clipped, for the reason a box is: a write that lands
/// nowhere reports "0 added", which reads as the model rejecting the colour
/// rather than as the agent naming a cell that does not exist.
fn point_in(args: &Value, model: &VoxelModel) -> Result<[i32; 3], String> {
    let p = [axis(args, "x")?, axis(args, "y")?, axis(args, "z")?];
    if !model.contains(p[0], p[1], p[2]) {
        let s = model.size();
        return Err(format!(
            "{p:?} is outside the {}x{}x{} volume",
            s[0], s[1], s[2]
        ));
    }
    Ok(p)
}

fn corner(args: &Value, name: &str) -> Result<[i32; 3], String> {
    let a = args
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{name}` must be [x, y, z]"))?;
    if a.len() != 3 {
        return Err(format!("`{name}` must have exactly 3 numbers"));
    }
    let mut out = [0i32; 3];
    for (i, v) in a.iter().enumerate() {
        out[i] = v
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| format!("`{name}[{i}]` is not a whole number"))?;
    }
    Ok(out)
}

/// Two corners, sorted into a min and a max, and checked against the volume.
///
/// Sorted rather than required in order: an agent that names the far corner
/// first means the same box, and refusing it would be a rule with no purpose.
/// The bounds check *is* worth refusing — a box quietly clipped to nothing
/// reports "0 added" and looks like the model rejecting the colour.
fn range(args: &Value, model: &VoxelModel) -> Result<([i32; 3], [i32; 3]), String> {
    let a = corner(args, "from")?;
    let b = corner(args, "to")?;
    let mut lo = [0i32; 3];
    let mut hi = [0i32; 3];
    for i in 0..3 {
        lo[i] = a[i].min(b[i]);
        hi[i] = a[i].max(b[i]);
    }
    let size = model.size();
    for i in 0..3 {
        if lo[i] < 0 || hi[i] >= size[i] as i32 {
            return Err(format!(
                "{:?}..{:?} runs outside the {}x{}x{} volume on {}",
                a,
                b,
                size[0],
                size[1],
                size[2],
                ["x", "y", "z"][i]
            ));
        }
    }
    Ok((lo, hi))
}

fn box_cells(lo: [i32; 3], hi: [i32; 3]) -> Vec<[i32; 3]> {
    let mut out = Vec::new();
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                out.push([x, y, z]);
            }
        }
    }
    out
}

fn color_arg(editor: &Editor, args: &Value) -> Result<u8, String> {
    match args.get("color") {
        None | Some(Value::Null) => Ok(editor.color),
        Some(v) => {
            let n = v.as_i64().ok_or("`color` must be a number")?;
            u8::try_from(n).map_err(|_| format!("colour {n} is outside 0..=255"))
        }
    }
}

/// A layer by index or by name. Names are matched case-insensitively, because
/// an agent reading "ARMOUR" out of `describe_model` and sending back "armour"
/// has not made a mistake worth an error.
fn layer_arg(editor: &Editor, args: &Value) -> Result<usize, String> {
    let v = args.get("layer").ok_or("missing `layer`")?;
    let count = editor.model().layer_count();
    if let Some(i) = v.as_u64() {
        return (i as usize)
            .lt(&count)
            .then_some(i as usize)
            .ok_or_else(|| format!("layer {i} does not exist; there are {count}"));
    }
    let name = v.as_str().ok_or("`layer` must be an index or a name")?;
    editor
        .model()
        .layers()
        .iter()
        .position(|l| l.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            format!(
                "no layer named {name:?}; there is {}",
                editor
                    .model()
                    .layers()
                    .iter()
                    .map(|l| format!("{:?}", l.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn editor() -> Editor {
        Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"))
    }

    /// The text a tool returns carries a JSON object; the tests read the
    /// numbers out of it the way an agent would.
    fn json_of(r: &CallResult) -> Value {
        let super::super::wire::Content::Text { text } = &r.content[0];
        let start = text.find('{').expect("a report object");
        serde_json::from_str(&text[start..]).expect("valid JSON")
    }

    fn run(e: &mut Editor, name: &str, args: Value) -> CallResult {
        call(e, name, &args)
    }

    #[test]
    fn every_advertised_tool_has_a_schema_and_answers_to_its_name() {
        let mut e = editor();
        for tool in list() {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(!tool.description.is_empty(), "{}", tool.name);
            // Called with no arguments a tool must fail cleanly, never panic,
            // and never be mistaken for one that does not exist.
            let r = call(&mut e, tool.name, &json!({}));
            let super::super::wire::Content::Text { text } = &r.content[0];
            assert!(!text.contains("no tool named"), "{}", tool.name);
        }
    }

    #[test]
    fn put_voxel_reports_what_it_did_to_the_cell() {
        let mut e = editor();
        let r = run(&mut e, "put_voxel", json!({"x": 1, "y": 2, "z": 3, "color": 7}));
        let j = json_of(&r);
        assert_eq!(j["targeted"], 1);
        assert_eq!(j["added"], 1);
        assert_eq!(j["model_voxels"], 1);
        assert_eq!(e.model().get(1, 2, 3), 7);

        // The same write again changes nothing, and says so.
        let j = json_of(&run(&mut e, "put_voxel", json!({"x": 1, "y": 2, "z": 3, "color": 7})));
        assert_eq!(j["unchanged"], 1);
        assert_eq!(j["added"], 0);

        let j = json_of(&run(&mut e, "put_voxel", json!({"x": 1, "y": 2, "z": 3, "color": 9})));
        assert_eq!(j["repainted"], 1);

        // Colour 0 is air, so it erases.
        let j = json_of(&run(&mut e, "put_voxel", json!({"x": 1, "y": 2, "z": 3, "color": 0})));
        assert_eq!(j["removed"], 1);
        assert_eq!(j["model_voxels"], 0);
    }

    /// The four outcomes have to add up, or the numbers an agent checks
    /// against are not a measurement of anything.
    #[test]
    fn the_outcomes_account_for_every_cell_targeted() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "put_voxel", json!({"x": 0, "y": 0, "z": 0, "color": 5}));

        let j = json_of(&run(
            &mut e,
            "put_rect",
            json!({"from": [0,0,0], "to": [3,1,3], "color": 4}),
        ));
        let sum = j["added"].as_u64().unwrap()
            + j["removed"].as_u64().unwrap()
            + j["repainted"].as_u64().unwrap()
            + j["unchanged"].as_u64().unwrap();
        assert_eq!(sum, j["targeted"].as_u64().unwrap());
        assert_eq!(j["targeted"], 32);
        assert_eq!(j["added"], 16, "the upper layer of the box was air");
        assert_eq!(j["repainted"], 1, "the one cell that was another colour");
        assert_eq!(j["unchanged"], 15);
    }

    #[test]
    fn a_whole_tool_call_is_one_undo_step() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [7,7,7], "color": 4}));
        assert_eq!(e.model().filled_count(), 512);
        assert_eq!(e.undo_depth(), 1, "one ctrl+Z, not five hundred");
        e.undo();
        assert_eq!(e.model().filled_count(), 0);
    }

    /// Paint changes what is there and creates nothing — the distinction that
    /// makes it a different tool from put_rect rather than a synonym.
    #[test]
    fn paint_recolours_material_and_creates_none() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [1,1,1], "to": [2,2,2], "color": 4}));
        let j = json_of(&run(&mut e, "paint", json!({"from": [0,0,0], "to": [7,7,7], "color": 9})));

        assert_eq!(j["targeted"], 8, "only the solid cells were in range");
        assert_eq!(j["repainted"], 8);
        assert_eq!(j["added"], 0);
        assert_eq!(j["model_voxels"], 8);
        assert_eq!(e.model().get(1, 1, 1), 9);
    }

    #[test]
    fn paint_refuses_colour_zero_rather_than_erasing_by_surprise() {
        let mut e = editor();
        let r = run(&mut e, "paint", json!({"from": [0,0,0], "to": [1,1,1], "color": 0}));
        assert_eq!(r.is_error, Some(true));
    }

    /// The bug this guards, found by driving the real binary: a fill seeded on
    /// another layer's voxel grew its region from the composite and wrote the
    /// result to the active layer — a hundred cells "added", the original
    /// untouched, and nothing on screen to explain it.
    #[test]
    fn fill_refuses_a_cell_another_layer_owns() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "add_layer", json!({"name": "TOWER"}));

        let r = run(&mut e, "fill", json!({"x": 0, "y": 0, "z": 0, "color": 9}));
        assert_eq!(r.is_error, Some(true));
        let super::super::wire::Content::Text { text } = &r.content[0];
        assert!(text.contains("TOWER") || text.contains("select_layer"), "{text}");
        assert_eq!(e.model().layers()[1].filled_count(), 0, "no ghost copy");

        // Told where to go, it works.
        run(&mut e, "select_layer", json!({"layer": 0}));
        let j = json_of(&run(&mut e, "fill", json!({"x": 0, "y": 0, "z": 0, "color": 9})));
        assert_eq!(j["repainted"], 16);
    }

    /// And a fill grows over the active layer's own shape, not over whatever
    /// happens to be showing through from below.
    #[test]
    fn fill_follows_only_the_active_layers_own_shape() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "add_layer", json!({"name": "TOWER"}));
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [1,0,1], "color": 4}));

        let j = json_of(&run(&mut e, "fill", json!({"x": 0, "y": 0, "z": 0, "color": 9})));
        assert_eq!(j["repainted"], 4, "the four cells this layer holds, not the sixteen below");
        assert_eq!(e.model().get_in(0, 3, 0, 3), 4, "the slab is untouched");
    }

    #[test]
    fn fill_follows_the_connected_region_it_was_seeded_in() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "put_voxel", json!({"x": 7, "y": 0, "z": 7, "color": 4}));

        let j = json_of(&run(&mut e, "fill", json!({"x": 0, "y": 0, "z": 0, "color": 9})));
        assert_eq!(j["repainted"], 16, "the slab, and not the speck across the floor");
        assert_eq!(e.model().get(7, 0, 7), 4);
    }

    #[test]
    fn a_box_outside_the_volume_is_refused_rather_than_clipped() {
        let mut e = editor();
        let r = run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [8,1,1], "color": 4}));
        assert_eq!(r.is_error, Some(true));
        let super::super::wire::Content::Text { text } = &r.content[0];
        assert!(text.contains("8x8x8"), "{text}");
        assert_eq!(e.model().filled_count(), 0);
    }

    #[test]
    fn corners_may_be_given_in_either_order() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [3,3,3], "to": [1,1,1], "color": 4}));
        assert_eq!(e.model().filled_count(), 27);
    }

    #[test]
    fn edits_land_on_the_active_layer_and_the_report_names_it() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "add_layer", json!({"name": "ARMOUR"}));
        let j = json_of(&run(&mut e, "put_rect", json!({"from": [0,1,0], "to": [3,1,3], "color": 9})));

        assert_eq!(j["layer"], "ARMOUR");
        assert_eq!(j["layer_voxels"], 16);
        assert_eq!(j["model_voxels"], 32);
        assert_eq!(e.model().get_in(0, 0, 1, 0), 0, "nothing landed on the layer below");
    }

    #[test]
    fn a_layer_can_be_named_or_numbered() {
        let mut e = editor();
        run(&mut e, "add_layer", json!({"name": "ARMOUR"}));
        assert_eq!(e.active_layer(), 1);

        run(&mut e, "select_layer", json!({"layer": 0}));
        assert_eq!(e.active_layer(), 0);
        run(&mut e, "select_layer", json!({"layer": "armour"}));
        assert_eq!(e.active_layer(), 1, "names match without regard to case");

        let r = run(&mut e, "select_layer", json!({"layer": "nothing"}));
        assert_eq!(r.is_error, Some(true));
        let r = run(&mut e, "select_layer", json!({"layer": 9}));
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn hiding_a_layer_keeps_its_voxels_out_of_the_visible_count() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,0,3], "color": 4}));
        run(&mut e, "set_layer_visible", json!({"layer": 0, "visible": false}));
        assert_eq!(e.model().filled_count(), 0);

        let j = json_of(&run(&mut e, "describe_model", json!({})));
        assert_eq!(j["voxels"], 0);
        assert_eq!(j["layers"][0]["voxels"], 16, "but the layer still holds them");
        assert_eq!(j["layers"][0]["visible"], false);
    }

    #[test]
    fn describe_model_carries_what_the_other_tools_need() {
        let mut e = editor();
        run(&mut e, "put_voxel", json!({"x": 2, "y": 3, "z": 4, "color": 7}));
        let j = json_of(&run(&mut e, "describe_model", json!({})));

        assert_eq!(j["size"], json!([8, 8, 8]));
        assert_eq!(j["voxels"], 1);
        assert_eq!(j["bounds"]["min"], json!([2, 3, 4]));
        assert_eq!(j["active_layer"], 0);
        assert_eq!(j["layers"][0]["active"], true);
    }

    #[test]
    fn find_color_never_answers_with_air() {
        let mut e = editor();
        // Black is in the default ramp, and index 0 is air rather than a colour.
        let j = json_of(&run(&mut e, "find_color", json!({"r": 0, "g": 0, "b": 0})));
        assert_ne!(j["color"], 0);

        // An exact entry comes back with no distance at all.
        let rgb = rgb_of(&e, 42);
        let j = json_of(&run(
            &mut e,
            "find_color",
            json!({"r": rgb[0], "g": rgb[1], "b": rgb[2]}),
        ));
        assert_eq!(j["distance"], 0);
        assert_eq!(j["rgb"], json!(rgb));
    }

    #[test]
    fn set_color_moves_the_selection_the_user_can_see() {
        let mut e = editor();
        run(&mut e, "set_color", json!({"color": 42}));
        assert_eq!(e.color, 42);
        // And a later edit without a colour uses it.
        run(&mut e, "put_voxel", json!({"x": 0, "y": 0, "z": 0}));
        assert_eq!(e.model().get(0, 0, 0), 42);

        assert_eq!(run(&mut e, "set_color", json!({"color": 0})).is_error, Some(true));
    }

    #[test]
    fn an_unknown_tool_is_an_error_the_agent_can_read() {
        let mut e = editor();
        let r = call(&mut e, "put_sphere", &json!({}));
        assert_eq!(r.is_error, Some(true));
        let super::super::wire::Content::Text { text } = &r.content[0];
        assert!(text.contains("put_sphere"), "{text}");
    }
}
