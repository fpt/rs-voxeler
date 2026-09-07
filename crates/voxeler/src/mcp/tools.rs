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

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};
use voxel_core::region::{self, Brush, Reach, Span};
use voxel_core::{shape, Bounds, Face, VoxelModel};

use super::wire::{base64, CallResult, Content, ToolInfo};
use crate::editor::Editor;

/// The directories an agent's file tools may reach.
///
/// An MCP server is driven by a model reading content nobody vetted, so `open`
/// and `save` resolve inside a set of directories and refuse to leave them.
///
/// # Absolute paths are how you say where you mean
///
/// A relative path is resolved against the first root, which is fine when you
/// started the server yourself in the directory you meant. A desktop MCP client
/// spawns it with whatever working directory the *app* happened to have, and
/// then nobody — user or agent — can say where "robot.vxm" is going. So an
/// absolute path is accepted too, checked against the roots rather than
/// refused, and the roots are named in the server's `initialize` instructions
/// so the agent is told where it may write before it tries.
#[derive(Clone, Debug, Default)]
pub struct Roots(Vec<PathBuf>);

impl Roots {
    /// Every directory the file tools may reach. The first is where a relative
    /// path lands.
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(
            dirs.into_iter()
                .map(|d| d.canonicalize().unwrap_or(d))
                .collect(),
        )
    }

    /// No filesystem at all — what the SSE transport uses.
    ///
    /// There, the user opened the document and is sitting in front of it. An
    /// agent that could save would be writing over their file while they
    /// worked, and one that could open would replace what they were looking at.
    /// Both are the user's to do.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.0
    }

    /// Where a relative path lands, and where a new server's model is named.
    pub fn primary(&self) -> Option<&Path> {
        self.0.first().map(PathBuf::as_path)
    }

    pub fn display(&self) -> String {
        if self.0.is_empty() {
            return "(none)".into();
        }
        self.0
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What an agent is told at `initialize`, so it knows where it may write
    /// without having to guess at the server's working directory.
    pub fn instructions(&self) -> String {
        if self.0.is_empty() {
            return "This server edits the document the user already has open. It has no \
                    access to the filesystem: the user opens and saves."
                .into();
        }
        format!(
            "Voxel models are read and written under these directories:\n{}\n\nPaths may be \
             absolute inside one of them, or relative to the first. Call list_models to see \
             what is there, describe_model for the model being edited.",
            self.0
                .iter()
                .map(|d| format!("  {}", d.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    /// Resolve a path the agent gave, or say why not.
    ///
    /// The check is on the *lexical* path — `..` components are rejected before
    /// anything touches the filesystem — and then, for an absolute path, on the
    /// canonical form of the deepest part of it that exists. A prefix test on
    /// the name alone would be satisfied by a symlink inside a root pointing
    /// anywhere at all.
    pub fn resolve(&self, given: &str) -> Result<PathBuf, String> {
        if self.0.is_empty() {
            return Err("this server edits the document the user already has open, and cannot \
                        reach the filesystem. The user saves it."
                .into());
        }
        let path = Path::new(given);
        for part in path.components() {
            match part {
                Component::Normal(_) | Component::CurDir | Component::RootDir => {}
                Component::Prefix(_) if path.is_absolute() => {}
                // `..` is refused outright rather than resolved and re-checked:
                // "a/../b" is harmless and "../b" is not, and telling them apart
                // after the fact is exactly the reasoning that goes wrong.
                _ => {
                    return Err(format!(
                        "{given:?} contains `..`; name a path without one, under {}",
                        self.display()
                    ))
                }
            }
        }
        if path.components().next().is_none() {
            return Err("a file name is required".into());
        }

        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            // Unwrapped safely: the empty case returned above.
            self.0[0].join(path)
        };
        if self.contains(&full) {
            Ok(full)
        } else {
            Err(format!(
                "{given:?} is outside the directories this server may use: {}",
                self.display()
            ))
        }
    }

    /// Whether a path is inside one of the roots, following symlinks as far as
    /// the filesystem can.
    fn contains(&self, path: &Path) -> bool {
        let real = canonical_enough(path);
        self.0.iter().any(|root| real.starts_with(root))
    }

    /// A path back in the shortest form that still names it: relative to a root
    /// when it is under one, and absolute otherwise.
    fn label(&self, path: &Path) -> String {
        for root in &self.0 {
            if let Ok(rest) = path.strip_prefix(root) {
                return rest.display().to_string();
            }
        }
        path.display().to_string()
    }
}

/// The canonical form of a path that may not exist yet: canonicalize the
/// deepest ancestor that does, and put the rest back on the end.
///
/// `save_model` names a file that is about to be created, so plain
/// `canonicalize` fails on exactly the case that matters most.
fn canonical_enough(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut here = path.to_path_buf();
    loop {
        if let Ok(real) = here.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match here.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                if !here.pop() {
                    return path.to_path_buf();
                }
            }
            None => return path.to_path_buf(),
        }
    }
}

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
            name: "put_sphere",
            description:
                "Fill a ball on the active layer. `radius` is measured to the far side of the \
                 centre cell, so 0 is one voxel and 3 is seven across — the same radius the \
                 editor's ball brush uses. `hollow` keeps only the surface, which is how you get \
                 a dome or a bubble. A colour of 0 carves the ball out instead.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "center": coord("The centre"),
                    "radius": {"type": "integer", "minimum": 0, "maximum": 255},
                    "hollow": {"type": "boolean", "description":
                        "Keep only the surface, one voxel thick. Default false."},
                    "color": color,
                },
                "required": ["center", "radius"],
            }),
        },
        ToolInfo {
            name: "put_cylinder",
            description:
                "Fill a cylinder on the active layer, between the centres of its two end caps. \
                 The ends must differ on at most one axis — that axis is the cylinder's — so \
                 [8,0,8] to [8,20,8] is an upright trunk twenty-one voxels tall. Give the same \
                 point twice for a disc one voxel thick. `hollow` makes it a capped tube; a \
                 colour of 0 bores it out instead.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": coord("The centre of one end cap"),
                    "to": coord("The centre of the other, differing on at most one axis"),
                    "radius": {"type": "integer", "minimum": 0, "maximum": 255},
                    "hollow": {"type": "boolean", "description":
                        "Keep only the surface, one voxel thick. Default false."},
                    "color": color,
                },
                "required": ["from", "to", "radius"],
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
            name: "screenshot",
            description:
                "Render the model to a PNG and return it as an image. The one tool that shows \
                 you what you built rather than counting it — voxel counts cannot tell you the \
                 arm is on backwards. Frames the model's contents, so it fills the picture \
                 whatever the volume's size.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "yaw": {"type": "number", "description":
                        "Degrees around the vertical axis. 0 looks along +Z; 90 is a quarter \
                         turn. Omit to keep the angle from the last screenshot."},
                    "pitch": {"type": "number", "minimum": -89, "maximum": 89,
                        "description": "Degrees above the horizon; 30 is a three-quarter view."},
                    "width": {"type": "integer", "minimum": 64, "maximum": 1024},
                    "height": {"type": "integer", "minimum": 64, "maximum": 1024},
                },
            }),
        },
        ToolInfo {
            name: "undo",
            description:
                "Undo the last edit, or several. One tool call is one step, so undoing once \
                 reverses one call however many voxels it touched. Shares the user's history: in \
                 a window they can undo your work and you can undo theirs.",
            input_schema: json!({
                "type": "object",
                "properties": {"steps": {"type": "integer", "minimum": 1, "default": 1}},
            }),
        },
        ToolInfo {
            name: "redo",
            description: "Re-apply what undo reversed. Any new edit discards the redo stack.",
            input_schema: json!({
                "type": "object",
                "properties": {"steps": {"type": "integer", "minimum": 1, "default": 1}},
            }),
        },
        ToolInfo {
            name: "list_models",
            description:
                "The model files in the server's root directory, with their sizes. Where open \
                 and save can reach; nothing outside it is visible.",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolInfo {
            name: "open_model",
            description:
                "Load a model from the root directory, replacing the one being edited. Discards \
                 unsaved changes and the undo history with them, so save first if they matter. A \
                 file that does not exist is an error — use new_model to start one.",
            input_schema: json!({
                "type": "object",
                "properties": {"path": {
                    "type": "string",
                    "description": "Relative to the root, e.g. \"robot.vxm\". A .vox extension \
                                    reads MagicaVoxel's format; anything else reads .vxm."
                }},
                "required": ["path"],
            }),
        },
        ToolInfo {
            name: "new_model",
            description:
                "Start an empty model of the given size, seeded with one voxel at its centre, \
                 under a name in the root directory. Nothing is written until you call \
                 save_model.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative to the root."},
                    "size": {"type": "integer", "minimum": 1, "maximum": 256,
                             "description": "Edge length of the cubic volume. Default 32."},
                },
                "required": ["path"],
            }),
        },
        ToolInfo {
            name: "save_model",
            description:
                "Write the model to the root directory. Omit the path to write back to where it \
                 came from. A .vox extension writes MagicaVoxel's format, which has nowhere to \
                 put layers and so flattens them; .vxm keeps the whole stack.",
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Relative to the root."}},
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
                 down, so a new layer covers what is below without consuming it. An empty layer \
                 costs nothing: give `origin` and `size` to declare where it will live, or leave \
                 them out and the layer grows to fit whatever you draw.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "origin": coord("The low corner of the layer's box"),
                    "size": {
                        "type": "array", "items": {"type": "integer"},
                        "minItems": 3, "maxItems": 3,
                        "description": "The layer's own extent, e.g. [64, 5, 64] for a ground \
                                        plane. A starting size, not a wall — writes outside it \
                                        enlarge it."
                    },
                },
            }),
        },
        ToolInfo {
            name: "trim_layer",
            description:
                "Shrink a layer's box to the voxels it actually holds. Boxes keep their \
                 high-water mark while you work so that erasing and redrawing does not churn \
                 them; this is how you hand the space back when a part is finished.",
            input_schema: json!({
                "type": "object",
                "properties": {"layer": layer},
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
    call_in(editor, &Roots::none(), name, args)
}

/// The same, with a root the file tools may reach into.
pub fn call_in(editor: &mut Editor, root: &Roots, name: &str, args: &Value) -> CallResult {
    match dispatch(editor, root, name, args) {
        Ok(result) => result,
        Err(message) => CallResult::failure(message),
    }
}

fn dispatch(
    editor: &mut Editor,
    root: &Roots,
    name: &str,
    args: &Value,
) -> Result<CallResult, String> {
    match name {
        "describe_model" => Ok(CallResult::text(describe(editor, root))),
        "put_voxel" => {
            let p = point_in(args, editor.model(), "")?;
            let color = color_arg(editor, args)?;
            edit(editor, "mcp put", color, move |_, _| vec![p])
        }
        "put_rect" => {
            let (lo, hi) = range(args, editor.model())?;
            let color = color_arg(editor, args)?;
            edit(editor, "mcp rect", color, move |_, _| box_cells(lo, hi))
        }
        "put_sphere" => {
            let center = point_in(args, editor.model(), "center")?;
            let radius = radius_arg(args)?;
            let color = color_arg(editor, args)?;
            let hollow = flag(args, "hollow")?;
            stamp(editor, "mcp sphere", color, shape::sphere(center, radius), hollow)
        }
        "put_cylinder" => {
            let from = corner(args, "from")?;
            let to = corner(args, "to")?;
            let radius = radius_arg(args)?;
            let color = color_arg(editor, args)?;
            let hollow = flag(args, "hollow")?;
            let cells = shape::cylinder(from, to, radius).ok_or_else(|| {
                format!(
                    "{from:?} and {to:?} differ on more than one axis; a cylinder is \
                     axis-aligned, so its ends share two of their three coordinates"
                )
            })?;
            stamp(editor, "mcp cylinder", color, cells, hollow)
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
            let p = point_in(args, editor.model(), "")?;
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
            let bounds = match (args.get("origin"), args.get("size")) {
                (None, None) => Bounds::default(),
                _ => Bounds::new(
                    u16_triple(args, "origin")?,
                    u16_triple(args, "size")?,
                ),
            };
            match args.get("name").and_then(Value::as_str) {
                Some(name) => editor.add_named_layer(name, bounds),
                None => editor.add_layer_with(bounds),
            }
            if editor.model().layer_count() == before {
                return Err(editor.status().to_string());
            }
            Ok(CallResult::text(format!(
                "added a layer\n{}",
                layers_json(editor)
            )))
        }
        "trim_layer" => {
            let i = match args.get("layer") {
                None | Some(Value::Null) => editor.active_layer(),
                Some(_) => layer_arg(editor, args)?,
            };
            let before = editor.model().layer_bounds(i).cells();
            editor.trim_layer_at(i);
            let after = editor.model().layer_bounds(i);
            Ok(CallResult::text(format!(
                "trimmed from {before} to {} cells\n{}",
                after.cells(),
                json!({
                    "layer": i,
                    "cells_before": before,
                    "cells_after": after.cells(),
                    "origin": after.origin,
                    "size": after.size,
                    "allocated_cells": editor.model().allocated_cells(),
                })
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
        "screenshot" => screenshot(editor, args),
        "undo" | "redo" => {
            let steps = match args.get("steps") {
                None | Some(Value::Null) => 1,
                Some(v) => v
                    .as_u64()
                    .filter(|n| *n >= 1)
                    .ok_or("`steps` must be a positive number")?,
            };
            let forward = name == "redo";
            let mut done = 0;
            for _ in 0..steps {
                let depth = if forward {
                    editor.redo_depth()
                } else {
                    editor.undo_depth()
                };
                if depth == 0 {
                    break;
                }
                if forward {
                    editor.redo();
                } else {
                    editor.undo();
                }
                done += 1;
            }
            Ok(CallResult::text(format!(
                "{name} {done} of {steps} steps\n{}",
                json!({
                    "steps": done,
                    "requested": steps,
                    "undo_depth": editor.undo_depth(),
                    "redo_depth": editor.redo_depth(),
                    "model_voxels": editor.model().filled_count(),
                })
            )))
        }
        "list_models" => {
            let mut rows: Vec<Value> = Vec::new();
            if root.is_empty() {
                return Err(root.resolve("x").unwrap_err());
            }
            for dir in root.dirs() {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !ext.eq_ignore_ascii_case("vxm") && !ext.eq_ignore_ascii_case("vox") {
                        continue;
                    }
                    rows.push(json!({
                        // Both forms: the short one to read, and the absolute
                        // one to hand back without having to know which root a
                        // relative path is measured from.
                        "path": root.label(&path),
                        "absolute": path.display().to_string(),
                        "bytes": entry.metadata().map(|m| m.len()).unwrap_or(0),
                    }));
                }
            }
            Ok(CallResult::text(format!(
                "{} model files under {}\n{}",
                rows.len(),
                root.display(),
                json!({"roots": root.dirs().iter().map(|d| d.display().to_string())
                            .collect::<Vec<_>>(), "models": rows})
            )))
        }
        "open_model" => {
            let given = path_arg(args)?;
            let path = root.resolve(&given)?;
            if !path.exists() {
                return Err(format!("{given:?} does not exist; new_model starts one"));
            }
            let model = voxel_core::format::load(&path).map_err(|e| format!("{given}: {e}"))?;
            editor.open(model, path);
            Ok(CallResult::text(format!("opened {given}\n{}", describe(editor, root))))
        }
        "new_model" => {
            let given = path_arg(args)?;
            let path = root.resolve(&given)?;
            let size = match args.get("size") {
                None | Some(Value::Null) => crate::editor::DEFAULT_SIZE,
                Some(v) => {
                    let n = v.as_u64().ok_or("`size` must be a number")?;
                    u16::try_from(n)
                        .ok()
                        .filter(|n| (1..=voxel_core::MAX_DIM).contains(n))
                        .ok_or_else(|| format!("size {n} is outside 1..={}", voxel_core::MAX_DIM))?
                }
            };
            editor.open(crate::editor::new_model(size), path);
            Ok(CallResult::text(format!(
                "started {given}, {size}x{size}x{size} — not written until save_model\n{}",
                describe(editor, root)
            )))
        }
        "save_model" => {
            let path = match args.get("path").and_then(Value::as_str) {
                Some(given) => root.resolve(given)?,
                // Back where it came from. Still gated on there being a root:
                // without one the editor's path is the *user's* file, and
                // saving over it while they work is not an agent's call to
                // make. With one, the path came through `resolve` already.
                None => {
                    // Still gated on there being a root: without one the
                    // editor's path is the *user's* file.
                    if root.is_empty() {
                        return Err(root.resolve("x").unwrap_err());
                    }
                    editor.path().to_path_buf()
                }
            };
            voxel_core::format::save(&path, editor.model())
                .map_err(|e| format!("{}: {e}", root.label(&path)))?;
            let flattened = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("vox"))
                && editor.model().layer_count() > 1;
            editor.mark_saved(&path);
            Ok(CallResult::text(format!(
                "saved {}{}\n{}",
                root.label(&path),
                if flattened {
                    format!(" — {} layers flattened into one", editor.model().layer_count())
                } else {
                    String::new()
                },
                json!({
                    "path": root.label(&path),
                    "voxels": editor.model().filled_count(),
                    "layers": editor.model().layer_count(),
                    "flattened": flattened,
                })
            )))
        }
        other => Err(format!("no tool named {other}")),
    }
}

fn path_arg(args: &Value) -> Result<String, String> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "`path` is required".into())
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

/// Render the model and hand it back as a PNG.
///
/// The camera is moved, used and put back: under the SSE transport this editor
/// is the one the user is looking at, and a screenshot that left their view
/// somewhere else would be an agent reaching through the screen.
fn screenshot(editor: &mut Editor, args: &Value) -> Result<CallResult, String> {
    let size = |name: &str| -> Result<u32, String> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(512),
            Some(v) => v
                .as_u64()
                .filter(|n| (64..=1024).contains(n))
                .map(|n| n as u32)
                .ok_or_else(|| format!("`{name}` must be between 64 and 1024")),
        }
    };
    let (width, height) = (size("width")?, size("height")?);
    let angle = |name: &str| -> Result<Option<f32>, String> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .map(|d| Some(d.to_radians() as f32))
                .ok_or_else(|| format!("`{name}` must be a number of degrees")),
        }
    };
    let yaw = angle("yaw")?;
    // Straight down the poles is a camera with no up vector; the editor clamps
    // its own orbit for the same reason.
    let pitch = angle("pitch")?.map(|p| p.clamp(-1.55, 1.55));

    let saved = editor.camera;
    let grid = editor.show_grid;
    // A picture of the model, not of the editor: the work-plane grid is a tool
    // for aiming a mouse, and there is no mouse here.
    editor.show_grid = false;
    if let Some(yaw) = yaw {
        editor.camera.yaw = yaw;
    }
    if let Some(pitch) = pitch {
        editor.camera.pitch = pitch;
    }
    editor.frame_model();

    let mut fb = voxel_render::Framebuffer::new(width, height);
    crate::view::render(&mut fb, editor, None);
    let png = voxel_render::png::encode(fb.width(), fb.height(), fb.color());

    editor.camera = saved;
    editor.show_grid = grid;

    let [sx, sy, sz] = editor.model().size();
    Ok(CallResult {
        content: vec![
            Content::Text {
                text: format!(
                    "{width}x{height} view of {sx}x{sy}x{sz}, {} voxels, yaw {:.0} pitch {:.0}",
                    editor.model().filled_count(),
                    yaw.unwrap_or(saved.yaw).to_degrees(),
                    pitch.unwrap_or(saved.pitch).to_degrees(),
                ),
            },
            Content::Image {
                data: base64(&png),
                mime_type: "image/png".into(),
            },
        ],
        is_error: None,
    })
}

