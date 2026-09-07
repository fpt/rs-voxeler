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
//! the model hold afterwards. Those four outcomes sum to `targeted`, which is
//! what makes them checkable rather than merely reassuring.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};
use voxel_core::region::{self, Brush, Reach, Span};
use voxel_core::{Bounds, Face, VoxelModel};

use super::wire::{base64, CallResult, Content, ToolInfo};
use crate::editor::Editor;

mod modeling;

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
///
/// # Two checks, because they catch different things
///
/// `..` is refused **lexically**, before anything touches the filesystem, and
/// every `..` rather than only the escaping ones: `a/../b` is harmless and
/// `../b` is not, and telling them apart after the fact is exactly the
/// reasoning that goes wrong. Then the resolved path is checked against its
/// *canonical* form, since a prefix test on the name alone would be satisfied
/// by a symlink inside a root pointing anywhere at all.
///
/// A symlink anywhere below a root is refused outright on top of that, rather
/// than followed and re-checked. A link that points inside the root today can
/// be repointed outside between the check and the write, and nothing about a
/// voxel model needs to be reached through one.
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
             absolute inside one of them, or relative to the first. Symlinks are not followed. \
             Call list_models to see what is there, describe_model for the model being edited.",
            self.0
                .iter()
                .map(|d| format!("  {}", d.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    /// Resolve a path the agent gave, or say why not.
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
        if !self.contains(&full) {
            return Err(format!(
                "{given:?} is outside the directories this server may use: {}",
                self.display()
            ));
        }
        self.reject_symlinks(given, &full)?;
        Ok(full)
    }

    /// Whether a path is inside one of the roots, following symlinks as far as
    /// the filesystem can.
    fn contains(&self, path: &Path) -> bool {
        let real = canonical_enough(path);
        self.0.iter().any(|root| real.starts_with(root))
    }

    /// Refuse a symlink anywhere between a root and the named file.
    ///
    /// Belt as well as braces: [`contains`](Self::contains) already refuses a
    /// link that leaves the roots, but a link that stays inside can be
    /// repointed between the check and the write, and nothing about a voxel
    /// model needs to be reached through one.
    fn reject_symlinks(&self, given: &str, full: &Path) -> Result<(), String> {
        let Some(base) = self.0.iter().find(|r| full.starts_with(r)) else {
            // Reached a root only after canonicalising, so the literal path
            // went through a link. `contains` allowed it; this does not.
            return Err(format!(
                "{given:?} reaches its directory through a symlink; file tools do not follow links"
            ));
        };
        let Ok(rest) = full.strip_prefix(base) else {
            return Ok(());
        };
        let mut walk = base.clone();
        for part in rest.components() {
            walk.push(part);
            match std::fs::symlink_metadata(&walk) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(format!(
                        "{given:?} contains a symlink; file tools do not follow links"
                    ));
                }
                Ok(_) => {}
                // The file being saved does not exist yet, and neither do the
                // directories a caller may be about to make.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot inspect {given:?}: {e}")),
            }
        }
        Ok(())
    }

    /// A reusable path: relative to the first root, and absolute otherwise.
    /// Relative inputs always resolve against the first root, so shortening a
    /// secondary-root path against that root would point at a different file.
    fn relative(&self, path: &Path) -> String {
        if let Some(root) = self.primary() {
            if let Ok(rest) = path.strip_prefix(root) {
                return rest.display().to_string();
            }
        }
        path.display().to_string()
    }
}