/// Write a shape, clipping it to the scene and saying how much it lost.
///
/// Clipped rather than refused, unlike `put_rect`: a dome half sunk into the
/// ground or a pillar rising out of the top of the scene is a thing to want,
/// where a box named outside the volume is usually a mistake. The count is in
/// the report either way, so an agent that meant the whole shape can see that it
/// did not get it.
fn stamp(
    editor: &mut Editor,
    label: &'static str,
    color: u8,
    cells: Vec<[i32; 3]>,
    hollow: bool,
) -> Result<CallResult, String> {
    let cells = if hollow { shape::shell(&cells) } else { cells };
    let wanted = cells.len();
    let inside: Vec<[i32; 3]> = cells
        .into_iter()
        .filter(|c| editor.model().contains(c[0], c[1], c[2]))
        .collect();
    let clipped = wanted - inside.len();

    let mut report = Report::default();
    editor.apply_batch(label, color, |_, _| inside, |before, after| {
        report.record(before, after)
    });
    Ok(CallResult::text(format!(
        "{}{}\n{}",
        summary(&report),
        if clipped > 0 {
            format!(" ({clipped} outside the scene)")
        } else {
            String::new()
        },
        json!({
            "targeted": report.targeted,
            "added": report.added,
            "removed": report.removed,
            "repainted": report.repainted,
            "unchanged": report.unchanged,
            "clipped": clipped,
            "layer": editor.model().layers()[editor.active_layer()].name,
            "layer_voxels": editor.model().layers()[editor.active_layer()].filled_count(),
            "model_voxels": editor.model().filled_count(),
        })
    )))
}