/// The canonical form of a path that may not exist yet: canonicalise the
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

    let mut tools = vec![
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
                        "Degrees around +Y. 0 views from +Z toward -Z; 90 views from +X. \
                         Overrides view. Omit both to use the editor camera."},
                    "pitch": {"type": "number", "minimum": -89, "maximum": 89,
                        "description": "Degrees above the horizon; 30 is a three-quarter view."},
                    "width": {"type": "integer", "minimum": 64, "maximum": 1024},
                    "height": {"type": "integer", "minimum": 64, "maximum": 1024},
                    "view": {"type": "string", "enum": ["front", "back", "left", "right", "top", "three_quarter"]},
                    "show_bounds": {"type": "boolean", "default": false},
                    "ambient": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.7},
                    "diffuse": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.3},
                    "path": {"type": "string", "description": "Optional PNG output path; absolute inside one of the server's directories, or relative to the first. Still returns the image. Does not save or dirty the model."},
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
                "List model files beneath the server directories, sorted by reusable path. \
                 Without directory, searches every root. Paths are relative to the first \
                 root or absolute; absolute is also returned for each file. Searches \
                 subdirectories by default; symlinks are never followed.",
            input_schema: json!({"type": "object", "properties": {
                "directory": {"type": "string", "default": "."},
                "recursive": {"type": "boolean", "default": true}
            }}),
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
                    "path": {"type": "string", "description": "Absolute inside one of the server's directories, or relative to the first."},
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
                "properties": {"path": {"type": "string", "description": "Absolute inside one of the server's directories, or relative to the first."}},
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
                 index. The palette has 255 editable entries, so an exact match is not guaranteed \
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
            name: "subdivide",
            description:
                "Scale the whole scene up so every voxel becomes factor cubed of them. The way \
                 to take a coarse shape you are happy with and carve detail into it: the \
                 silhouette is unchanged and there is simply more room in it. Applies to every \
                 layer at once and is ONE undo step. A plain replication, not a smoothing — \
                 corners stay square, so what you carve is carved against what you drew. \
                 Refused when any axis would pass 256.",
            input_schema: json!({
                "type": "object",
                "properties": {"factor": {
                    "type": "integer", "minimum": 2, "maximum": 8, "default": 2,
                    "description": "Cells per axis per original voxel. 2 is the usual step."
                }},
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
                "Choose the default layer for edits. Basic tools write here; apply_edits, \
                 put_ellipsoid and put_line can explicitly address another layer.",
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
    ];
    tools.extend(modeling::schemas());
    tools
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
        "describe_model" => Ok(CallResult::text(describe(editor))),
        "apply_edits" | "put_ellipsoid" | "put_line" => modeling::apply(editor, name, args),
        "set_palette_color" => {
            let index = channel(args, "index")?;
            if index == 0 {
                return Err("palette index 0 is air; use 1..=255".into());
            }
            let rgb = voxel_core::Rgb8::new(
                channel(args, "r")?,
                channel(args, "g")?,
                channel(args, "b")?,
            );
            let changed = editor.set_palette_color(index, rgb);
            Ok(CallResult::text(format!(
                "palette colour {index}\n{}",
                json!({
                    "index": index, "rgb": [rgb.r, rgb.g, rgb.b], "changed": changed
                })
            )))
        }
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
        "subdivide" => {
            let factor = match args.get("factor") {
                None | Some(Value::Null) => 2,
                Some(v) => v
                    .as_u64()
                    .and_then(|f| u16::try_from(f).ok())
                    .filter(|f| (2..=8).contains(f))
                    .ok_or("`factor` must be between 2 and 8")?,
            };
            let before = editor.model().size();
            editor.subdivide(factor)?;
            let [x, y, z] = editor.model().size();
            Ok(CallResult::text(format!(
                "subdivided by {factor} — {}x{}x{} is now {x}x{y}x{z}\n{}",
                before[0],
                before[1],
                before[2],
                json!({
                    "factor": factor,
                    "size": [x, y, z],
                    "voxels": editor.model().filled_count(),
                    "allocated_cells": editor.model().allocated_cells(),
                    "layers": layer_rows(editor),
                })
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
        "screenshot" => screenshot(editor, root, args),
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
            let directory = match args.get("directory") {
                None => ".",
                Some(v) => v.as_str().ok_or("`directory` must be a string")?,
            };
            let recursive = bool_arg(args, "recursive", true)?;
            // With no `directory` named, every root is searched rather than
            // only the first: a relative path lands in the first, but a model
            // may live in any of them, and a listing that showed one would
            // report the others as empty.
            let mut pending = match args.get("directory") {
                None if root.dirs().len() > 1 => root.dirs().to_vec(),
                _ => vec![root.resolve(directory)?],
            };
            while let Some(dir) = pending.pop() {
                for entry in std::fs::read_dir(&dir)
                    .map_err(|e| format!("cannot read {}: {e}", root.relative(&dir)))?
                {
                    let entry = entry.map_err(|e| format!("cannot read directory entry: {e}"))?;
                    let kind = entry.file_type().map_err(|e| e.to_string())?;
                    if kind.is_dir() && recursive {
                        pending.push(entry.path());
                    }
                    if !kind.is_file() {
                        continue;
                    }
                    let path = entry.path();
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !ext.eq_ignore_ascii_case("vxm") && !ext.eq_ignore_ascii_case("vox") {
                        continue;
                    }
                    rows.push(json!({
                        // Both forms are reusable: `path` is relative only to
                        // the primary root, and `absolute` is always explicit.
                        "path": root.relative(&path),
                        "absolute": path.display().to_string(),
                        "bytes": entry.metadata().map_err(|e| e.to_string())?.len(),
                    }));
                }
            }
            rows.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            Ok(CallResult::text(format!(
                "{} model files in {}\n{}",
                rows.len(),
                root.display(),
                json!({
                    "roots": root.dirs().iter().map(|d| d.display().to_string())
                        .collect::<Vec<_>>(),
                    "models": rows,
                })
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
            Ok(CallResult::text(format!("opened {given}\n{}", describe(editor))))
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
                describe(editor)
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
                    if root.is_empty() {
                        return Err(root.resolve("x").unwrap_err());
                    }
                    editor.path().to_path_buf()
                }
            };
            voxel_core::format::save(&path, editor.model())
                .map_err(|e| format!("{}: {e}", root.relative(&path)))?;
            let flattened = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("vox"))
                && editor.model().layer_count() > 1;
            editor.mark_saved(&path);
            Ok(CallResult::text(format!(
                "saved {}{}\n{}",
                root.relative(&path),
                if flattened {
                    format!(" — {} layers flattened into one", editor.model().layer_count())
                } else {
                    String::new()
                },
                json!({
                    "path": root.relative(&path),
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
    editor.apply_batch(label, color, cells, |before, after| {
        report.record(before, after)
    });
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
fn screenshot(editor: &mut Editor, root: &Roots, args: &Value) -> Result<CallResult, String> {
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
                .filter(|d| d.is_finite() && d.abs() <= 360_000.)
                .map(|d| Some(d.to_radians() as f32))
                .ok_or_else(|| format!("`{name}` must be a number of degrees")),
        }
    };
    let preset = match args.get("view") {
        None => None,
        Some(v) => Some(match v.as_str().ok_or("`view` must be a string")? {
            "front" => (0_f32, 0_f32),
            "back" => (180., 0.),
            "left" => (-90., 0.),
            "right" => (90., 0.),
            "top" => (0., 89.),
            "three_quarter" => (45., 30.),
            other => return Err(format!("unknown view {other:?}")),
        }),
    };
    let yaw = angle("yaw")?.or(preset.map(|p| p.0.to_radians()));
    // Straight down the poles is a camera with no up vector; the editor clamps
    // its own orbit for the same reason.
    let pitch = angle("pitch")?
        .or(preset.map(|p| p.1.to_radians()))
        .map(|p| p.clamp(-1.55, 1.55));
    let path = match args.get("path") {
        None => None,
        Some(_) => {
            let p = root.resolve(&path_arg(args)?)?;
            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
                return Err("screenshot output path must end in .png".into());
            }
            Some(p)
        }
    };
    let mut options = crate::view::RenderOptions {
        show_bounds: bool_arg(args, "show_bounds", false)?,
        ..Default::default()
    };
    let intensity = |name: &str, default: f32| -> Result<f32, String> {
        match args.get(name) {
            None => Ok(default),
            Some(v) => v
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .map(|v| v as f32)
                .ok_or_else(|| format!("`{name}` must be a number in 0..=1")),
        }
    };
    options.light.ambient = intensity("ambient", options.light.ambient)?;
    options.light.diffuse = intensity("diffuse", options.light.diffuse)?;

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
    crate::view::render_with_options(&mut fb, editor, None, options);
    let png = voxel_render::png::encode(fb.width(), fb.height(), fb.color());

    editor.camera = saved;
    editor.show_grid = grid;

    if let Some(path) = &path {
        std::fs::write(path, &png)
            .map_err(|e| format!("cannot write {}: {e}", root.relative(path)))?;
    }

    let [sx, sy, sz] = editor.model().size();
    Ok(CallResult {
        content: vec![
            Content::Text {
                text: format!(
                    "{width}x{height} view of {sx}x{sy}x{sz}, {} voxels, yaw {:.0} pitch {:.0}\n{}",
                    editor.model().filled_count(),
                    yaw.unwrap_or(saved.yaw).to_degrees(),
                    pitch.unwrap_or(saved.pitch).to_degrees(),
                    json!({"width":width,"height":height,"path":path.as_ref().map(|p|root.relative(p)),
                        "show_bounds":options.show_bounds,"ambient":options.light.ambient,"diffuse":options.light.diffuse}),
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
            "scene_note": "size is the range voxels may occupy, not an allocation — each layer \
                           has its own origin and size and costs only that",
            "allocated_cells": model.allocated_cells(),
            "voxels": model.filled_count(),
            "bounds": bounds,
            "color": editor.color,
            "color_rgb": rgb_of(editor, editor.color),
            "active_layer": editor.active_layer(),
            "layers": layer_rows(editor),
            "path": model_path(editor),
            "unsaved": editor.is_dirty(),
            "note": "basic edits write to the active layer; apply_edits, put_ellipsoid and put_line accept explicit layers; palette edits affect all uses of an index",
        })
    )
}

fn model_path(editor: &Editor) -> String {
    editor
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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

fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, String> {
    match args.get(name) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| format!("`{name}` must be a boolean")),
    }
}

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

    #[test]
    fn subdivide_scales_the_scene_and_reports_it() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        run(&mut e, "put_rect", json!({"from": [2,2,2], "to": [5,5,5], "color": 4}));
        let voxels = e.model().filled_count();

        let j = json_of(&run(&mut e, "subdivide", json!({})));
        assert_eq!(j["factor"], 2);
        assert_eq!(j["size"], json!([32, 32, 32]));
        assert_eq!(j["voxels"], voxels * 8);
        assert_eq!(e.model().get(4, 4, 4), 4);
        assert_eq!(e.undo_depth(), 2, "the rect, then the subdivide");

        // And back, scene size and all.
        run(&mut e, "undo", json!({}));
        assert_eq!(e.model().size(), [16, 16, 16]);
        assert_eq!(e.model().filled_count(), voxels);
    }

    #[test]
    fn subdivide_refuses_a_factor_that_would_not_fit_or_make_sense() {
        let mut e = Editor::new(VoxelModel::new(200, 8, 8), PathBuf::from("t.vxm"));
        let r = run(&mut e, "subdivide", json!({}));
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).contains("256"), "{}", text_of(&r));

        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        assert_eq!(run(&mut e, "subdivide", json!({"factor": 1})).is_error, Some(true));
        assert_eq!(run(&mut e, "subdivide", json!({"factor": 99})).is_error, Some(true));
        assert_eq!(e.model().size(), [8, 8, 8]);
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

    /// The boundary an agent must not cross. Every one of these is a path that
    /// resolves outside the root, and the check is lexical so none of them
    /// reaches the filesystem to find out.
    #[test]
    fn a_path_that_leaves_the_root_is_refused() {
        let root = Roots::new([temp_root("escape")]);
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

    /// The reason absolute paths are accepted: a desktop client spawns the
    /// server with a working directory nobody can see, so a full path has to be
    /// sayable. Lost when this branch was written against an older base, and
    /// restored — do not let it regress again.
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

        let outside = temp_root("absolute-elsewhere").canonicalize().unwrap();
        assert!(root.resolve(outside.join("x.vxm").to_str().unwrap()).is_err());
    }

    #[test]
    fn a_path_may_be_absolute_inside_any_root() {
        let a = temp_root("many-a");
        let b = temp_root("many-b");
        let root = Roots::new([a.clone(), b.clone()]);

        assert!(root
            .resolve(a.canonicalize().unwrap().join("x.vxm").to_str().unwrap())
            .is_ok());
        assert!(root
            .resolve(b.canonicalize().unwrap().join("y.vxm").to_str().unwrap())
            .is_ok());
        // A relative path lands in the first, which is what the instructions say.
        assert_eq!(
            root.resolve("z.vxm").unwrap(),
            a.canonicalize().unwrap().join("z.vxm")
        );
    }

    /// Absolute paths made a name-only prefix test insufficient: a symlink
    /// inside a root could point anywhere. Refused by name *and* by canonical
    /// form, so neither spelling gets through.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_root_does_not_get_through_by_either_spelling() {
        let dir = temp_root("symlink-escape");
        let outside = temp_root("symlink-escape-outside");
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        let root = Roots::new([dir.clone()]);

        assert!(root.resolve("escape/stolen.vxm").is_err(), "by name");
        let via = dir.canonicalize().unwrap().join("escape").join("stolen.vxm");
        assert!(root.resolve(via.to_str().unwrap()).is_err(), "and absolutely");
    }

    /// And a link that stays *inside* a root is refused too. It would survive a
    /// canonical prefix test, but it can be repointed between the check and the
    /// write, and nothing about a voxel model needs to be reached through one.
    #[cfg(unix)]
    #[test]
    fn even_a_symlink_that_stays_inside_a_root_is_refused() {
        let dir = temp_root("symlink-inside");
        std::fs::create_dir(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let root = Roots::new([dir.clone()]);

        assert!(root.resolve("real/ok.vxm").is_ok());
        assert!(root.resolve("link/nope.vxm").is_err());
    }

    /// What the agent is told at `initialize`, since it cannot see the
    /// server's working directory.
    #[test]
    fn the_instructions_name_the_directories() {
        let dir = temp_root("instructions");
        let text = Roots::new([dir.clone()]).instructions();
        assert!(
            text.contains(&dir.canonicalize().unwrap().display().to_string()),
            "{text}"
        );
        assert!(text.contains("absolute"), "{text}");

        let none = Roots::none().instructions();
        assert!(none.contains("no access") || none.contains("The user"), "{none}");
    }

    /// A listing with no directory named covers every root, or a model in the
    /// second one is invisible.
    #[test]
    fn a_listing_covers_every_root() {
        let a = temp_root("list-a");
        let b = temp_root("list-b");
        std::fs::write(a.join("one.vxm"), b"VXM3").unwrap();
        std::fs::write(b.join("two.vxm"), b"VXM3").unwrap();
        let root = Roots::new([a.clone(), b.clone()]);

        let mut e = editor();
        let j = json_of(&run_in(&mut e, &root, "list_models", json!({})));
        let names: Vec<&str> = j["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"one.vxm"), "{names:?}");
        let second = b.canonicalize().unwrap().join("two.vxm");
        assert!(names.contains(&second.to_str().unwrap()), "{names:?}");
        assert_eq!(j["roots"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reported_save_and_listing_paths_reopen_the_correct_root() {
        let a = temp_root("reported-path-a");
        let b = temp_root("reported-path-b");
        let root = Roots::new([a.clone(), b.clone()]);
        let mut e = editor();
        for (dir, size) in [(&a, 8), (&b, 16)] {
            let path = dir.canonicalize().unwrap().join("same.vxm");
            let r = run_in(&mut e, &root, "new_model", json!({"path": path, "size": size}));
            assert_eq!(r.is_error, None);
            for args in [json!({"path": path}), json!({})] {
                let saved = run_in(&mut e, &root, "save_model", args);
                assert_eq!(saved.is_error, None);
                let saved = json_of(&saved);
                assert_eq!(root.resolve(saved["path"].as_str().unwrap()).unwrap(), path);
                let opened = run_in(&mut e, &root, "open_model", json!({"path": saved["path"]}));
                assert_eq!(opened.is_error, None);
                assert_eq!(e.model().size(), [size; 3]);
            }
        }
        let listing = json_of(&run_in(&mut e, &root, "list_models", json!({})));
        let rows = listing["models"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0]["path"], rows[1]["path"]);
        for row in rows {
            let path = root.resolve(row["path"].as_str().unwrap()).unwrap();
            assert_eq!(path, PathBuf::from(row["absolute"].as_str().unwrap()));
            let opened = run_in(&mut e, &root, "open_model", json!({"path": row["path"]}));
            assert_eq!(opened.is_error, None);
            let size = if path.starts_with(a.canonicalize().unwrap()) { 8 } else { 16 };
            assert_eq!(e.model().size(), [size; 3]);
        }
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    #[test]
    fn screenshot_reports_a_reusable_path_in_the_secondary_root() {
        let a = temp_root("reported-png-a");
        let b = temp_root("reported-png-b");
        let root = Roots::new([a.clone(), b.clone()]);
        let mut e = editor();
        let path = b.canonicalize().unwrap().join("preview.png");
        let r = run_in(&mut e, &root, "screenshot", json!({"path": path, "width": 64, "height": 64}));
        assert_eq!(r.is_error, None);
        let report = json_of(&r);
        assert_eq!(root.resolve(report["path"].as_str().unwrap()).unwrap(), path);
        assert_eq!(base64(&std::fs::read(&path).unwrap()), image_of(&r));
        assert!(!a.join("preview.png").exists());
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
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
        assert_eq!(j["path"], "robot.vxm");
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
        let r = call(&mut e, "put_sphere", &json!({}));
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(text.contains("put_sphere"), "{text}");
    }

    #[test]
    fn batch_edits_span_layers_and_unwind_overlapping_writes_in_one_step() {
        let mut e = editor();
        run(&mut e, "add_layer", json!({"name":"TOP"}));
        let depth = e.undo_depth();
        let r = run(
            &mut e,
            "apply_edits",
            json!({"layer":0,"color":3,"edits":[
                {"op":"rect","from":[1,1,1],"to":[2,1,1]},
                {"op":"voxel","x":1,"y":1,"z":1,"color":4},
                {"op":"voxel","x":1,"y":1,"z":1,"layer":"TOP","color":5},
                {"op":"voxel","x":2,"y":1,"z":1,"color":0}
            ]}),
        );
        assert_eq!(r.is_error, None, "{}", text_of(&r));
        let j = json_of(&r);
        assert_eq!(j["targeted"], 5);
        assert_eq!(j["added"], 3);
        assert_eq!(j["repainted"], 1);
        assert_eq!(j["removed"], 1);
        assert_eq!(e.active_layer(), 1);
        assert_eq!(e.model().get_in(0, 1, 1, 1), 4);
        assert_eq!(e.model().get(1, 1, 1), 5);
        assert_eq!(e.undo_depth(), depth + 1);
        run(&mut e, "undo", json!({}));
        assert_eq!(e.model().filled_count(), 0);
        run(&mut e, "redo", json!({}));
        assert_eq!(e.model().get_in(0, 1, 1, 1), 4);
        assert_eq!(e.model().get(1, 1, 1), 5);
        assert_eq!(e.model().get(2, 1, 1), 0);
    }

    #[test]
    fn invalid_batch_is_atomic_and_preserves_redo() {
        let mut e = editor();
        run(&mut e, "put_voxel", json!({"x":0,"y":0,"z":0}));
        e.undo();
        for invalid in [
            json!({"op":"voxel","x":8,"y":0,"z":0}),
            json!({"op":"voxel","x":0,"y":0,"z":0,"layer":99}),
            json!({"op":"voxel","x":0,"y":0,"z":0,"color":256}),
            json!({"op":"ellipsoid","center":[3,3,3],"radii":[2,0,2]}),
            json!({"op":"unknown"}),
        ] {
            let before = voxel_core::format::native::encode(e.model());
            let r = run(
                &mut e,
                "apply_edits",
                json!({"edits":[
                    {"op":"voxel","x":1,"y":1,"z":1},invalid
                ]}),
            );
            assert_eq!(r.is_error, Some(true));
            assert_eq!(voxel_core::format::native::encode(e.model()), before);
            assert_eq!(e.undo_depth(), 0);
            assert_eq!(e.redo_depth(), 1);
        }
    }

    #[test]
    fn oversized_batch_is_rejected_before_writing() {
        let mut e = Editor::new(VoxelModel::new(256, 256, 256), PathBuf::from("t.vxm"));
        let r = run(
            &mut e,
            "apply_edits",
            json!({"edits":[
                {"op":"voxel","x":1,"y":1,"z":1},
                {"op":"rect","from":[0,0,0],"to":[255,255,255]}
            ]}),
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(e.model().filled_count(), 0);
        assert_eq!(e.undo_depth(), 0);
        assert!(!e.is_dirty());
    }

    #[test]
    fn ellipsoid_supports_fractional_symmetry_and_erases_only_its_layer() {
        let mut e = editor();
        run(
            &mut e,
            "put_rect",
            json!({"from":[0,0,0],"to":[7,7,7],"color":3}),
        );
        run(&mut e, "add_layer", json!({"name":"ROUND"}));
        let shape = json!({"center":[3.5,3.5,3.5],"radii":[3.,2.,1.],"color":4});
        let r = run(&mut e, "put_ellipsoid", shape.clone());
        assert_eq!(r.is_error, None);
        assert!(e.model().layers()[1].filled_count() > 0);
        for ([x, y, z], _) in e.model().iter_filled_in(1) {
            assert_eq!(e.model().get_in(1, 7 - x as i32, y as i32, z as i32), 4);
            assert_eq!(e.model().get_in(1, x as i32, 7 - y as i32, z as i32), 4);
            assert_eq!(e.model().get_in(1, x as i32, y as i32, 7 - z as i32), 4);
        }
        let mut erase = shape;
        erase["color"] = json!(0);
        run(&mut e, "put_ellipsoid", erase);
        assert_eq!(e.model().layers()[1].filled_count(), 0);
        assert_eq!(e.model().layers()[0].filled_count(), 512);
        e.undo();
        assert!(e.model().layers()[1].filled_count() > 0);
    }

    #[test]
    fn rounded_line_has_caps_is_reversible_and_clips_to_the_scene() {
        let mut e = editor();
        let r = run(
            &mut e,
            "put_line",
            json!({"from":[2,3,3],"to":[5,3,3],"radius":1,"color":7}),
        );
        assert_eq!(r.is_error, None);
        for x in 1..=6 {
            assert_eq!(e.model().get(x, 3, 3), 7);
        }
        assert_eq!(e.model().get(2, 4, 3), 7);
        assert_eq!(e.model().get(1, 4, 3), 0, "rounded, not square end");
        let before: Vec<_> = e.model().iter_filled().collect();
        e.undo();
        run(
            &mut e,
            "put_line",
            json!({"from":[5,3,3],"to":[2,3,3],"radius":1,"color":7}),
        );
        assert_eq!(e.model().iter_filled().collect::<Vec<_>>(), before);
        e.undo();
        run(
            &mut e,
            "put_line",
            json!({"from":[0,0,0],"to":[0,0,0],"radius":1,"color":7}),
        );
        assert_eq!(e.model().filled_count(), 4, "sphere clipped to a corner");
        assert_eq!(e.model().get(1, 0, 0), 7);
        assert_eq!(e.model().get(1, 1, 0), 0);
    }

    #[test]
    fn invalid_shape_parameters_do_not_edit() {
        let mut e = editor();
        for args in [
            json!({"center":[3,3,3],"radii":[-1,2,3]}),
            json!({"center":[3,3,3],"radii":[257,2,3]}),
            json!({"center":[8,3,3],"radii":[1,2,3]}),
            json!({"center":[3,3],"radii":[1,2,3]}),
            json!({"center":[3,3,3],"radii":["1",2,3]}),
        ] {
            assert_eq!(run(&mut e, "put_ellipsoid", args).is_error, Some(true));
        }
        assert_eq!(e.undo_depth(), 0);
        assert_eq!(e.model().filled_count(), 0);
    }

    #[test]
    fn palette_changes_undo_redo_and_round_trip_without_changing_geometry() {
        let mut e = editor();
        run(&mut e, "put_voxel", json!({"x":2,"y":2,"z":2,"color":65}));
        let before = rgb_of(&e, 65);
        let depth = e.undo_depth();
        let args = json!({"index":65,"r":200,"g":25,"b":36});
        let r = run(&mut e, "set_palette_color", args.clone());
        assert_eq!(r.is_error, None);
        assert_eq!(rgb_of(&e, 65), [200, 25, 36]);
        assert_eq!(e.color, 1, "does not select a different colour");
        assert_eq!(e.model().get(2, 2, 2), 65);
        assert!(e.is_dirty());
        run(&mut e, "set_palette_color", args);
        assert_eq!(e.undo_depth(), depth + 1, "same RGB is a no-op");
        e.undo();
        assert_eq!(rgb_of(&e, 65), before);
        e.redo();
        let loaded =
            voxel_core::format::native::decode(&voxel_core::format::native::encode(e.model()))
                .unwrap();
        assert_eq!(loaded.palette().get(65), voxel_core::Rgb8::new(200, 25, 36));
        assert_eq!(loaded.get(2, 2, 2), 65);
        assert_eq!(
            run(
                &mut e,
                "set_palette_color",
                json!({"index":0,"r":0,"g":0,"b":0})
            )
            .is_error,
            Some(true)
        );
    }

    #[test]
    fn model_listing_finds_subdirectories_and_can_be_restricted() {
        let dir = temp_root("recursive-list");
        std::fs::create_dir_all(dir.join("models/deep")).unwrap();
        for name in [
            "z.vxm",
            "models/b.vox",
            "models/deep/a.vxm",
            "models/not.txt",
        ] {
            std::fs::write(dir.join(name), b"fixture").unwrap();
        }
        std::fs::create_dir(dir.join("directory.vxm")).unwrap();
        let root = Roots::new([dir.clone()]);
        let mut e = editor();
        let j = json_of(&run_in(&mut e, &root, "list_models", json!({})));
        let paths: Vec<_> = j["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["models/b.vox", "models/deep/a.vxm", "z.vxm"]);
        let j = json_of(&run_in(
            &mut e,
            &root,
            "list_models",
            json!({"directory":"models","recursive":false}),
        ));
        assert_eq!(j["models"].as_array().unwrap().len(), 1);
        assert_eq!(j["models"][0]["path"], "models/b.vox");
        assert_eq!(
            run_in(&mut e, &root, "list_models", json!({"directory":"../"})).is_error,
            Some(true)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_tools_refuse_symlinks_and_recursive_listing_skips_them() {
        let dir = temp_root("symlinks");
        let outside = temp_root("symlinks-outside");
        std::fs::write(outside.join("hidden.vxm"), b"fixture").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        std::os::unix::fs::symlink(outside.join("hidden.vxm"), dir.join("linked.vxm")).unwrap();
        let root = Roots::new([dir.clone()]);
        let mut e = editor();
        let j = json_of(&run_in(&mut e, &root, "list_models", json!({})));
        assert_eq!(j["models"], json!([]));
        for p in ["linked/hidden.vxm", "linked/new.png", "linked.vxm"] {
            assert!(root.resolve(p).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn screenshot_exports_the_same_image_and_restores_state_even_on_io_failure() {
        let dir = temp_root("preview");
        let root = Roots::new([dir.clone()]);
        let mut e = editor();
        run(
            &mut e,
            "put_rect",
            json!({"from":[1,1,1],"to":[6,3,4],"color":1}),
        );
        let camera = e.camera;
        let dirty = e.is_dirty();
        let depth = e.undo_depth();
        let args = json!({"view":"front","width":64,"height":64,"path":"front.png","ambient":0.8,"diffuse":0.2});
        let r = run_in(&mut e, &root, "screenshot", args.clone());
        assert_eq!(r.is_error, None, "{}", text_of(&r));
        assert_eq!(
            base64(&std::fs::read(dir.join("front.png")).unwrap()),
            image_of(&r)
        );
        assert_eq!(json_of(&r)["show_bounds"], false);
        assert_eq!(e.is_dirty(), dirty);
        assert_eq!(e.undo_depth(), depth);
        let mut fail = args.clone();
        fail["path"] = json!("missing/front.png");
        assert_eq!(
            run_in(&mut e, &root, "screenshot", fail).is_error,
            Some(true)
        );
        assert_eq!(format!("{:?}", e.camera), format!("{camera:?}"));
        assert!(e.show_grid);
        let mut bounds = args.clone();
        bounds["show_bounds"] = json!(true);
        assert_ne!(
            image_of(&run_in(&mut e, &root, "screenshot", bounds)),
            image_of(&r)
        );
        let mut light = args.clone();
        light["ambient"] = json!(0.1);
        assert_ne!(
            image_of(&run_in(&mut e, &root, "screenshot", light)),
            image_of(&r)
        );
        let mut bad = args.clone();
        bad["path"] = json!("model.vxm");
        assert_eq!(
            run_in(&mut e, &root, "screenshot", bad).is_error,
            Some(true)
        );
        assert!(!dir.join("model.vxm").exists());
        assert_eq!(
            run(&mut e, "screenshot", args).is_error,
            Some(true),
            "SSE still has no filesystem"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn screenshot_presets_match_explicit_angles_and_validate_options() {
        let mut e = editor();
        run(
            &mut e,
            "put_rect",
            json!({"from":[0,1,2],"to":[3,5,6],"color":3}),
        );
        for (view, yaw, pitch) in [
            ("front", 0, 0),
            ("back", 180, 0),
            ("left", -90, 0),
            ("right", 90, 0),
            ("top", 0, 89),
            ("three_quarter", 45, 30),
        ] {
            let a = run(
                &mut e,
                "screenshot",
                json!({"view":view,"width":64,"height":64}),
            );
            let b = run(
                &mut e,
                "screenshot",
                json!({"yaw":yaw,"pitch":pitch,"width":64,"height":64}),
            );
            assert_eq!(image_of(&a), image_of(&b));
        }
        for options in [
            json!({"view":"nope"}),
            json!({"ambient":-1}),
            json!({"diffuse":2}),
            json!({"show_bounds":"false"}),
        ] {
            assert_eq!(run(&mut e, "screenshot", options).is_error, Some(true));
        }
    }
}