fn radius_arg(args: &Value) -> Result<u16, String> {
    args.get("radius")
        .and_then(Value::as_u64)
        .and_then(|r| u16::try_from(r).ok())
        .filter(|r| *r <= 255)
        .ok_or_else(|| "`radius` is required, and must be 0..=255".into())
}

fn flag(args: &Value, name: &str) -> Result<bool, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| format!("`{name}` must be true or false")),
    }
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

fn describe(editor: &Editor, root: &Roots) -> String {
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
            "scene_note": "size is the range voxels may occupy, not an allocation — each layer \
                           has its own origin and size and costs only that",
            "allocated_cells": model.allocated_cells(),
            "voxels": model.filled_count(),
            "bounds": bounds,
            "color": editor.color,
            "color_rgb": rgb_of(editor, editor.color),
            "active_layer": editor.active_layer(),
            "layers": layer_rows(editor),
            // Absolute, so an agent that cannot see the server's working
            // directory still knows exactly which file it is editing.
            "path": editor.path().display().to_string(),
            "roots": root.dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
            "unsaved": editor.is_dirty(),
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
                "origin": l.bounds().origin,
                "size": l.bounds().size,
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
fn point_in(args: &Value, model: &VoxelModel, field: &str) -> Result<[i32; 3], String> {
    let p = if field.is_empty() {
        [axis(args, "x")?, axis(args, "y")?, axis(args, "z")?]
    } else {
        corner(args, field)?
    };
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

/// Three whole numbers that must fit a `u16` — an origin or a size.
fn u16_triple(args: &Value, name: &str) -> Result<[u16; 3], String> {
    let a = args
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{name}` must be [x, y, z]"))?;
    if a.len() != 3 {
        return Err(format!("`{name}` must have exactly 3 numbers"));
    }
    let mut out = [0u16; 3];
    for (i, v) in a.iter().enumerate() {
        out[i] = v
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .filter(|v| *v <= voxel_core::MAX_DIM)
            .ok_or_else(|| format!("`{name}[{i}]` is outside 0..={}", voxel_core::MAX_DIM))?;
    }
    Ok(out)
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

    fn text_of(r: &CallResult) -> &str {
        match &r.content[0] {
            super::super::wire::Content::Text { text } => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The text a tool returns carries a JSON object; the tests read the
    /// numbers out of it the way an agent would.
    fn json_of(r: &CallResult) -> Value {
        let text = text_of(r);
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
            let text = text_of(&r);
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
        let text = text_of(&r);
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
        let text = text_of(&r);
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

    // -- shapes -----------------------------------------------------------

    /// The same radius the editor's ball brush uses, so the two mean the same
    /// thing by the same word.
    #[test]
    fn a_sphere_matches_the_ball_brush_of_the_same_radius() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        let j = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [16, 16, 16], "radius": 1, "color": 5}),
        ));
        assert_eq!(j["added"], 19, "27 less the eight corners, as the brush draws it");
        assert_eq!(e.model().get(17, 17, 16), 5, "an edge neighbour");
        assert_eq!(e.model().get(17, 17, 17), 0, "and not a corner");

        // Radius 0 is one voxel, so "no radius" and "one voxel" agree.
        let j = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [2, 2, 2], "radius": 0, "color": 5}),
        ));
        assert_eq!(j["added"], 1);
    }

    #[test]
    fn a_hollow_sphere_keeps_its_surface_and_drops_the_middle() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        let solid = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [16, 16, 16], "radius": 4, "color": 5}),
        ));
        run(&mut e, "put_sphere", json!({"center": [16, 16, 16], "radius": 4, "color": 0}));

        let hollow = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [16, 16, 16], "radius": 4, "color": 5, "hollow": true}),
        ));
        assert!(
            hollow["added"].as_u64().unwrap() < solid["added"].as_u64().unwrap(),
            "a shell is smaller than the ball it came from"
        );
        assert_eq!(e.model().get(16, 16, 16), 0, "the middle is empty");
        assert_eq!(e.model().get(16, 20, 16), 5, "and the top is not");
    }

    /// A trunk: two ends, the axis implied by which coordinate differs.
    #[test]
    fn a_cylinder_runs_between_the_ends_it_is_given() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        let j = json_of(&run(
            &mut e,
            "put_cylinder",
            json!({"from": [16, 4, 16], "to": [16, 20, 16], "radius": 1, "color": 7}),
        ));
        assert_eq!(j["added"], 9 * 17, "a 3x3 disc, seventeen cells tall");
        assert_eq!(e.model().get(16, 4, 16), 7);
        assert_eq!(e.model().get(16, 20, 16), 7);
        assert_eq!(e.model().get(16, 21, 16), 0, "and it stops at the end cap");
        assert_eq!(e.model().get(16, 12, 18), 0, "and at the radius");
    }

    #[test]
    fn a_cylinder_across_two_axes_is_refused_with_the_reason() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        let r = run(
            &mut e,
            "put_cylinder",
            json!({"from": [4, 4, 4], "to": [10, 10, 4], "radius": 1}),
        );
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).contains("axis-aligned"), "{}", text_of(&r));
        assert_eq!(e.model().filled_count(), 0);
    }

    /// A colour of 0 carves, which is how a cave or a bore gets made.
    #[test]
    fn colour_zero_carves_a_shape_out_of_what_is_there() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        run(&mut e, "put_rect", json!({"from": [10,10,10], "to": [21,21,21], "color": 4}));
        let before = e.model().filled_count();

        let j = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [16, 16, 16], "radius": 3, "color": 0}),
        ));
        assert!(j["removed"].as_u64().unwrap() > 100);
        assert_eq!(j["added"], 0);
        assert_eq!(e.model().get(16, 16, 16), 0, "hollowed out");
        assert_eq!(e.model().get(10, 10, 10), 4, "and the block is still there");
        assert!(e.model().filled_count() < before);
    }

    /// Clipped rather than refused: a dome half sunk in the ground is a thing
    /// to want, and the count says what did not fit.
    #[test]
    fn a_shape_over_the_edge_is_clipped_and_the_report_says_so() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        let j = json_of(&run(
            &mut e,
            "put_sphere",
            json!({"center": [0, 0, 0], "radius": 3, "color": 5}),
        ));
        assert!(j["clipped"].as_u64().unwrap() > 0, "most of it was outside");
        assert!(j["added"].as_u64().unwrap() > 0, "and the rest went in");
        assert_eq!(e.model().get(0, 0, 0), 5);

        // A centre outside the scene is a mistake, not a shape.
        let r = run(&mut e, "put_sphere", json!({"center": [99, 0, 0], "radius": 1}));
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn a_shape_is_one_undo_step() {
        let mut e = Editor::new(VoxelModel::new(32, 32, 32), PathBuf::from("t.vxm"));
        run(&mut e, "put_sphere", json!({"center": [16, 16, 16], "radius": 5, "color": 5}));
        let placed = e.model().filled_count();
        assert!(placed > 300);
        assert_eq!(e.undo_depth(), 1, "one ctrl+Z, not five hundred");
        e.undo();
        assert_eq!(e.model().filled_count(), 0);
    }

    #[test]
    fn a_shape_without_a_radius_says_so() {
        let mut e = editor();
        assert_eq!(
            run(&mut e, "put_sphere", json!({"center": [1, 1, 1]})).is_error,
            Some(true)
        );
        assert_eq!(
            run(&mut e, "put_cylinder", json!({"from": [1,1,1], "to": [1,2,1]})).is_error,
            Some(true)
        );
    }

    // -- layer boxes ------------------------------------------------------

    /// The example this was built for, driven through the tools an agent has.
    #[test]
    fn layers_can_be_declared_with_their_own_boxes() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        run(&mut e, "add_layer", json!({"name": "GROUND", "origin": [0,0,0], "size": [64,5,64]}));
        run(&mut e, "add_layer", json!({"name": "TREE", "origin": [20,5,10], "size": [16,32,16]}));
        run(&mut e, "add_layer", json!({"name": "CHARACTER", "origin": [40,5,40], "size": [16,16,16]}));

        let j = json_of(&run(&mut e, "describe_model", json!({})));
        assert_eq!(j["size"], json!([64, 64, 64]));
        let layers = j["layers"].as_array().unwrap();
        assert_eq!(layers[1]["name"], "GROUND");
        assert_eq!(layers[1]["size"], json!([64, 5, 64]));
        assert_eq!(layers[2]["size"], json!([16, 32, 16]));
        assert_eq!(layers[2]["origin"], json!([20, 5, 10]));
        // The whole point: three shapes, not three copies of the scene.
        assert_eq!(
            j["allocated_cells"].as_u64().unwrap(),
            (64 * 5 * 64 + 16 * 32 * 16 + 16 * 16 * 16) as u64
        );
    }

    /// A declared box is a starting size, not a wall — a write outside it
    /// enlarges the layer rather than being refused.
    #[test]
    fn a_write_outside_a_declared_box_grows_it_rather_than_failing() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        run(&mut e, "add_layer", json!({"name": "TREE", "origin": [20,5,10], "size": [4,4,4]}));
        let r = run(&mut e, "put_voxel", json!({"x": 19, "y": 5, "z": 10, "color": 7}));
        assert_eq!(r.is_error, None);
        assert_eq!(json_of(&r)["added"], 1);

        let j = json_of(&run(&mut e, "describe_model", json!({})));
        assert_eq!(j["layers"][1]["origin"], json!([19, 5, 10]));
        assert_eq!(j["layers"][1]["size"], json!([5, 4, 4]));
    }

    #[test]
    fn trim_layer_reports_the_space_it_gave_back() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [15,15,15], "color": 4}));
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [15,15,15], "color": 0}));
        run(&mut e, "put_voxel", json!({"x": 3, "y": 3, "z": 3, "color": 4}));

        let j = json_of(&run(&mut e, "trim_layer", json!({})));
        assert_eq!(j["cells_before"], 4096);
        assert_eq!(j["cells_after"], 1);
        assert_eq!(j["origin"], json!([3, 3, 3]));
        assert_eq!(j["allocated_cells"], 1);
        assert_eq!(e.model().get(3, 3, 3), 4, "and the voxel is still there");
    }

    #[test]
    fn a_box_given_as_only_half_a_pair_is_refused() {
        let mut e = editor();
        assert_eq!(
            run(&mut e, "add_layer", json!({"origin": [1, 1, 1]})).is_error,
            Some(true)
        );
        assert_eq!(
            run(&mut e, "add_layer", json!({"size": [1, 1]})).is_error,
            Some(true)
        );
    }

    // -- the root, and the file tools ------------------------------------

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("voxeler-root-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_in(e: &mut Editor, root: &Roots, name: &str, args: Value) -> CallResult {
        call_in(e, root, name, &args)
    }

    /// The boundary an agent must not cross.
    #[test]
    fn a_path_that_leaves_the_root_is_refused() {
        let dir = temp_root("escape");
        let root = Roots::new([dir.clone()]);
        for bad in [
            "../outside.vxm",
            "a/../../outside.vxm",
            "../../../../etc/passwd",
            "/etc/passwd",
            "/tmp/anywhere.vxm",
            "",
        ] {
            assert!(root.resolve(bad).is_err(), "{bad:?} should be refused");
        }
        // And the ordinary cases still work, including a harmless interior dot.
        assert!(root.resolve("robot.vxm").is_ok());
        assert!(root.resolve("parts/arm.vxm").is_ok());
        assert!(root.resolve("./robot.vxm").is_ok());
    }

    /// The reason this exists: a desktop client spawns the server with a
    /// working directory nobody can see, so a full path has to be sayable.
    #[test]
    fn an_absolute_path_inside_a_root_is_accepted() {
        let dir = temp_root("absolute");
        let root = Roots::new([dir.clone()]);
        let real = dir.canonicalize().unwrap();

        let inside = real.join("robot.vxm");
        assert_eq!(root.resolve(inside.to_str().unwrap()).unwrap(), inside);
        // Including one that does not exist yet, which is every save.
        let deep = real.join("parts").join("arm.vxm");
        assert_eq!(root.resolve(deep.to_str().unwrap()).unwrap(), deep);

        // A sibling directory is still outside, absolute or not.
        let outside = temp_root("absolute-elsewhere").canonicalize().unwrap();
        assert!(root.resolve(outside.join("x.vxm").to_str().unwrap()).is_err());
    }

    /// More than one root, because a desktop config names the places models
    /// live rather than one working directory.
    #[test]
    fn a_path_may_be_absolute_inside_any_root() {
        let a = temp_root("many-a");
        let b = temp_root("many-b");
        let root = Roots::new([a.clone(), b.clone()]);

        assert!(root.resolve(a.canonicalize().unwrap().join("x.vxm").to_str().unwrap()).is_ok());
        assert!(root.resolve(b.canonicalize().unwrap().join("y.vxm").to_str().unwrap()).is_ok());
        // A relative path lands in the first, which is what the instructions say.
        assert_eq!(
            root.resolve("z.vxm").unwrap(),
            a.canonicalize().unwrap().join("z.vxm")
        );
    }

    /// A prefix test on the name alone would be satisfied by a symlink inside
    /// a root pointing anywhere at all.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_root_does_not_get_through() {
        let dir = temp_root("symlink");
        let outside = temp_root("symlink-outside");
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        let root = Roots::new([dir.clone()]);

        assert!(root.resolve("escape/stolen.vxm").is_err(), "through the link by name");
        let via = dir.canonicalize().unwrap().join("escape").join("stolen.vxm");
        assert!(root.resolve(via.to_str().unwrap()).is_err(), "and absolutely");
    }

    /// What the agent is told at `initialize`, since it cannot see the
    /// server's working directory.
    #[test]
    fn the_instructions_name_the_directories() {
        let dir = temp_root("instructions");
        let text = Roots::new([dir.clone()]).instructions();
        assert!(text.contains(&dir.canonicalize().unwrap().display().to_string()), "{text}");
        assert!(text.contains("absolute"), "{text}");

        let none = Roots::none().instructions();
        assert!(none.contains("no access") || none.contains("The user"), "{none}");
    }

    /// `a/../b` stays inside and would survive a resolve-then-check, but the
    /// rule refuses every `..` rather than reasoning about which ones are safe
    /// — that reasoning is exactly what goes wrong.
    #[test]
    fn even_a_harmless_dotdot_is_refused() {
        let root = Roots::new([temp_root("dotdot")]);
        assert!(root.resolve("a/../b.vxm").is_err());
    }

    #[test]
    fn the_sse_transport_has_a_root_that_reaches_nothing() {
        let mut e = editor();
        // `call` (rather than `call_in`) is what the windowed server uses: the
        // user opened the document, and an agent there has no business opening
        // another.
        let r = call(&mut e, "open_model", &json!({"path": "anything.vxm"}));
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(!text.contains("no tool named"), "the tool exists, it is confined: {text}");
    }

    #[test]
    fn a_model_can_be_started_saved_listed_and_opened_again() {
        let dir = temp_root("lifecycle");
        let root = Roots::new([dir.clone()]);
        let mut e = editor();

        run_in(&mut e, &root, "new_model", json!({"path": "robot.vxm", "size": 16}));
        assert_eq!(e.model().size(), [16, 16, 16]);
        assert!(!dir.join("robot.vxm").exists(), "new_model writes nothing yet");

        run_in(&mut e, &root, "put_rect", json!({"from": [0,0,0], "to": [3,3,3], "color": 4}));
        let r = run_in(&mut e, &root, "save_model", json!({}));
        assert_eq!(r.is_error, None);
        assert!(dir.join("robot.vxm").exists(), "and save_model writes it");
        assert!(!e.is_dirty());

        let j = json_of(&run_in(&mut e, &root, "list_models", json!({})));
        assert_eq!(j["models"].as_array().unwrap().len(), 1);
        assert_eq!(j["models"][0]["path"], "robot.vxm");

        // Somewhere else entirely, then back.
        run_in(&mut e, &root, "new_model", json!({"path": "other.vxm"}));
        assert_eq!(e.model().filled_count(), 1, "a new model is its seed");
        run_in(&mut e, &root, "open_model", json!({"path": "robot.vxm"}));
        assert_eq!(e.model().size(), [16, 16, 16]);
        assert_eq!(e.model().get(1, 1, 1), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Layers reach the file, and `.vox` says out loud that they will not.
    #[test]
    fn saving_as_vox_reports_the_layers_it_flattened() {
        let dir = temp_root("flatten");
        let root = Roots::new([dir.clone()]);
        let mut e = editor();
        run_in(&mut e, &root, "new_model", json!({"path": "m.vxm", "size": 8}));
        run_in(&mut e, &root, "add_layer", json!({"name": "TOP"}));
        run_in(&mut e, &root, "put_voxel", json!({"x": 2, "y": 2, "z": 2, "color": 9}));

        let j = json_of(&run_in(&mut e, &root, "save_model", json!({"path": "m.vox"})));
        assert_eq!(j["flattened"], true);
        assert_eq!(j["layers"], 2);

        let j = json_of(&run_in(&mut e, &root, "save_model", json!({"path": "m.vxm"})));
        assert_eq!(j["flattened"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_file_that_is_not_there_says_how_to_make_one() {
        let root = Roots::new([temp_root("missing")]);
        let mut e = editor();
        let r = run_in(&mut e, &root, "open_model", json!({"path": "nope.vxm"}));
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(text.contains("new_model"), "{text}");
    }

    #[test]
    fn describe_model_names_the_file_and_whether_it_is_unsaved() {
        let dir = temp_root("describe");
        let root = Roots::new([dir.clone()]);
        let mut e = editor();
        run_in(&mut e, &root, "new_model", json!({"path": "robot.vxm", "size": 8}));
        run_in(&mut e, &root, "put_voxel", json!({"x": 0, "y": 0, "z": 0, "color": 3}));

        let j = json_of(&run_in(&mut e, &root, "describe_model", json!({})));
        // Absolute: an agent that cannot see the server's working directory
        // still knows exactly which file it is editing.
        assert!(
            j["path"].as_str().unwrap().ends_with("/robot.vxm"),
            "{}", j["path"]
        );
        assert!(Path::new(j["path"].as_str().unwrap()).is_absolute());
        assert_eq!(j["roots"].as_array().unwrap().len(), 1);
        assert_eq!(j["unsaved"], true);

        run_in(&mut e, &root, "save_model", json!({}));
        let j = json_of(&run_in(&mut e, &root, "describe_model", json!({})));
        assert_eq!(j["unsaved"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- seeing, and taking it back --------------------------------------

    fn image_of(r: &CallResult) -> &str {
        r.content
            .iter()
            .find_map(|c| match c {
                Content::Image { data, mime_type } => {
                    assert_eq!(mime_type, "image/png");
                    Some(data.as_str())
                }
                _ => None,
            })
            .expect("an image block")
    }

    /// The tool that shows rather than counts. A voxel count cannot tell an
    /// agent the arm is on backwards.
    #[test]
    fn a_screenshot_comes_back_as_a_png_image_block() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [1,1,1], "to": [6,3,4], "color": 4}));
        let r = run(&mut e, "screenshot", json!({"width": 128, "height": 96}));

        assert_eq!(r.is_error, None);
        let data = image_of(&r);
        assert!(data.len() > 100, "an empty picture is not a picture");
        // Base64 of a real PNG: the signature is the first eight bytes, which
        // is the first eleven base64 characters plus a bit.
        assert!(data.starts_with("iVBORw0KG"), "not a PNG: {}", &data[..16.min(data.len())]);
        // And the text block says what was rendered, for a transcript to read.
        assert!(text_of(&r).contains("128x96"), "{}", text_of(&r));
    }

    /// The SSE transport points at the document the user is looking at. A
    /// screenshot must not leave their camera somewhere else.
    #[test]
    fn a_screenshot_puts_the_camera_back_where_it_found_it() {
        let mut e = editor();
        run(&mut e, "put_voxel", json!({"x": 4, "y": 4, "z": 4, "color": 1}));
        e.camera.yaw = 0.25;
        e.camera.pitch = 0.5;
        let before = (e.camera.yaw, e.camera.pitch, e.camera.distance);
        let grid = e.show_grid;

        run(&mut e, "screenshot", json!({"yaw": 90, "pitch": 30, "width": 64, "height": 64}));
        assert_eq!((e.camera.yaw, e.camera.pitch, e.camera.distance), before);
        assert_eq!(e.show_grid, grid, "and the grid setting too");
    }

    #[test]
    fn a_screenshot_of_a_size_it_cannot_render_is_refused() {
        let mut e = editor();
        assert_eq!(run(&mut e, "screenshot", json!({"width": 4})).is_error, Some(true));
        assert_eq!(run(&mut e, "screenshot", json!({"height": 99999})).is_error, Some(true));
    }

    /// One tool call is one step, so an agent that regrets a fill takes it back
    /// with one undo however many voxels it moved.
    #[test]
    fn undo_takes_back_whole_tool_calls_and_redo_puts_them_again() {
        let mut e = editor();
        run(&mut e, "put_rect", json!({"from": [0,0,0], "to": [3,3,3], "color": 4}));
        run(&mut e, "put_voxel", json!({"x": 7, "y": 7, "z": 7, "color": 9}));
        assert_eq!(e.model().filled_count(), 65);

        let j = json_of(&run(&mut e, "undo", json!({})));
        assert_eq!(j["steps"], 1);
        assert_eq!(e.model().filled_count(), 64, "one call, whatever it touched");

        let j = json_of(&run(&mut e, "undo", json!({"steps": 5})));
        assert_eq!(j["steps"], 1, "there was only one left to take");
        assert_eq!(j["requested"], 5);
        assert_eq!(e.model().filled_count(), 0);

        let j = json_of(&run(&mut e, "redo", json!({"steps": 2})));
        assert_eq!(j["steps"], 2);
        assert_eq!(e.model().filled_count(), 65);
    }

    #[test]
    fn undo_with_nothing_to_undo_reports_zero_rather_than_failing() {
        let mut e = editor();
        let r = run(&mut e, "undo", json!({}));
        assert_eq!(r.is_error, None, "an empty history is not a fault");
        assert_eq!(json_of(&r)["steps"], 0);
    }

    #[test]
    fn an_unknown_tool_is_an_error_the_agent_can_read() {
        let mut e = editor();
        let r = call(&mut e, "put_torus", &json!({}));
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(text.contains("put_torus"), "{text}");
    }
}
