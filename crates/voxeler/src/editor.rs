//! The editor's state and every operation on it — everything except the window.
//!
//! Nothing here knows about `winit`. That is deliberate: the interesting
//! behaviour is which voxel a click lands on, whether a drag stays on its
//! plane, and what mirroring does, and all of it is testable by calling these
//! methods with a camera and a pixel coordinate. The window layer's job shrinks
//! to translating events into these calls.

use std::path::{Path, PathBuf};

use voxel_core::region::{self, Brush, BrushShape, Span, MAX_BRUSH};
use voxel_core::{format, Bounds, Face, History, RayHit, Stroke, VoxelModel};
use voxel_render::mesh::{extract_dirty, ExtractOptions, FaceMesh};
use voxel_render::{Mat4, OrbitCamera, Vec3};

/// The default edge length for a new model.
///
/// 32 rather than 64 because a volume you cannot judge by eye is not a
/// workspace: at 64³ framed to fit, one voxel is a few pixels across and the
/// grid reads as haze. 32³ is the size the plan starts at, it is enough for a
/// character, and `--size 64` is one flag away. A loaded file keeps whatever
/// size it was saved at — this is only the size of a model that does not exist
/// yet.
pub const DEFAULT_SIZE: u16 = 32;

/// Voxels held between calls, so they can be moved rather than only drawn.
///
/// # Cells, not a box
///
/// A selection is the set of cells it actually covers, because that is what a
/// transform moves. A box would have to carry the air inside it, and "move this
/// arm" would then drag a cube of nothing along with the arm and erase whatever
/// it landed on.
///
/// # One layer
///
/// The layer is part of the selection, not a lookup at use time. Tools write to
/// the active layer and nowhere else, and a selection that silently followed the
/// active layer would move a *different* set of voxels than the one you were
/// shown. Spanning layers is a later question; naming one is the honest answer
/// now.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Selection {
    layer: usize,
    cells: std::collections::HashSet<[i32; 3]>,
}

impl Selection {
    pub fn new(layer: usize, cells: impl IntoIterator<Item = [i32; 3]>) -> Self {
        Self {
            layer,
            cells: cells.into_iter().collect(),
        }
    }

    pub fn layer(&self) -> usize {
        self.layer
    }

    /// How many voxels are selected. Never zero: `Editor::selection` holds
    /// `None` rather than an empty selection, so "is anything selected" is one
    /// question with one answer instead of two that can disagree.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn cells(&self) -> impl Iterator<Item = [i32; 3]> + '_ {
        self.cells.iter().copied()
    }

    /// The inclusive box the selection covers, or `None` when it is empty.
    pub fn bounds(&self) -> Option<([i32; 3], [i32; 3])> {
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for c in &self.cells {
            for a in 0..3 {
                lo[a] = lo[a].min(c[a]);
                hi[a] = hi[a].max(c[a]);
            }
        }
        (!self.cells.is_empty()).then_some((lo, hi))
    }
}

fn no_selection() -> String {
    "nothing is selected".into()
}

/// Voxels lifted out of the model, waiting to be put back somewhere.
///
/// # Not part of the document
///
/// It is never saved, it is not in a [`Snapshot`](voxel_core::Snapshot), and it
/// survives undo — undo puts the *model* back, and a clipboard that emptied
/// itself when you undid the copy would be a surprise rather than a rule.
///
/// # Relative to the low corner
///
/// Cells are stored as offsets from the copied selection's low corner, so
/// `paste` at that same corner is exactly what was copied rather than an
/// arithmetic guess. It carries no layer: a paste writes to the active layer,
/// which is what makes copying from one layer to another a paste rather than a
/// separate tool.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Clipboard {
    cells: Vec<([i32; 3], u8)>,
    size: [i32; 3],
    /// The low corner it was copied from.
    ///
    /// Kept so a paste can land exactly where the copy came from without the
    /// caller having to remember the number. Over MCP the corner is an
    /// argument; at the keyboard there is nowhere to type one, and "paste puts
    /// it back where it was, then move it" is the gesture people expect.
    origin: [i32; 3],
}

impl Clipboard {
    /// How many voxels are held. Never zero: `Editor::clipboard` is `None`
    /// rather than an empty clipboard, the same rule the selection follows.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// The extent of what was copied, in cells.
    pub fn size(&self) -> [i32; 3] {
        self.size
    }

    /// Where it was copied from.
    pub fn origin(&self) -> [i32; 3] {
        self.origin
    }
}

/// What one sculpt application did.
///
/// The four exclusive outcomes every MCP edit reports, summing to `targeted`:
/// an agent cannot see the screen, so a tool that says "ok" has told it
/// nothing, and one whose numbers do not add up has told it something false.
#[derive(Clone, Copy, Default, Debug)]
pub struct SculptReport {
    pub targeted: usize,
    pub added: usize,
    pub removed: usize,
    pub repainted: usize,
    pub unchanged: usize,
}

/// What moving a selection did.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct MoveReport {
    /// Cells that arrived somewhere inside the scene.
    pub moved: usize,
    /// Of those, ones that landed on a voxel that was not part of the
    /// selection — replaced rather than filled.
    pub overwritten: usize,
    /// Cells whose destination was outside the scene, and were lost.
    pub dropped: usize,
}

/// A validated, explicitly addressed write; batches may span several layers.
pub struct CellWrite {
    pub layer: usize,
    pub pos: [i32; 3],
    pub color: u8,
}

/// What a click does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Add a voxel against the face you clicked.
    Build,
    /// Remove the voxel you clicked.
    Erase,
    /// Recolour the voxel you clicked.
    Paint,
    /// Take the colour of the voxel you clicked, without changing anything.
    Pick,
    /// Level the surface to the plane the stroke started on.
    ///
    /// The one sculpt operation that suits voxels best, and the only one that
    /// needs a *frame* rather than a reach: a plane locked at mouse-down, from
    /// the cell that was hit and the face that was hit. Material in front of it
    /// goes, air behind it fills, and the surface under the brush ends flat.
    Flatten,
    /// Round the surface off by majority vote of each cell's neighbours.
    ///
    /// A solid cell with too few solid neighbours is a spur and goes; an air
    /// cell with too many is a notch and fills. Every decision is read from the
    /// model as it stood before the application, so one pass does not cascade
    /// into itself.
    Smooth,
    /// Pick out what is already there, rather than changing it.
    ///
    /// The one tool that never writes. It is a tool rather than a modifier
    /// because a drag already means "apply the current tool", and span and
    /// brush then compose with it exactly as they do with the other four —
    /// which is the whole reason `Tool` and `Span` are separate.
    Select,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Build => "BUILD",
            Tool::Erase => "ERASE",
            Tool::Paint => "PAINT",
            Tool::Pick => "PICK",
            Tool::Flatten => "FLATTEN",
            Tool::Smooth => "SMOOTH",
            Tool::Select => "SELECT",
        }
    }
}

/// Where a click would land, resolved against the model.
///
/// A target does not have to come from hitting a voxel. Building also resolves
/// against the ground plane, so the fields describe *a face being pointed at*
/// rather than a ray hit: for the ground that face is the top of an imaginary
/// row of cells just under the volume, which makes the two cases identical
/// everywhere downstream.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Target {
    /// The voxel whose face is under the cursor.
    pub voxel: [i32; 3],
    /// Which of that voxel's faces.
    pub face: Face,
    /// The palette index at `voxel`, or 0 when the cursor is on the ground
    /// plane rather than on the model.
    pub index: u8,
    /// The cell the active tool would write to.
    pub cell: [i32; 3],
}

impl Target {
    /// Whether this is the ground plane rather than a voxel of the model.
    pub fn is_ground(self) -> bool {
        self.index == 0
    }
}

/// A drag in progress.
///
/// `plane` is what stops a build drag from climbing its own work. Each voxel
/// placed becomes a new surface for the ray to hit, so without a constraint,
/// dragging across a floor grows a staircase towards the camera. Pinning the
/// stroke to the axis and coordinate of the first placement means a drag paints
/// across the plane it started on, which is what the hand expects.
struct Drag {
    stroke: Stroke,
    /// The plane a sculpt stroke levels toward: a cell on it, and the outward
    /// normal of the face that was hit.
    ///
    /// Locked at mouse-down and never re-estimated. Re-deriving it every frame
    /// would make the brush direction flap — crossing a corner goes +X, +Y, +X
    /// and the stroke fights the hand — which is the same reason `plane` and
    /// `before` are decided once: a stroke commits to what it started on.
    reference: Option<([i32; 3], [i32; 3])>,
    plane: Option<(usize, i32)>,
    last_cell: Option<[i32; 3]>,
    /// Which layer owned the voxel the stroke started on, if any. A tool writes
    /// to the active layer, so aiming at a voxel another layer holds is a
    /// legitimate no-op — and a no-op with no explanation reads as a broken
    /// editor, which is the whole reason this is carried.
    owner: Option<usize>,
    /// Whether this stroke began on the work plane. Keeps the plane open for
    /// the rest of the drag: its first placement fills the active layer, which
    /// would otherwise close the plane out from under the remaining moves.
    on_plane: bool,
    /// What each cell this stroke has touched held **before** it did.
    ///
    /// A drag has to keep aiming at the model it started on. Placing a voxel
    /// puts a new face under the pointer, and the next mouse event — the jitter
    /// of the click itself is enough — would build against that, so one click
    /// laid down a voxel per event. Erasing has the mirror of it: the hole it
    /// opens lets the next event reach the wall behind. Casting through this
    /// map makes the stroke's own work invisible to its own aim.
    before: std::collections::HashMap<[i32; 3], u8>,
}

pub struct Editor {
    document_id: u64,
    model: VoxelModel,
    history: History,
    mesh: FaceMesh,
    mesh_dirty: bool,

    pub camera: OrbitCamera,
    pub tool: Tool,
    /// How far the tool reaches from the cell it was aimed at.
    pub span: Span,
    /// The shape stamped around that cell when the span is [`Span::Voxel`].
    pub brush: Brush,
    pub color: u8,
    /// Reflect every edit across the middle of each axis it is set for.
    ///
    /// This mirrors the *edit*, not the model. Nothing is written that the edit
    /// did not touch, so a model that is deliberately asymmetric stays that way
    /// — turning mirroring on does not go back and symmetrise what is already
    /// there, it only means the next stroke lands on both sides.
    pub mirror: [bool; 3],
    pub show_grid: bool,
    pub show_help: bool,
    /// Voxels held for a transform, if any. Not part of the document: it is
    /// never saved, and it does not survive an undo — see [`Editor::undo`].
    /// Objects whose rows are folded shut in the layer panel.
    ///
    /// A view state, not a document one: it is not saved, and it is dropped
    /// when the document is replaced along with the selection and the
    /// clipboard. Held by index, which is the one wart — removing an object
    /// renumbers the ones above it, so a fold can end up on a neighbour. That
    /// is a fold, not data: one click puts it right, and the alternative is an
    /// identity on `Object` that the file format would have to carry for the
    /// sake of a triangle in a panel.
    collapsed: std::collections::HashSet<usize>,
    /// Whether the select tool picks whole objects rather than cells.
    ///
    /// Two modes rather than two tools: they answer the same question — "that
    /// thing there" — at two grains, and the row of tools is already the size
    /// a row of tools should be.
    pub select_objects: bool,
    /// The part the select tool last picked, in object mode.
    ///
    /// Deliberately *not* a `Selection`. A selection is cells on one layer, and
    /// an object spans as many layers as it likes: gathering an object's voxels
    /// into one would silently take only the active layer's share and tear the
    /// part in half on the first move. So this is an object index, and the
    /// transforms it feeds are `move_object` and `rotate_object`, which already
    /// know how to carry a whole subtree.
    pub selected_object: Option<usize>,
    pub selection: Option<Selection>,
    /// Voxels lifted for a paste, if any. Also not part of the document, but
    /// unlike the selection it *does* survive undo — see [`Clipboard`].
    pub clipboard: Option<Clipboard>,
    /// Watching somebody else's document rather than holding one.
    ///
    /// `voxeler attach` sets it. The model belongs to a running `voxeler mcp`,
    /// so nothing here may edit it — the window layer refuses the input and this
    /// flag is what the HUD says so with.
    pub viewing: bool,
    /// Hide everything at or above this Y. `None` shows the whole model.
    pub slice: Option<u16>,

    path: PathBuf,
    dirty: bool,
    status: String,
    drag: Option<Drag>,
    /// The name being typed, while a rename is open. `Some` is a modal state:
    /// the window layer sends keystrokes here instead of to the tools, because
    /// a rename that fired `B` for "build" while you typed "BODY" would be
    /// unusable.
    rename: Option<String>,
}

impl Editor {
    pub fn new(model: VoxelModel, path: PathBuf) -> Self {
        let mut editor = Self {
            document_id: next_document_id(),
            collapsed: Default::default(),
            select_objects: false,
            selected_object: None,
            camera: OrbitCamera::default(),
            tool: Tool::Build,
            span: Span::default(),
            brush: Brush::default(),
            color: 1,
            mirror: [false; 3],
            show_grid: true,
            show_help: false,
            selection: None,
            clipboard: None,
            viewing: false,
            slice: None,
            mesh: FaceMesh::default(),
            mesh_dirty: true,
            history: History::default(),
            dirty: false,
            status: format!("{}", path.display()),
            path,
            model,
            drag: None,
            rename: None,
        };
        editor.frame_volume();
        editor
    }

    pub fn model(&self) -> &VoxelModel {
        &self.model
    }

    /// Changes on document replacement, not on edits or saves. Paired with
    /// the MCP process identity, this lets an agent detect a restarted session.
    pub fn document_id(&self) -> u64 {
        self.document_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// Run something that reports, and put its answer in the status line.
    ///
    /// The keyboard has nowhere else to put a refusal, and a key that silently
    /// does nothing is the failure this editor keeps designing against.
    pub fn report(&mut self, f: impl FnOnce(&mut Self) -> Result<String, String>) {
        self.status = match f(self) {
            Ok(said) => said,
            Err(why) => why,
        };
    }

    /// Switch the select tool between picking cells and picking parts.
    pub fn toggle_select_mode(&mut self) {
        self.select_objects = !self.select_objects;
        self.tool = Tool::Select;
        self.status = if self.select_objects {
            "select: objects".into()
        } else {
            "select: cells".into()
        };
    }

    /// Drop both selections. One key, because "nothing selected" is one idea.
    pub fn clear_all_selection(&mut self) {
        self.selection = None;
        self.selected_object = None;
        self.status = "selection cleared".into();
    }

    /// Paste the clipboard back where it was copied from.
    pub fn paste_in_place(&mut self) {
        let Some(at) = self.clipboard.as_ref().map(|c| c.origin()) else {
            self.status = "nothing copied".into();
            return;
        };
        self.report(|e| e.paste(at).map(|r| format!("pasted {}", r.moved)));
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
    }

    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    pub fn redo_depth(&self) -> usize {
        self.history.redo_depth()
    }

    /// The model-space Y of the work plane — the grid you see, and the height a
    /// build lands at when the ray misses the model.
    ///
    /// The middle of the volume, so the plane passes through the world origin
    /// and the volume is symmetric about it. A model is an object, not a scene:
    /// there is no reason for its ground to be at the bottom of the box, and
    /// putting it in the middle means you can grow the model either way and
    /// the `Y` mirror reflects about a plane you can actually see.
    pub fn ground_y(&self) -> i32 {
        self.model.size()[1] as i32 / 2
    }

    /// The world-space offset that puts the volume's centre on the origin.
    ///
    /// The grid's own coordinates start at zero, but a model that orbits about
    /// its corner is unusable. Centring here rather than in the grid keeps the
    /// stored coordinates unsigned and identical to what the file holds.
    pub fn offset(&self) -> Vec3 {
        let [x, y, z] = self.model.size();
        Vec3 {
            x: -(x as f32) * 0.5,
            y: -(y as f32) * 0.5,
            z: -(z as f32) * 0.5,
        }
    }

    /// The extracted surface, rebuilt only when something has changed it.
    pub fn mesh(&mut self) -> &FaceMesh {
        if self.mesh_dirty {
            // Only the chunks the model says have changed. One voxel placed is
            // one chunk rebuilt — and its neighbour, when the voxel sat against
            // a chunk's face — rather than the whole model.
            extract_dirty(
                &self.model,
                ExtractOptions {
                    y_limit: self.slice.unwrap_or(u16::MAX),
                },
                &mut self.mesh,
            );
            // This is the consumer that has caught up, so this is where the
            // marks are cleared.
            self.model.clear_dirty();
            self.mesh_dirty = false;
        }
        &self.mesh
    }

    fn invalidate_mesh(&mut self) {
        self.mesh_dirty = true;
    }

    // -- picking ---------------------------------------------------------

    /// What the tool would act on for a ray through a pixel, or `None` when
    /// there is nothing there to act on.
    pub fn target_at(&self, px: f32, py: f32, width: u32, height: u32) -> Option<Target> {
        let (origin, dir) = self.camera.ray(px, py, width, height);
        let offset = self.offset();
        // The raycaster works in the grid's own coordinates, so the ray is
        // moved into model space rather than the model into world space.
        let origin = [
            origin.x - offset.x,
            origin.y - offset.y,
            origin.z - offset.z,
        ];
        let dir = [dir.x, dir.y, dir.z];

        // While a stroke is in progress the ray must not see what that stroke
        // has done, or a drag re-aims at its own work — see `Drag::before`.
        let empty = std::collections::HashMap::new();
        let before = self.drag.as_ref().map_or(&empty, |d| &d.before);
        let mask = |cell: [i32; 3]| before.get(&cell).copied();
        let hit =
            voxel_core::raycast::cast_masked(&self.model, origin, dir, self.camera.far, &mask);
        // A slice hides the layers above the cut, and a ray must not pick a
        // voxel that is not on screen. Re-cast from just under the cut instead,
        // so clicking through the opening reaches the cross-section.
        let hit = match (hit, self.slice) {
            (Some(h), Some(limit)) if h.voxel[1] >= limit as i32 => {
                self.cast_below_slice(origin, dir, limit, &mask)
            }
            (h, _) => h,
        };

        let Some(hit) = hit else {
            // Nothing under the cursor. Building may fall back to the work
            // plane — but only where that is the way to start, never as a
            // standing offer. See `plane_is_open`.
            return (self.tool == Tool::Build && self.plane_is_open())
                .then(|| self.ground_target(origin, dir))
                .flatten();
        };

        let cell = match self.tool {
            Tool::Build => hit.adjacent(),
            // Select acts on the voxel you pointed at, like erase and paint:
            // it picks out what is there rather than putting something beside
            // it, so there is no adjacent cell in the question.
            // A sculpt tool works on the material it is aimed at and the air
            // just off it, so the cell it centres on is the one that was hit.
            Tool::Erase | Tool::Paint | Tool::Pick | Tool::Select => hit.voxel,
            Tool::Flatten | Tool::Smooth => hit.voxel,
        };
        Some(Target {
            voxel: hit.voxel,
            face: hit.face,
            index: hit.index,
            cell,
        })
    }

    /// Whether the work plane is currently something you can build on.
    ///
    /// It is open while the **active layer is empty**, and closed once that
    /// layer holds anything. Three things fall out of that one rule:
    ///
    /// - You can always start. An empty layer — a new one, or one you have just
    ///   erased the last voxel of — has nothing to build against, and the plane
    ///   is what rescues it. That was the fallback's whole purpose.
    /// - You can start a *part*. A new layer for a tree is empty, so its first
    ///   voxel goes anywhere on the plane; after that the tree is what you
    ///   build against.
    /// - Empty space stops being clickable the moment there is something to
    ///   aim at. Leaving the plane open turned the entire viewport into a build
    ///   surface, so a click meant for the camera placed a voxel instead —
    ///   which is the bug this rule exists to fix.
    ///
    /// A stroke that began on the plane keeps it open until the button comes
    /// up, or the first placement of a drag would close the plane under the
    /// rest of it.
    fn plane_is_open(&self) -> bool {
        if self.drag.as_ref().is_some_and(|d| d.on_plane) {
            return true;
        }
        // `is_empty` stops at the first filled cell, so the common answer —
        // "no, this layer has something in it" — costs one comparison.
        self.model.layers()[self.model.active_layer()].is_empty()
    }

    /// Where a ray crosses the volume's floor, as a build target.
    ///
    /// Reported as the *top* face of a row of cells one below the volume, so it
    /// is shaped exactly like a hit on a voxel sitting on the floor: the
    /// highlight, the placement and the drag plane all fall out of the same
    /// code with no special case for the ground.
    fn ground_target(&self, origin: [f32; 3], dir: [f32; 3]) -> Option<Target> {
        // A ray running along the plane never lands on it, and one aimed away
        // meets it only behind the viewer.
        if dir[1].abs() < 1e-6 {
            return None;
        }
        let gy = self.ground_y() as f32;
        let t = (gy - origin[1]) / dir[1];
        if t <= 0.0 || t > self.camera.far {
            return None;
        }
        let x = (origin[0] + dir[0] * t).floor() as i32;
        let z = (origin[2] + dir[2] * t).floor() as i32;

        // Place against the side of the plane you are looking at, the same way
        // a build places against the face of a voxel it can see: from above the
        // new voxel sits on the plane, from below it hangs under it.
        let (voxel, face, cell) = if dir[1] < 0.0 {
            let gy = self.ground_y();
            ([x, gy - 1, z], Face::PosY, [x, gy, z])
        } else {
            let gy = self.ground_y();
            ([x, gy, z], Face::NegY, [x, gy - 1, z])
        };
        if !self.model.contains(cell[0], cell[1], cell[2]) {
            return None;
        }
        Some(Target {
            voxel,
            face,
            index: 0,
            cell,
        })
    }

    /// Re-cast against a copy of the model with the hidden layers removed.
    ///
    /// Building a temporary model is the honest way to do this: the alternative
    /// is a second traversal that skips cells by height, which duplicates the
    /// traversal logic for one caller. A slice click is a user action at human
    /// speed, so the copy costs nothing anyone can perceive.
    fn cast_below_slice(
        &self,
        origin: [f32; 3],
        dir: [f32; 3],
        limit: u16,
        mask: &dyn Fn([i32; 3]) -> Option<u8>,
    ) -> Option<RayHit> {
        let mut sliced = self.model.clone();
        let [sx, sy, sz] = sliced.size();
        for y in limit..sy {
            for z in 0..sz {
                for x in 0..sx {
                    sliced.set(x as i32, y as i32, z as i32, 0);
                }
            }
        }
        voxel_core::raycast::cast_masked(&sliced, origin, dir, self.camera.far, mask)
    }

    // -- editing ---------------------------------------------------------

    /// Begin a stroke and apply the tool once.
    pub fn begin_stroke(&mut self, target: Target) {
        let label = match self.tool {
            // Selecting is not an edit and has no stroke: `app.rs` handles it
            // on mouse-down, the way a click on a panel is handled — a choice,
            // never something to undo.
            Tool::Select => return,
            Tool::Build => "build",
            Tool::Erase => "erase",
            Tool::Paint => "paint",
            Tool::Flatten => "flatten",
            Tool::Smooth => "smooth",
            Tool::Pick => {
                if target.index != 0 {
                    self.color = target.index;
                    self.status = format!("picked colour {}", self.color);
                }
                return;
            }
        };
        self.drag = Some(Drag {
            stroke: Stroke::new(label),
            reference: (self.tool == Tool::Flatten).then(|| (target.voxel, target.face.normal())),
            // Build is the only tool that grows the surface it is aimed at, so
            // it is the only one that needs pinning; erase and paint follow the
            // pointer over whatever it is actually over.
            plane: (self.tool == Tool::Build)
                .then(|| (target.face.axis(), target.cell[target.face.axis()])),
            last_cell: None,
            owner: self
                .model
                .owner_at(target.voxel[0], target.voxel[1], target.voxel[2]),
            on_plane: target.is_ground(),
            before: std::collections::HashMap::new(),
        });
        self.continue_stroke(target);
    }

    /// Whether the current span reaches only so far from where it is aimed.
    ///
    /// A voxel span is the brush and is bounded by construction. Any other span
    /// grows, and is bounded only when the brush gives it a radius. This is the
    /// seam between a click and a stroke — see `continue_stroke`.
    pub fn region_is_bounded(&self) -> bool {
        self.span == Span::Voxel || self.brush.radius > 0
    }

    /// Apply the tool again, part-way through a drag.
    pub fn continue_stroke(&mut self, target: Target) {
        let Some(drag) = &self.drag else { return };
        if drag.last_cell == Some(target.cell) {
            return;
        }
        // An *unbounded* region is a click, not a stroke. It already reached
        // everything connected to the cell it was aimed at, so re-running it as
        // the pointer moves would re-flood from a new seed several times a
        // frame and turn one intended fill into a wandering pile of them.
        //
        // The rule used to be "any span but Voxel", which named the wrong
        // thing. What makes a fill unstrokeable is that it has no limit, not
        // that it is a flood: a flood with a radius covers a patch the size you
        // chose, and dragging one is exactly how a surface gets worked. So the
        // test is boundedness, and the brush radius is what supplies it.
        if !self.region_is_bounded() && drag.last_cell.is_some() {
            return;
        }
        if let Some((axis, coord)) = drag.plane {
            if target.cell[axis] != coord {
                return;
            }
        }
        if matches!(self.tool, Tool::Pick | Tool::Select) {
            // Neither writes anything, so neither reaches a stroke.
            return;
        }
        let reference = drag.reference;

        // Resolved before the stroke is borrowed: the span reads the model, and
        // writing through the stroke needs it mutably.
        let cells = self.write_cells(target, reference);
        let layer = self.model.active_layer();
        let (tool, color) = (self.tool, self.color);

        // Every decision read before any of them is applied. It matters for
        // `Smooth`, whose answer at a cell depends on its neighbours: written
        // as it goes, one pass would cascade into itself and eat a surface in a
        // single application. The same rule `transform_selection` follows.
        let writes: Vec<([i32; 3], u8)> = cells
            .into_iter()
            .filter_map(|c| {
                sculpt_value(&self.model, tool, color, c, layer, reference).map(|v| (c, v))
            })
            .collect();

        let Some(drag) = &mut self.drag else { return };
        drag.last_cell = Some(target.cell);
        for (cell @ [x, y, z], value) in writes {
            // The composite, not the layer: what the *ray* would have found
            // here before this stroke ran.
            let was = self.model.get(x, y, z);
            drag.before.entry(cell).or_insert(was);
            drag.stroke.set(&mut self.model, x, y, z, value);
        }
        self.invalidate_mesh();
    }

    /// Every cell this application of the tool may write to: the span around
    /// the target, and each of its reflections.
    fn write_cells(
        &self,
        target: Target,
        reference: Option<([i32; 3], [i32; 3])>,
    ) -> Vec<[i32; 3]> {
        // A sculpt tool works over a neighbourhood in three dimensions, not
        // along a surface: it has to see the material behind a cell as well as
        // the air in front. So it takes the brush ball whatever the span row
        // says, and the chip says as much.
        // A voxel span is a shape and ignores `matches`, so the colour rule
        // below is inert for these two — which is what lets them reach the air
        // they have to fill.
        let span = match self.tool {
            Tool::Flatten | Tool::Smooth => Span::Voxel,
            _ => self.span,
        };
        // Flatten's brush centre rides the locked plane rather than the ray.
        // Left on the ray it would sink as it carved — each pass exposing a
        // deeper cell for the next to centre on — and a stroke that walks
        // itself into the model is not levelling anything.
        let seed = match reference {
            Some((origin, normal)) => {
                let d: i32 = (0..3)
                    .map(|a| (target.cell[a] - origin[a]) * normal[a])
                    .sum();
                std::array::from_fn(|a| target.cell[a] - normal[a] * d)
            }
            None => target.cell,
        };
        let reach = region::Reach {
            seed,
            face: target.face,
            // Building grows over air; erasing and painting grow over the
            // colour under the cursor, so a region stops where the colour does.
            matches: region::Match::Index(if self.tool == Tool::Build {
                0
            } else {
                target.index
            }),
            brush: self.brush,
            grounded: target.is_ground(),
            // An edit must not reach a layer the slice has taken off screen.
            y_limit: self.slice.unwrap_or(u16::MAX),
            // A click has no box to be held inside; only a selection does.
            within: None,
            // The composite: a click selects what the user can see, and they
            // can see which layers are stacked and undo if it was not what
            // they meant. See `write_cells`.
            layer: None,
        };
        // The region is grown over the *composite*: you point at what you can
        // see, so that is what a fill selects. What it writes is then narrowed
        // to the active layer by the rule above. With one layer, or while
        // working on the layer you are looking at, the two are the same set.
        let cells = region::cells(&self.model, span, reach);
        if self.mirror == [false; 3] {
            return cells;
        }
        let mut out = Vec::with_capacity(cells.len() * self.mirror_count());
        for cell in cells {
            self.reflect_into(cell, &mut out);
        }
        out
    }

    fn mirror_count(&self) -> usize {
        1 << self.mirror.iter().filter(|m| **m).count()
    }

    /// A cell and every reflection of it across the active mirror planes.
    ///
    /// Two axes give four cells and three give eight. A cell lying *on* a mirror
    /// plane is its own reflection, and the duplicate is left in: `Stroke::set`
    /// drops a write that changes nothing, so the second one costs an iteration
    /// rather than an extra entry in the undo step.
    fn reflect_into(&self, cell: [i32; 3], out: &mut Vec<[i32; 3]>) {
        let size = self.model.size();
        let start = out.len();
        out.push(cell);
        for axis in 0..3 {
            if !self.mirror[axis] {
                continue;
            }
            for i in start..out.len() {
                let mut p = out[i];
                p[axis] = size[axis] as i32 - 1 - p[axis];
                out.push(p);
            }
        }
    }

    /// Finish the drag, committing it as one undo step.
    pub fn end_stroke(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        // How many cells the edit actually changed, which for a fill is the
        // only feedback there is: a span is not previewed before the click, so
        // the count is what tells a fill of nine from a fill of nine hundred.
        let changed = drag.stroke.len();
        let owner = drag.owner;
        let label = if self.history.push(drag.stroke) {
            self.dirty = true;
            self.refresh_instances();
            Some(self.tool.name())
        } else {
            None
        };
        match label {
            Some(label) => {
                self.status = format!(
                    "{label} {changed} cells, {} voxels",
                    self.model.filled_count()
                );
            }
            // Nothing changed. If the voxel that was clicked belongs to some
            // other layer, that is why, and saying so is the difference between
            // a rule the user can learn and an editor that ignores them.
            None => {
                if let Some(owner) = owner.filter(|o| *o != self.model.active_layer()) {
                    self.status = format!(
                        "{} holds that voxel — select it to edit",
                        self.model.layers()[owner].name
                    );
                }
            }
        }
    }

    pub fn undo(&mut self) {
        // A selection names coordinates, and an undo can change what is at them
        // arbitrarily — including putting back voxels a move took away. Keeping
        // it would leave a selection pointing at cells that are no longer the
        // ones it was made from, which is worse than asking for it again.
        self.selection = None;
        match self.history.undo(&mut self.model) {
            Some(label) => {
                self.dirty = true;
                // An instance's cells are derived, so they are not in the
                // history at all: putting the source back and rebuilding is
                // what keeps the copies in step instead of restoring four
                // stale ones.
                self.refresh_instances();
                self.invalidate_mesh();
                self.status = format!("undo {label}");
            }
            None => self.status = "nothing to undo".into(),
        }
    }

    pub fn redo(&mut self) {
        self.selection = None;
        match self.history.redo(&mut self.model) {
            Some(label) => {
                self.dirty = true;
                // An instance's cells are derived, so they are not in the
                // history at all: putting the source back and rebuilding is
                // what keeps the copies in step instead of restoring four
                // stale ones.
                self.refresh_instances();
                self.invalidate_mesh();
                self.status = format!("redo {label}");
            }
            None => self.status = "nothing to redo".into(),
        }
    }

    // -- selection -------------------------------------------------------

    /// Select the voxels of the active layer inside a box.
    ///
    /// The voxels, not the box: air inside it is not selected, so moving the
    /// result carries the shape and not a cube of nothing around it.
    pub fn select_box(&mut self, from: [i32; 3], to: [i32; 3]) -> usize {
        let layer = self.model.active_layer();
        let (mut lo, mut hi) = ([0i32; 3], [0i32; 3]);
        for a in 0..3 {
            lo[a] = from[a].min(to[a]);
            hi[a] = from[a].max(to[a]);
        }
        let mut cells = Vec::new();
        for z in lo[2]..=hi[2] {
            for y in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    if self.model.get_in(layer, x, y, z) != 0 {
                        cells.push([x, y, z]);
                    }
                }
            }
        }
        self.set_selection(Selection::new(layer, cells))
    }

    /// Select the connected piece of the active layer containing a cell.
    ///
    /// Connected by *material*, not by colour: an arm is one part whether or
    /// not the glove on the end of it is a different index, and a selection
    /// that stopped at the wrist would be the wrong answer.
    /// `within`, when given, holds the growth inside a box. A part of a figure
    /// is connected to the rest of it, so connectivity alone can only ever
    /// answer "the whole figure" — the box is what makes "this arm" sayable.
    pub fn select_connected(&mut self, at: [i32; 3], within: Option<Bounds>) -> usize {
        let layer = self.model.active_layer();
        if self.model.get_in(layer, at[0], at[1], at[2]) == 0 {
            self.selection = None;
            self.status = format!("nothing at {at:?} on this layer");
            return 0;
        }
        let cells = region::cells(
            &self.model,
            region::Span::Volume,
            region::Reach {
                seed: at,
                // Volume growth never consults the face; any of the six gives
                // the same region.
                face: Face::PosY,
                matches: region::Match::Solid,
                brush: region::Brush::default(),
                grounded: false,
                y_limit: self.slice.unwrap_or(u16::MAX),
                within,
                layer: Some(layer),
            },
        );
        self.set_selection(Selection::new(layer, cells))
    }

    /// Select what the current span reaches from a cell.
    ///
    /// The same `region` walk the drawing tools use, so the span row and the
    /// brush mean here exactly what they mean everywhere else — one cell, a
    /// run, a face, the connected part. Matched on **material** rather than
    /// colour, for the reason `select_connected` is: an arm is one part whether
    /// or not the glove on the end of it is a different index.
    pub fn select_with_span(&mut self, at: [i32; 3], face: Face) -> usize {
        let layer = self.model.active_layer();
        if self.model.get_in(layer, at[0], at[1], at[2]) == 0 {
            self.selection = None;
            self.status = match self.model.owner_at(at[0], at[1], at[2]) {
                Some(owner) => format!(
                    "{} holds that voxel — select it first",
                    self.model.layers()[owner].name
                ),
                None => "nothing there to select".into(),
            };
            return 0;
        }
        let cells = region::cells(
            &self.model,
            self.span,
            region::Reach {
                seed: at,
                face,
                matches: region::Match::Solid,
                brush: self.brush,
                grounded: false,
                y_limit: self.slice.unwrap_or(u16::MAX),
                within: None,
                layer: Some(layer),
            },
        );
        self.set_selection(Selection::new(layer, cells))
    }

    /// Select every voxel of a layer.
    ///
    /// The one selection tool from the original proposal that never shipped.
    /// "All of it" is the commonest selection there is, and building it out of
    /// a flood fill needs a seed the user has to find first.
    pub fn select_all_in_layer(&mut self, layer: usize) -> usize {
        if layer >= self.model.layer_count() {
            self.status = format!("there is no layer {layer}");
            return 0;
        }
        let cells: Vec<[i32; 3]> = self
            .model
            .iter_filled_in(layer)
            .map(|([x, y, z], _)| [i32::from(x), i32::from(y), i32::from(z)])
            .collect();
        self.set_selection(Selection::new(layer, cells))
    }

    // -- picking a part rather than cells ---------------------------------

    /// Pick the object that owns a cell, for the select tool in object mode.
    pub fn select_object_at(&mut self, at: [i32; 3]) -> Option<usize> {
        let owner = self.model.owner_at(at[0], at[1], at[2])?;
        let object = self.model.layers()[owner].object;
        self.select_object(Some(object));
        Some(object)
    }

    /// Set or clear the selected object.
    pub fn select_object(&mut self, object: Option<usize>) {
        self.selected_object = object.filter(|i| *i < self.model.object_count());
        self.status = match self.selected_object {
            Some(i) => format!("selected {}", self.object_name(i)),
            None => "no object selected".into(),
        };
    }

    /// The box the selected object's voxels occupy, for the outline.
    ///
    /// Every layer of the subtree, not just the active one — the outline has to
    /// show what a move would actually carry.
    pub fn selected_object_bounds(&self) -> Option<([i32; 3], [i32; 3])> {
        let object = self.selected_object?;
        let subtree = self.model.subtree(object);
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for (n, layer) in self.model.layers().iter().enumerate() {
            if !subtree.contains(&layer.object) || layer.filled_count() == 0 {
                continue;
            }
            let _ = n;
            let b = layer.occupied();
            let end = b.end();
            for a in 0..3 {
                lo[a] = lo[a].min(i32::from(b.origin[a]));
                hi[a] = hi[a].max(end[a] - 1);
            }
        }
        (lo[0] <= hi[0]).then_some((lo, hi))
    }

    fn set_selection(&mut self, selection: Selection) -> usize {
        let n = selection.len();
        self.selection = (n > 0).then_some(selection);
        self.status = if n > 0 {
            format!("selected {n} voxels")
        } else {
            "selected nothing".into()
        };
        n
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.status = "selection cleared".into();
    }

    /// Move the selected voxels, as one undo step.
    pub fn move_selection(&mut self, delta: [i32; 3]) -> Result<MoveReport, String> {
        if delta == [0, 0, 0] {
            return self
                .selection
                .as_ref()
                .map(|_| MoveReport::default())
                .ok_or_else(no_selection);
        }
        self.transform_selection("move selection", |c| {
            [c[0] + delta[0], c[1] + delta[1], c[2] + delta[2]]
        })
    }

    /// Mirror the selected voxels about the middle of their own box.
    ///
    /// About the selection, not the scene: `M` mirrors an *edit* across the
    /// model's middle, which is a different thing, and flipping a hand you have
    /// selected should turn the hand over rather than send it to the far side
    /// of the volume.
    ///
    /// Exact at any size. `lo + hi - v` needs no centre cell, so an even extent
    /// flips without the half-cell rounding a rotation cannot avoid.
    pub fn flip_selection(&mut self, axis: usize) -> Result<MoveReport, String> {
        let Some((lo, hi)) = self.selection.as_ref().and_then(|s| s.bounds()) else {
            return Err(no_selection());
        };
        let sum = lo[axis] + hi[axis];
        self.transform_selection("flip selection", move |mut c| {
            c[axis] = sum - c[axis];
            c
        })
    }

    /// Turn the selected voxels a quarter turn at a time about their own box.
    ///
    /// Counter-clockwise about the **positive** axis, by the right-hand rule —
    /// the same convention face winding uses (`e_b × e_c = e_a`), so there is
    /// one sense of "positive rotation" in this codebase rather than two.
    ///
    /// The selection pivots about the **low corner** of its box, not its
    /// centre. Centring reads better on paper and is not invertible: a quarter
    /// turn swaps two extents, and where those differ in parity the centre
    /// falls between cells and has to be rounded. Rounding the same way each
    /// time accumulates, so `rotate(+1)` then `rotate(-1)` came back a whole
    /// cell from where it started. Turning something to look at it and turning
    /// it back is a thing people do constantly, and it has to be exact.
    ///
    /// A square footprint pivots identically either way, which is most
    /// rotations. For one that is not square, follow with `move_selection`.
    pub fn rotate_selection(
        &mut self,
        axis: usize,
        quarter_turns: i32,
    ) -> Result<MoveReport, String> {
        let Some((lo, hi)) = self.selection.as_ref().and_then(|s| s.bounds()) else {
            return Err(no_selection());
        };
        let turns = quarter_turns.rem_euclid(4);
        if turns == 0 {
            return Ok(MoveReport::default());
        }
        // The two axes the rotation moves, in the cyclic order that makes a
        // positive turn positive.
        let (b, c) = ((axis + 1) % 3, (axis + 2) % 3);
        let (eb, ec) = (hi[b] - lo[b] + 1, hi[c] - lo[c] + 1);
        let (anchor_b, anchor_c) = (lo[b], lo[c]);
        let _ = hi;

        self.transform_selection("rotate selection", move |cell| {
            let (mut db, mut dc) = (cell[b] - lo[b], cell[c] - lo[c]);
            let (mut wb, mut wc) = (eb, ec);
            for _ in 0..turns {
                let (nb, nc) = (wc - 1 - dc, db);
                db = nb;
                dc = nc;
                std::mem::swap(&mut wb, &mut wc);
            }
            let _ = wb;
            let mut out = cell;
            out[b] = anchor_b + db;
            out[c] = anchor_c + dc;
            out
        })
    }

    /// Lift the selected voxels into the clipboard, leaving the model alone.
    pub fn copy_selection(&mut self) -> Result<usize, String> {
        let Some(selection) = self.selection.as_ref() else {
            return Err(no_selection());
        };
        let Some((lo, hi)) = selection.bounds() else {
            return Err(no_selection());
        };
        let layer = selection.layer();
        let cells: Vec<([i32; 3], u8)> = selection
            .cells()
            .map(|c| {
                (
                    [c[0] - lo[0], c[1] - lo[1], c[2] - lo[2]],
                    self.model.get_in(layer, c[0], c[1], c[2]),
                )
            })
            .collect();
        let n = cells.len();
        self.clipboard = Some(Clipboard {
            cells,
            size: [hi[0] - lo[0] + 1, hi[1] - lo[1] + 1, hi[2] - lo[2] + 1],
            origin: lo,
        });
        self.status = format!("copied {n} voxels");
        Ok(n)
    }

    /// Copy, then clear what was copied — one undo step for the clearing.
    pub fn cut_selection(&mut self) -> Result<usize, String> {
        let n = self.copy_selection()?;
        let selection = self.selection.clone().ok_or_else(no_selection)?;
        let layer = selection.layer();
        let writes: Vec<CellWrite> = selection
            .cells()
            .map(|pos| CellWrite {
                layer,
                pos,
                color: 0,
            })
            .collect();
        self.apply_writes("cut selection", writes, |_, _| {});
        // Nothing is there any more, so nothing is selected. The clipboard is
        // what holds the voxels now.
        self.selection = None;
        self.status = format!("cut {n} voxels");
        Ok(n)
    }

    /// Write the clipboard into the **active layer**, its low corner at `at`.
    ///
    /// The active layer rather than the one it came from: that is what makes
    /// copying between layers a paste instead of a separate tool, and it is the
    /// rule every other tool follows.
    ///
    /// What lands is left selected, so a paste can be moved, turned or pasted
    /// again without saying where it went.
    pub fn paste(&mut self, at: [i32; 3]) -> Result<MoveReport, String> {
        let Some(clipboard) = self.clipboard.clone() else {
            return Err("the clipboard is empty; copy or cut something first".into());
        };
        let layer = self.model.active_layer();

        let mut report = MoveReport::default();
        let mut writes = Vec::with_capacity(clipboard.cells.len());
        let mut landed = Vec::with_capacity(clipboard.cells.len());
        for (offset, color) in &clipboard.cells {
            let to = [at[0] + offset[0], at[1] + offset[1], at[2] + offset[2]];
            if !self.model.contains(to[0], to[1], to[2]) {
                report.dropped += 1;
                continue;
            }
            report.moved += 1;
            if self.model.get_in(layer, to[0], to[1], to[2]) != 0 {
                report.overwritten += 1;
            }
            landed.push(to);
            writes.push(CellWrite {
                layer,
                pos: to,
                color: *color,
            });
        }

        self.apply_writes("paste", writes, |_, _| {});
        self.selection = (!landed.is_empty()).then(|| Selection::new(layer, landed));
        self.status = format!(
            "pasted {} voxels{}",
            report.moved,
            if report.dropped > 0 {
                format!(", {} lost off the edge", report.dropped)
            } else {
                String::new()
            }
        );
        Ok(report)
    }

    /// Copy the selection and paste it at an offset, in one step.
    ///
    /// The common case — a second wheel, a mirrored limb — without having to
    /// name the corner the original happened to sit at. The copy lands
    /// selected, so `duplicate` then `flip` is the whole of a mirrored pair.
    pub fn duplicate_selection(&mut self, delta: [i32; 3]) -> Result<MoveReport, String> {
        let Some((lo, _)) = self.selection.as_ref().and_then(|s| s.bounds()) else {
            return Err(no_selection());
        };
        self.copy_selection()?;
        self.paste([lo[0] + delta[0], lo[1] + delta[1], lo[2] + delta[2]])
    }

    /// Apply a cell mapping to the selection, as one undo step.
    ///
    /// The shape every transform has. Colours are read **before anything
    /// moves**, then the clears are emitted ahead of the writes in one batch: a
    /// transform whose result overlaps its source — a short move, a rotation of
    /// a squat shape — would otherwise carry a voxel along instead of leaving
    /// it where it landed, or erase its own arrival.
    fn transform_selection(
        &mut self,
        label: &'static str,
        map: impl Fn([i32; 3]) -> [i32; 3],
    ) -> Result<MoveReport, String> {
        let Some(selection) = self.selection.clone() else {
            return Err(no_selection());
        };
        let layer = selection.layer;
        if layer >= self.model.layer_count() {
            return Err("the selected layer is gone".into());
        }

        let carried: Vec<([i32; 3], u8)> = selection
            .cells()
            .map(|c| (c, self.model.get_in(layer, c[0], c[1], c[2])))
            .collect();
        let source: std::collections::HashSet<[i32; 3]> = selection.cells().collect();

        let mut report = MoveReport::default();
        let mut writes: Vec<CellWrite> = carried
            .iter()
            .map(|(pos, _)| CellWrite {
                layer,
                pos: *pos,
                color: 0,
            })
            .collect();
        let mut landed = Vec::with_capacity(carried.len());
        for (pos, color) in &carried {
            let to = map(*pos);
            if !self.model.contains(to[0], to[1], to[2]) {
                report.dropped += 1;
                continue;
            }
            report.moved += 1;
            if !source.contains(&to) && self.model.get_in(layer, to[0], to[1], to[2]) != 0 {
                report.overwritten += 1;
            }
            landed.push(to);
            writes.push(CellWrite {
                layer,
                pos: to,
                color: *color,
            });
        }

        self.apply_writes(label, writes, |_, _| {});
        // The selection follows its voxels, so a transform can be repeated or
        // refined. Cells that fell outside the scene are simply gone.
        self.selection = (!landed.is_empty()).then(|| Selection::new(layer, landed));
        self.status = format!(
            "{} {} voxels{}",
            label.split(' ').next().unwrap_or(label),
            report.moved,
            if report.dropped > 0 {
                format!(", {} lost off the edge", report.dropped)
            } else {
                String::new()
            }
        );
        Ok(report)
    }

    /// Scale the whole scene up, so every voxel becomes `factor`³ of them.
    ///
    /// One undo step covering every layer at once, which is why it goes through
    /// `restructure` rather than a stroke: it changes the scene's own size, and
    /// a snapshot is the only record that can put that back.
    ///
    /// The camera and the slice are moved with it. Neither is part of the
    /// document, but both are measured in voxels: leaving the camera alone
    /// would make the model appear to leap towards you, and leaving the slice
    /// would put the cut through a different part of the shape than the one you
    /// were looking at.
    pub fn subdivide(&mut self, factor: u16) -> Result<(), String> {
        let mut failed = None;
        let changed = self
            .history
            .restructure(&mut self.model, "subdivide", |model| {
                failed = model.subdivide(factor).err();
            });
        if let Some(e) = failed {
            self.status = e.clone();
            return Err(e);
        }
        if !changed {
            self.status = "nothing to subdivide".into();
            return Ok(());
        }
        self.camera.distance *= factor as f32;
        self.slice = self.slice.map(|s| s.saturating_mul(factor));
        self.after_structural();
        let [x, y, z] = self.model.size();
        self.status = format!(
            "subdivided by {factor} — {x}x{y}x{z}, {} voxels",
            self.model.filled_count()
        );
        Ok(())
    }

    /// Empty the model as one undoable step.
    ///
    /// Every layer, not the visible cells of the active one: `ctrl+N` is "start
    /// this model again", and leaving the hidden layers full would make the
    /// next save carry work the user believes they threw away.
    pub fn clear(&mut self) {
        let filled: Vec<Vec<_>> = (0..self.model.layer_count())
            .map(|n| self.model.iter_filled_in(n).map(|(p, _)| p).collect())
            .collect();
        let changed = self
            .history
            .edit(&mut self.model, "clear", |model, stroke| {
                for (n, cells) in filled.into_iter().enumerate() {
                    for [x, y, z] in cells {
                        stroke.set_in(model, n, x as i32, y as i32, z as i32, 0);
                    }
                }
            });
        if changed {
            self.dirty = true;
            self.invalidate_mesh();
        }
        self.status = "cleared".into();
    }

    // -- layers ----------------------------------------------------------

    pub fn active_layer(&self) -> usize {
        self.model.active_layer()
    }

    fn layer_name(&self, i: usize) -> String {
        self.model.layers()[i].name.clone()
    }

    fn report_layer(&mut self) {
        let i = self.model.active_layer();
        let b = self.model.layer_bounds(i);
        let extent = if b.is_empty() {
            "empty".to_string()
        } else {
            let [w, h, d] = b.size;
            let [x, y, z] = b.origin;
            format!("{w}x{h}x{d} at {x},{y},{z}")
        };
        self.status = format!(
            "layer {}/{}: {} ({extent})",
            i + 1,
            self.model.layer_count(),
            self.layer_name(i)
        );
    }

    /// Shrink the active layer's box to the voxels it actually holds.
    ///
    /// Not automatic on erase: a box keeps its high-water mark while you work,
    /// so erasing and redrawing in one spot does not reallocate the layer every
    /// stroke. This is the "I am done with that area" button.
    pub fn trim_layer(&mut self) {
        let i = self.model.active_layer();
        self.trim_layer_at(i);
    }

    /// The same, on a layer the caller names.
    pub fn trim_layer_at(&mut self, i: usize) {
        let before = self.model.layer_bounds(i);
        if !self.model.trim_layer(i) {
            self.status = "the layer already fits its contents".into();
            return;
        }
        let after = self.model.layer_bounds(i);
        // Not an undo step and not a change to the model: the voxels are
        // identical either side, and only how much room is set aside for them
        // has moved. It does dirty the document, because the file records it.
        self.dirty = true;
        self.status = format!(
            "trimmed {} from {} to {} cells",
            self.layer_name(i),
            before.cells(),
            after.cells()
        );
    }

    /// Select a layer outright — what a click on the panel does.
    pub fn select_layer(&mut self, i: usize) {
        // The same gate the MCP surface uses. A derived layer never becomes
        // active, which is what keeps every active-layer tool — build, erase,
        // paint, fill, paste, and every selection made from it — off an
        // instance without a check of its own.
        if let Some(why) = self.generated_refusal(i) {
            self.status = why;
            return;
        }
        self.model.set_active_layer(i);
        self.report_layer();
    }

    /// Step the active layer, clamped rather than wrapped.
    ///
    /// Wrapping would put the top of the stack one key away from the bottom,
    /// and a stack is a thing with ends — running off one and finding yourself
    /// at the other is how an edit lands on the wrong layer.
    /// Whether `i` is a layer the editor will write to.
    fn writable_layer(&self, i: usize) -> bool {
        self.model
            .layers()
            .get(i)
            .is_some_and(|l| !l.is_generated())
    }

    pub fn cycle_layer(&mut self, delta: i32) {
        let n = self.model.layer_count() as i32;
        let mut next = (self.model.active_layer() as i32 + delta).clamp(0, n - 1);
        // Step over an instance's copy rather than stopping on one. It cannot
        // be made active, so landing there would make the key look broken; the
        // step it costs is the one thing it can do instead.
        while !self.writable_layer(next as usize) {
            let after = next + delta.signum();
            if !(0..n).contains(&after) {
                return;
            }
            next = after;
        }
        self.model.set_active_layer(next as usize);
        self.report_layer();
    }

    /// Add an empty layer above the active one and select it.
    pub fn add_layer(&mut self) {
        // Named for its position at the moment it is made. Two layers can end
        // up sharing a name after a reorder, which is untidy but honest — the
        // alternative is renaming layers behind the user's back.
        let name = format!("LAYER {}", self.model.layer_count() + 1);
        self.insert_layer(name, Bounds::default());
    }

    /// The same, with a name the caller chose — what an agent driving the
    /// editor over MCP wants, having a purpose in mind that "LAYER 3" does not
    /// record.
    pub fn add_named_layer(&mut self, name: &str, bounds: Bounds) {
        self.insert_layer(name.to_string(), bounds);
    }

    /// A layer with a box declared up front but no name of its own.
    pub fn add_layer_with(&mut self, bounds: Bounds) {
        let name = format!("LAYER {}", self.model.layer_count() + 1);
        self.insert_layer(name, bounds);
    }

    fn insert_layer(&mut self, name: String, bounds: Bounds) {
        let mut added = None;
        let changed = self
            .history
            .restructure(&mut self.model, "add layer", |model| {
                added = model.add_layer_with(model.active_layer(), name, bounds);
                if let Some(i) = added {
                    model.set_active_layer(i);
                }
            });
        if changed {
            self.after_structural();
            self.report_layer();
        } else {
            self.status = format!("at the {}-layer limit", voxel_core::MAX_LAYERS);
        }
    }

    pub fn delete_layer(&mut self) {
        let name = self.layer_name(self.model.active_layer());
        let changed = self
            .history
            .restructure(&mut self.model, "delete layer", |model| {
                model.remove_layer(model.active_layer());
            });
        if changed {
            self.after_structural();
            self.status = format!("deleted {name} — ctrl+Z brings it back");
        } else {
            self.status = "a model needs one layer".into();
        }
    }

    /// Move the active layer up or down the stack, changing what covers what.
    pub fn move_layer(&mut self, up: bool) {
        let changed = self
            .history
            .restructure(&mut self.model, "move layer", |model| {
                model.move_layer(model.active_layer(), up);
            });
        if changed {
            self.after_structural();
            self.report_layer();
        } else {
            self.status = if up {
                "already on top"
            } else {
                "already at the bottom"
            }
            .into();
        }
    }

    pub fn merge_layer_down(&mut self) {
        let name = self.layer_name(self.model.active_layer());
        let changed = self
            .history
            .restructure(&mut self.model, "merge layer", |model| {
                model.merge_down(model.active_layer());
            });
        if changed {
            self.after_structural();
            self.status = format!("merged {name} down");
        } else {
            self.status = "the bottom layer has nothing to merge into".into();
        }
    }

    // -- objects ----------------------------------------------------------

    /// Add an object under `parent`. Structural, so it is one undo step.
    pub fn add_object(&mut self, parent: usize, name: &str) -> bool {
        let name = name.to_string();
        let changed = self
            .history
            .restructure(&mut self.model, "add object", |model| {
                model.add_object(parent, name);
            });
        if changed {
            self.after_structural();
            self.status = format!("added {}", self.object_name(self.model.object_count() - 1));
        } else {
            self.status = format!("at the {}-object limit", voxel_core::MAX_OBJECTS);
        }
        changed
    }

    /// Remove an object, leaving its children and layers with its parent.
    pub fn remove_object(&mut self, i: usize) -> bool {
        let name = self.object_name(i);
        let changed = self
            .history
            .restructure(&mut self.model, "delete object", |model| {
                model.remove_object(i);
            });
        if changed {
            self.after_structural();
            self.status = format!("removed {name} — its layers moved up, ctrl+Z brings it back");
        } else {
            self.status = "the scene root cannot be removed".into();
        }
        changed
    }

    pub fn rename_object(&mut self, i: usize, name: &str) -> bool {
        if i >= self.model.object_count() {
            return false;
        }
        let name = name.to_string();
        self.history
            .restructure(&mut self.model, "rename object", |model| {
                model.rename_object(i, name);
            });
        self.after_structural();
        true
    }

    /// Show or hide an object, and with it every layer inside it.
    ///
    /// Deliberately outside the history, for the reason layer visibility is: it
    /// is toggled constantly while working, and undo would spend its first few
    /// presses turning things back on instead of undoing the edit you wanted
    /// back. It still dirties the document, because what you had hidden is part
    /// of the model.
    pub fn set_object_visible(&mut self, i: usize, visible: bool) -> bool {
        if i >= self.model.object_count() {
            return false;
        }
        self.model.set_object_visible(i, visible);
        self.dirty = true;
        self.invalidate_mesh();
        self.status = format!(
            "{} {}",
            self.object_name(i),
            if visible { "shown" } else { "hidden" }
        );
        true
    }

    pub fn reparent_object(&mut self, i: usize, parent: usize) -> bool {
        let mut ok = false;
        self.history
            .restructure(&mut self.model, "reparent object", |model| {
                ok = model.reparent_object(i, parent);
            });
        if ok {
            self.after_structural();
            self.status = format!(
                "{} is now part of {}",
                self.object_name(i),
                self.object_name(parent)
            );
        } else {
            self.status = "an object cannot be part of itself".into();
        }
        ok
    }

    pub fn set_layer_object(&mut self, layer: usize, object: usize) -> bool {
        let mut ok = false;
        self.history
            .restructure(&mut self.model, "move layer to object", |model| {
                ok = model.set_layer_object(layer, object);
            });
        if ok {
            self.after_structural();
            self.status = format!(
                "{} is now part of {}",
                self.layer_name(layer),
                self.object_name(object)
            );
        }
        ok
    }

    /// Move an object and everything under it. One undo step for the lot.
    /// Turn an object and everything under it a quarter turn at a time.
    ///
    /// One undo step for the whole subtree, through the same snapshot path a
    /// move takes: a rotation rewrites grids rather than sliding boxes, so
    /// there is nothing cheaper to record than the stack either side of it.
    pub fn rotate_object(
        &mut self,
        i: usize,
        axis: usize,
        quarter_turns: i32,
    ) -> Result<usize, String> {
        // Checked on a copy first, so a refusal does not spend an undo on a
        // snapshot of a model that did not change.
        self.model.clone().rotate_object(i, axis, quarter_turns)?;
        let mut result = Err("nothing happened".to_string());
        self.history
            .restructure(&mut self.model, "rotate object", |model| {
                result = model.rotate_object(i, axis, quarter_turns);
            });
        if let Ok(n) = &result {
            self.after_structural();
            self.status = format!(
                "turned {} {} quarter turn{} about {} — {n} layer{}",
                self.object_name(i),
                quarter_turns,
                if quarter_turns.abs() == 1 { "" } else { "s" },
                ["x", "y", "z"][axis],
                if *n == 1 { "" } else { "s" }
            );
        }
        result
    }

    pub fn move_object(&mut self, i: usize, delta: [i32; 3]) -> Result<usize, String> {
        let mut result = Err("nothing happened".to_string());
        self.history
            .restructure(&mut self.model, "move object", |model| {
                result = model.move_object(i, delta);
            });
        match &result {
            Ok(n) => {
                self.after_structural();
                self.status = format!(
                    "moved {} by [{}, {}, {}] — {n} layer{}",
                    self.object_name(i),
                    delta[0],
                    delta[1],
                    delta[2],
                    if *n == 1 { "" } else { "s" }
                );
            }
            Err(why) => self.status = why.clone(),
        }
        result
    }

    /// Whether an object's row in the layer panel is folded shut.
    pub fn is_collapsed(&self, object: usize) -> bool {
        self.collapsed.contains(&object)
    }

    /// Fold an object's row open or shut. A view change: it does not dirty the
    /// document, and there is nothing to undo.
    pub fn toggle_collapsed(&mut self, object: usize) {
        if !self.collapsed.remove(&object) {
            self.collapsed.insert(object);
        }
    }

    /// Flip an object's visibility, for the panel's click target.
    pub fn toggle_object_visible(&mut self, object: usize) -> bool {
        let Some(o) = self.model.objects().get(object) else {
            return false;
        };
        let visible = !o.visible;
        self.set_object_visible(object, visible)
    }

    pub fn object_name(&self, i: usize) -> String {
        self.model
            .objects()
            .get(i)
            .map_or_else(|| format!("object {i}"), |o| o.name.clone())
    }

    /// Show or hide a layer by index. Ignores an index that is not there — a
    /// caller naming a layer that does not exist has already been told so.
    pub fn set_layer_visible(&mut self, i: usize, visible: bool) {
        if i >= self.model.layer_count() {
            return;
        }
        self.model.set_layer_visible(i, visible);
        self.dirty = true;
        self.invalidate_mesh();
        self.status = format!(
            "{} {}",
            self.layer_name(i),
            if visible { "shown" } else { "hidden" }
        );
    }

    /// Write `color` into a set of cells on the active layer, as one undo step,
    /// reporting each cell's before and after to `observe`.
    ///
    /// The entry point for an edit that did not come from a click: no ray, no
    /// span, no mirror — a caller that already knows which cells it means. One
    /// undo step because the user shares this history, and an agent that filled
    /// a box should cost them one `ctrl+Z` rather than five hundred.
    ///
    /// `cells` is a closure over the model so a caller can choose against the
    /// grid it is about to change — a paint wants the solid cells, a fill wants
    /// a flood — without this method knowing which.
    /// `cells` returns anything iterable, so a caller whose cells are pure
    /// arithmetic — a box — can hand over a lazy iterator instead of a list.
    /// Filling a 256³ scene materialised about 600 MB of coordinates and writes
    /// before touching the model; streaming it costs nothing but the undo step.
    /// A caller that has to *read* the model to choose its cells still collects,
    /// because the read cannot outlive the borrow the writes need.
    pub fn apply_batch<I: IntoIterator<Item = [i32; 3]>>(
        &mut self,
        label: &'static str,
        color: u8,
        cells: impl FnOnce(&VoxelModel, usize) -> I,
        observe: impl FnMut(u8, u8),
    ) {
        let layer = self.model.active_layer();
        let picked = cells(&self.model, layer);
        self.apply_writes(
            label,
            picked
                .into_iter()
                .map(|pos| CellWrite { layer, pos, color }),
            observe,
        );
    }

    /// Apply a sculpt tool at a point, as one undo step.
    ///
    /// The agent's door onto the rule the hand uses: the same brush ball, the
    /// same `sculpt_value`, and `apply_writes` like every other edit that did
    /// not come from a click. "Round this corner off" is a semantic edit; an
    /// agent naming several thousand cells is not, and it cannot see the screen
    /// to check what it got.
    ///
    /// The normal matters only to `Flatten`, which locks a plane through `at`.
    pub fn sculpt_at(
        &mut self,
        tool: Tool,
        at: [i32; 3],
        radius: u8,
        normal: [i32; 3],
        color: u8,
        layer: usize,
    ) -> Result<SculptReport, String> {
        if layer >= self.model.layer_count() {
            return Err(format!("there is no layer {layer}"));
        }
        if !self.model.contains(at[0], at[1], at[2]) {
            return Err(format!("{at:?} is outside the scene"));
        }
        let brush = region::Brush {
            radius,
            shape: self.brush.shape,
        };
        let reference = (tool == Tool::Flatten).then_some((at, normal));
        let cells = region::cells(
            &self.model,
            Span::Voxel,
            region::Reach {
                seed: at,
                // A voxel span consults neither of these: it is a shape, and
                // hands back every cell under the brush whatever it holds.
                // That matters here rather than being a detail — a candidate
                // set filtered to material would put air out of reach, and a
                // smooth could never fill a notch. Pinned by
                // `a_voxel_span_is_a_shape_and_ignores_the_match_rule`.
                face: Face::PosY,
                matches: region::Match::Solid,
                brush,
                grounded: false,
                y_limit: self.slice.unwrap_or(u16::MAX),
                within: None,
                layer: Some(layer),
            },
        );
        // Every decision read before any is applied, the same rule the stroke
        // follows — a smooth written as it goes cascades into itself.
        let writes: Vec<CellWrite> = cells
            .into_iter()
            .filter_map(|pos| {
                sculpt_value(&self.model, tool, color, pos, layer, reference)
                    .map(|color| CellWrite { layer, pos, color })
            })
            .collect();
        let mut report = SculptReport::default();
        self.apply_writes(tool.name(), writes, |before, after| {
            report.targeted += 1;
            match (before, after) {
                (0, 0) => report.unchanged += 1,
                (0, _) => report.added += 1,
                (_, 0) => report.removed += 1,
                _ => report.repainted += 1,
            }
        });
        Ok(report)
    }

    /// Apply ordered writes as one undo step without changing the selection.
    pub fn apply_writes(
        &mut self,
        label: &'static str,
        writes: impl IntoIterator<Item = CellWrite>,
        mut observe: impl FnMut(u8, u8),
    ) {
        let changed = self.history.edit(&mut self.model, label, |model, stroke| {
            for CellWrite {
                layer,
                pos: [x, y, z],
                color,
            } in writes
            {
                if layer >= model.layer_count() || !model.contains(x, y, z) {
                    continue;
                }
                // Read before the write, and report even when nothing moved:
                // "you asked for 125 cells and 125 were already that colour" is
                // an answer, and silence is not.
                let before = model.get_in(layer, x, y, z);
                stroke.set_in(model, layer, x, y, z, color);
                observe(before, color);
            }
        });
        if changed {
            self.dirty = true;
            self.refresh_instances();
        }
        self.invalidate_mesh();
    }

    // -- colour as a way of naming parts -----------------------------------

    /// Select every voxel of one colour on a layer.
    ///
    /// Colour is how a voxel model is organised: "all the red" is a part in a
    /// way that "all the cells in this box" is not. One layer, like every other
    /// selection, because the transforms write to one.
    pub fn select_by_color(&mut self, index: u8, layer: usize) -> usize {
        let cells: Vec<[i32; 3]> = self
            .model
            .iter_filled_in(layer)
            .filter(|(_, v)| *v == index)
            .map(|([x, y, z], _)| [x as i32, y as i32, z as i32])
            .collect();
        self.set_selection(Selection::new(layer, cells))
    }

    /// How many voxels each index holds, across every layer or within one.
    ///
    /// A walk, not a tally: this is a question an agent asks a few times a
    /// session, where the per-index counters that would answer it in O(1) would
    /// have to be maintained on every write for the rest of time.
    pub fn color_counts(&self, layer: Option<usize>) -> Vec<(u8, usize)> {
        let mut counts = [0usize; 256];
        match layer {
            Some(n) => {
                for (_, v) in self.model.iter_filled_in(n) {
                    counts[v as usize] += 1;
                }
            }
            None => {
                for n in 0..self.model.layer_count() {
                    for (_, v) in self.model.iter_filled_in(n) {
                        counts[v as usize] += 1;
                    }
                }
            }
        }
        counts
            .into_iter()
            .enumerate()
            .skip(1) // index 0 is air, not a colour
            .filter(|(_, n)| *n > 0)
            .map(|(i, n)| (i as u8, n))
            .collect()
    }

    /// Every cell holding one of `from`, rewritten to `to`, across every layer.
    ///
    /// The palette is untouched: this moves voxels between slots rather than
    /// changing what a slot means. `set_palette_color` is the other one.
    ///
    /// Returns how many voxels moved from each index.
    pub fn recolor(&mut self, from: &[u8], to: u8) -> Vec<(u8, usize)> {
        let mut moved = [0usize; 256];
        let writes: Vec<CellWrite> = (0..self.model.layer_count())
            .flat_map(|layer| {
                self.model
                    .iter_filled_in(layer)
                    .filter(|(_, v)| from.contains(v) && *v != to)
                    .map(move |([x, y, z], v)| (layer, [x as i32, y as i32, z as i32], v))
                    .collect::<Vec<_>>()
            })
            .map(|(layer, pos, v)| {
                moved[v as usize] += 1;
                CellWrite {
                    layer,
                    pos,
                    color: to,
                }
            })
            .collect();
        self.apply_writes("recolour", writes, |_, _| {});
        moved
            .into_iter()
            .enumerate()
            .filter(|(_, n)| *n > 0)
            .map(|(i, n)| (i as u8, n))
            .collect()
    }

    /// Exchange two indices, voxel for voxel.
    ///
    /// The **voxels** move, not the palette entries. Both readings put the same
    /// picture on screen — a swap of two slots' colours looks identical to a
    /// swap of which slot each voxel names — but only this one leaves the
    /// palette meaning what it meant. After it, index 3 is still the red it was,
    /// so a brush set to 3 still paints red. Swapping the entries instead would
    /// silently change what every future edit with that index does.
    ///
    /// Returns how many voxels moved each way.
    pub fn swap_colors(&mut self, a: u8, b: u8) -> (usize, usize) {
        let mut counts = (0usize, 0usize);
        let writes: Vec<CellWrite> = (0..self.model.layer_count())
            .flat_map(|layer| {
                self.model
                    .iter_filled_in(layer)
                    .filter(|(_, v)| *v == a || *v == b)
                    .map(move |([x, y, z], v)| (layer, [x as i32, y as i32, z as i32], v))
                    .collect::<Vec<_>>()
            })
            .map(|(layer, pos, v)| {
                let color = if v == a {
                    counts.0 += 1;
                    b
                } else {
                    counts.1 += 1;
                    a
                };
                CellWrite { layer, pos, color }
            })
            .collect();
        self.apply_writes("swap colours", writes, |_, _| {});
        counts
    }

    /// Drop unused palette entries, renumber what is left from 1 upwards, and
    /// rewrite every voxel to follow. Returns the mapping, old index to new.
    ///
    /// One undo step for both halves, through the snapshot path — which is why
    /// `Snapshot` carries the palette. A compact recorded as cell edits alone
    /// would undo the voxels and leave them pointing at colours that had moved.
    ///
    /// Index 0 stays index 0: it is air, not a colour.
    pub fn compact_palette(&mut self) -> Vec<(u8, u8)> {
        let used: Vec<u8> = self
            .color_counts(None)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        if used.len() > 255 {
            return Vec::new();
        }
        let mapping: Vec<(u8, u8)> = used
            .iter()
            .enumerate()
            .map(|(n, old)| (*old, n as u8 + 1))
            .collect();
        if mapping.iter().all(|(old, new)| old == new) {
            self.status = "the palette is already compact".into();
            return Vec::new();
        }

        let mut lookup = [0u8; 256];
        for (old, new) in &mapping {
            lookup[*old as usize] = *new;
        }
        let colors: Vec<voxel_core::Rgb8> = mapping
            .iter()
            .map(|(old, _)| self.model.palette().get(*old))
            .collect();

        self.history
            .restructure(&mut self.model, "compact palette", |model| {
                for layer in 0..model.layer_count() {
                    let cells: Vec<_> = model
                        .iter_filled_in(layer)
                        .map(|([x, y, z], v)| ([x as i32, y as i32, z as i32], lookup[v as usize]))
                        .collect();
                    for ([x, y, z], v) in cells {
                        model.set_in(layer, x, y, z, v);
                    }
                }
                let mut next = [voxel_core::Rgb8::default(); 256];
                for (n, c) in colors.iter().enumerate() {
                    next[n + 1] = *c;
                }
                model.set_palette(voxel_core::Palette::from_colors(next));
            });
        // The selected colour is an index, and every index has just moved.
        self.color = lookup[self.color as usize].max(1);
        self.after_structural();
        self.status = format!("compacted the palette to {} colours", mapping.len());
        mapping
    }

    pub fn set_palette_color(&mut self, index: u8, color: voxel_core::Rgb8) -> bool {
        let changed = self
            .history
            .set_palette_color(&mut self.model, index, color);
        if changed {
            self.dirty = true;
            self.invalidate_mesh();
        }
        changed
    }

    /// Show or hide the active layer.
    ///
    /// Not an undo step, though it does dirty the document: visibility is a
    /// thing you toggle constantly while working, and putting it on the undo
    /// stack would mean `ctrl+Z` spent its first few presses turning layers
    /// back on instead of undoing the edit you wanted back. It is still saved,
    /// because which layers you had hidden is part of the model.
    pub fn toggle_layer_visible(&mut self) {
        let i = self.model.active_layer();
        let visible = !self.model.layers()[i].visible;
        self.set_layer_visible(i, visible);
    }

    fn after_structural(&mut self) {
        self.dirty = true;
        self.refresh_instances();
        self.invalidate_mesh();
    }

    /// Bring every instance back in step with the source it repeats.
    ///
    /// Called after anything that can change a source's cells — a stroke, a
    /// batch, an undo, a structural change — rather than inside the write
    /// itself. A rebuild walks the source, so charging it per voxel would make
    /// a fill pay for it a million times; charging it per *commit* is the same
    /// trade `Editor::mesh` already makes, and a stroke is the unit a user
    /// thinks in.
    fn refresh_instances(&mut self) {
        if self.model.rebuild_instances() {
            self.invalidate_mesh();
        }
    }

    /// Repeat an object somewhere else, as a reference rather than a copy.
    pub fn add_instance(
        &mut self,
        source: usize,
        parent: usize,
        name: &str,
        offset: [i32; 3],
        mirror: [bool; 3],
    ) -> Result<usize, String> {
        // Checked before the snapshot, so a refusal does not spend an undo.
        self.model
            .clone()
            .add_instance(source, parent, name, offset, mirror)?;
        let name = name.to_string();
        let mut made = 0;
        self.history
            .restructure(&mut self.model, "add instance", |model| {
                made = model
                    .add_instance(source, parent, name, offset, mirror)
                    .unwrap_or(0);
            });
        self.after_structural();
        self.status = format!(
            "{} repeats {}",
            self.object_name(made),
            self.object_name(source)
        );
        Ok(made)
    }

    /// Move or mirror an instance, all or nothing.
    pub fn place_instance(
        &mut self,
        object: usize,
        offset: [i32; 3],
        mirror: [bool; 3],
    ) -> Result<(), String> {
        self.model.clone().place_instance(object, offset, mirror)?;
        self.history
            .restructure(&mut self.model, "place instance", |model| {
                let _ = model.place_instance(object, offset, mirror);
            });
        self.after_structural();
        self.status = format!("moved {}", self.object_name(object));
        Ok(())
    }

    /// Turn an instance into ordinary work, keeping exactly what is on screen.
    pub fn detach_instance(&mut self, object: usize) -> bool {
        if self.model.instance(object).is_none() {
            self.status = format!("{} is not an instance", self.object_name(object));
            return false;
        }
        let name = self.object_name(object);
        self.history
            .restructure(&mut self.model, "detach instance", |model| {
                model.detach_instance(object);
            });
        self.after_structural();
        self.status = format!("{name} is its own work now");
        true
    }

    /// Why a write to `layer` was refused, if it was.
    ///
    /// A tool acting only on the active layer is a rule; a tool that ignores
    /// you with no explanation is a bug report. This is the instance half of
    /// that: the message names the source to edit instead, and the way out.
    pub fn generated_refusal(&self, layer: usize) -> Option<String> {
        let l = self.model.layers().get(layer)?;
        if !l.is_generated() {
            return None;
        }
        let at = self.model.instance(l.object)?;
        Some(format!(
            "\"{}\" repeats \"{}\" — edit \"{}\" to change every copy, or detach it to make \
             this one its own work",
            self.object_name(l.object),
            self.object_name(at.source),
            self.object_name(at.source),
        ))
    }

    // -- renaming --------------------------------------------------------

    /// The name being typed, if a rename is open. While this is `Some` the
    /// window layer must route keystrokes here and nowhere else.
    pub fn renaming(&self) -> Option<&str> {
        self.rename.as_deref()
    }

    pub fn begin_rename(&mut self) {
        self.rename = Some(self.layer_name(self.model.active_layer()));
    }

    /// Take one typed character. Control characters are dropped here rather
    /// than at the window layer, so every platform's idea of what arrives with
    /// a key press meets the same filter.
    pub fn rename_push(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        if let Some(buf) = &mut self.rename {
            if buf.chars().count() < 32 {
                buf.push(c);
            }
        }
    }

    pub fn rename_backspace(&mut self) {
        if let Some(buf) = &mut self.rename {
            buf.pop();
        }
    }

    pub fn commit_rename(&mut self) {
        let Some(name) = self.rename.take() else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            self.status = "a layer needs a name".into();
            return;
        }
        let i = self.model.active_layer();
        self.model.rename_layer(i, name);
        self.dirty = true;
        self.report_layer();
    }

    pub fn cancel_rename(&mut self) {
        self.rename = None;
    }

    // -- view ------------------------------------------------------------

    /// Frame the whole volume, whatever is in it.
    ///
    /// This is what opening a file does, rather than framing the contents: a
    /// model that is one voxel — a new one, seeded so there is something to
    /// build against — would otherwise fill the window with a single cube and
    /// give no sense of the space around it. `F` frames the contents on demand.
    pub fn frame_volume(&mut self) {
        let offset = self.offset();
        let [x, y, z] = self.model.size();
        self.camera.frame(
            offset,
            Vec3 {
                x: offset.x + x as f32,
                y: offset.y + y as f32,
                z: offset.z + z as f32,
            },
        );
    }

    /// Point the camera at the model's contents, or at the whole volume when
    /// it is empty — an empty model has no bounds to frame.
    pub fn frame_model(&mut self) {
        let offset = self.offset();
        let (min, max) = match self.model.occupied_bounds() {
            Some((lo, hi)) => (
                Vec3 {
                    x: lo[0] as f32,
                    y: lo[1] as f32,
                    z: lo[2] as f32,
                },
                Vec3 {
                    x: hi[0] as f32 + 1.0,
                    y: hi[1] as f32 + 1.0,
                    z: hi[2] as f32 + 1.0,
                },
            ),
            None => {
                let [x, y, z] = self.model.size();
                (
                    Vec3::ZERO,
                    Vec3 {
                        x: x as f32,
                        y: y as f32,
                        z: z as f32,
                    },
                )
            }
        };
        self.camera.frame(min + offset, max + offset);
    }

    pub fn reset_view(&mut self) {
        let fov = self.camera.fov_y;
        self.camera = OrbitCamera {
            fov_y: fov,
            ..OrbitCamera::default()
        };
        self.frame_volume();
    }

    /// Move the slice plane, clamped to the volume. `None` turns slicing off.
    pub fn set_slice(&mut self, slice: Option<u16>) {
        let sy = self.model.size()[1];
        self.slice = slice.map(|s| s.clamp(1, sy));
        // At full height a slice hides nothing; drop it so the HUD does not
        // claim a slice is active when the model is whole.
        if self.slice == Some(sy) {
            self.slice = None;
        }
        // The cut is not a property of any one chunk: moving it changes which
        // faces exist throughout, so nothing that was built for the old one
        // still stands.
        self.model.dirty_all();
        self.invalidate_mesh();
        self.status = match self.slice {
            Some(s) => format!("slice below y={s}"),
            None => "slice off".into(),
        };
    }

    pub fn nudge_slice(&mut self, delta: i32) {
        let sy = self.model.size()[1];
        let current = self.slice.unwrap_or(sy) as i32;
        self.set_slice(Some((current + delta).clamp(1, sy as i32) as u16));
    }

    /// Which mirror planes are on, as `"X"`, `"XZ"` and so on, or `None`.
    pub fn mirror_axes(&self) -> Option<String> {
        let s: String = ["X", "Y", "Z"]
            .iter()
            .enumerate()
            .filter(|(i, _)| self.mirror[*i])
            .map(|(_, a)| *a)
            .collect();
        (!s.is_empty()).then_some(s)
    }

    /// Toggle mirroring across one axis: 0 = x, 1 = y, 2 = z.
    pub fn toggle_mirror(&mut self, axis: usize) {
        self.mirror[axis] = !self.mirror[axis];
        self.status = match self.mirror_axes() {
            Some(axes) => format!("mirror {axes}"),
            None => "mirror off".into(),
        };
    }

    pub fn set_span(&mut self, span: Span) {
        self.span = span;
        self.status = match span {
            Span::Voxel if self.brush.radius > 0 => {
                let e = self.brush.edge();
                format!(
                    "{} brush {e}x{e}x{e}",
                    self.brush.shape.name().to_lowercase()
                )
            }
            _ => format!("span {}", span.name().to_lowercase()),
        };
    }

    /// Grow or shrink the brush, clamped to a single cell at one end and
    /// [`MAX_BRUSH`] at the other.
    ///
    /// Resizing also selects [`Span::Voxel`]: the brush is that span's shape and
    /// has no meaning under the others, so a size key that left a plane fill
    /// selected would appear to do nothing at all.
    /// Resize the brush.
    ///
    /// It used to snap the span back to `Voxel`, because the brush was a shape
    /// only a voxel span had a use for. It is a *reach* now — the radius bounds
    /// whatever span is running — so changing it while a plane span is selected
    /// is a deliberate thing to do rather than a mistake to correct.
    pub fn nudge_brush(&mut self, delta: i32) {
        self.brush.radius = (self.brush.radius as i32 + delta).clamp(0, MAX_BRUSH as i32) as u8;
        let e = self.brush.edge();
        self.status = match (self.span, self.brush.radius) {
            (Span::Voxel, _) => format!(
                "{} brush {e}x{e}x{e}",
                self.brush.shape.name().to_lowercase()
            ),
            (span, 0) => format!(
                "{} reaches as far as it connects",
                span.name().to_lowercase()
            ),
            (span, _) => format!(
                "{} within {} of where you click — drag to work it",
                span.name().to_lowercase(),
                self.brush.radius
            ),
        };
    }

    pub fn toggle_brush_shape(&mut self) {
        self.brush.shape = match self.brush.shape {
            BrushShape::Cube => BrushShape::Sphere,
            BrushShape::Sphere => BrushShape::Cube,
        };
        self.span = Span::Voxel;
        let e = self.brush.edge();
        self.status = format!(
            "{} brush {e}x{e}x{e}",
            self.brush.shape.name().to_lowercase()
        );
    }

    /// Step the palette index, wrapping within the paintable range 1..=255.
    pub fn nudge_color(&mut self, delta: i32) {
        let next = (self.color as i32 - 1 + delta).rem_euclid(255) + 1;
        self.color = next as u8;
        self.status = format!("colour {}", self.color);
    }

    // -- files -----------------------------------------------------------

    pub fn save(&mut self) {
        let path = self.path.clone();
        self.save_as(&path);
    }

    pub fn save_as(&mut self, path: &Path) {
        match format::save(path, &self.model) {
            Ok(()) => {
                // Only a save to the *working* path clears the dirty flag; an
                // export elsewhere leaves the document unsaved, because it is.
                if path == self.path {
                    self.dirty = false;
                }
                self.status = format!("saved {}", path.display());
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    /// Export beside the working file, as `.vox`.
    ///
    /// `.vox` has nowhere to put a layer stack, so an export writes what is on
    /// screen as one model. Saying so is the point: losing layers quietly is
    /// how someone ends up treating the export as their save.
    pub fn export_vox(&mut self) {
        let path = self.path.with_extension("vox");
        self.save_as(&path);
        let layers = self.model.layer_count();
        if layers > 1 && self.status.starts_with("saved") {
            self.status = format!("{} — {layers} layers flattened into one", self.status);
        }
    }

    /// Replace the document wholesale: a different model, under a different
    /// path.
    ///
    /// The history goes with it, for the reason [`Editor::reload`]'s does — the
    /// recorded coordinates describe a grid that no longer exists. The camera
    /// is re-framed on the new volume, because a view fitted to the old one is
    /// as likely as not to be pointing at empty space.
    pub fn open(&mut self, model: VoxelModel, path: PathBuf) {
        self.document_id = next_document_id();
        self.selection = None;
        self.selected_object = None;
        self.clipboard = None;
        self.collapsed.clear();
        self.model = model;
        self.path = path;
        self.history.reset();
        self.slice = None;
        self.dirty = false;
        self.invalidate_mesh();
        self.frame_volume();
        self.status = format!("opened {}", self.path.display());
    }

    /// Show a model that arrived from somewhere else, keeping the view.
    ///
    /// Unlike [`Editor::open`] the camera is left exactly where it was: this is
    /// called every time an attached session changes, and re-framing on each
    /// update would wrench the view out of the watcher's hands several times a
    /// second. The history goes, because it described a grid that has been
    /// replaced — and a viewer has nothing to undo in any case.
    pub fn show(&mut self, model: VoxelModel) {
        self.document_id = next_document_id();
        self.selection = None;
        self.selected_object = None;
        self.clipboard = None;
        self.collapsed.clear();
        // A slice past the new model's height would hide all of it.
        let sy = model.size()[1];
        if self.slice.is_some_and(|s| s >= sy) {
            self.slice = None;
        }
        self.model = model;
        self.history.reset();
        self.invalidate_mesh();
    }

    /// Record that the model now matches what is on disk at `path`.
    ///
    /// Only a save to the *working* path clears the dirty flag; writing a copy
    /// elsewhere leaves the document unsaved, because it is — the same rule
    /// [`Editor::save_as`] follows, exposed for a caller that did its own
    /// writing.
    pub fn mark_saved(&mut self, path: &Path) {
        if path == self.path {
            self.dirty = false;
        }
        self.status = format!("saved {}", path.display());
    }

    /// Reload from disk, discarding unsaved work and the history with it — the
    /// recorded coordinates describe a grid that is being replaced.
    pub fn reload(&mut self) {
        match format::load(&self.path) {
            Ok(model) => {
                self.document_id = next_document_id();
                self.selection = None;
                self.clipboard = None;
                self.model = model;
                self.history.reset();
                self.dirty = false;
                self.slice = None;
                self.invalidate_mesh();
                self.frame_volume();
                self.status = format!("reloaded {}", self.path.display());
            }
            Err(e) => self.status = format!("reload failed: {e}"),
        }
    }

    /// The view-projection matrix for an image of this size.
    pub fn view_projection(&self, width: u32, height: u32) -> Mat4 {
        self.camera
            .view_projection(width.max(1) as f32 / height.max(1) as f32)
    }

    /// A one-line summary for the HUD.
    pub fn summary(&self) -> String {
        let [x, y, z] = self.model.size();
        let mut s = format!(
            "{}  {x}x{y}x{z}  {} VOX  {}",
            if self.dirty { "*" } else { " " },
            self.model.filled_count(),
            if self.viewing {
                "VIEWING"
            } else {
                self.tool.name()
            },
        );
        if self.viewing {
            // Nothing after this is a choice the watcher can make, so the line
            // stops here rather than advertising tools that do nothing.
            if let Some(cut) = self.slice {
                s.push_str(&format!("  SLICE {cut}"));
            }
            return s;
        }
        if self.span != Span::Voxel {
            s.push_str(&format!("/{}", self.span.name()));
        } else if self.brush.radius > 0 {
            let e = self.brush.edge();
            s.push_str(&format!("  {} {e}x{e}x{e}", self.brush.shape.name()));
        }
        if let Some(axes) = self.mirror_axes() {
            s.push_str(&format!("  MIRROR {axes}"));
        }
        if self.model.layer_count() > 1 {
            s.push_str(&format!(
                "  L{}/{}",
                self.model.active_layer() + 1,
                self.model.layer_count()
            ));
        }
        if let Some(cut) = self.slice {
            s.push_str(&format!("  SLICE {cut}"));
        }
        // Whether undo has anywhere to go is worth a glance before a big edit.
        s.push_str(&format!(
            "  UNDO {}/{}",
            self.undo_depth(),
            self.redo_depth()
        ));
        s
    }
}

fn next_document_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Load `path`, or start a new `size`³ model if it does not exist yet.
///
/// A missing file is not an error: `voxeler robot.vxm` on a fresh directory is
/// how you start a model, and refusing would mean the tool could only ever open
/// something another tool had made.
pub fn open_or_create(path: &Path, size: u16) -> Result<VoxelModel, String> {
    if !path.exists() {
        return Ok(new_model(size));
    }
    format::load(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// An empty volume with one voxel at its centre.
///
/// The seed is there so a new model has something to click. Building places
/// against a face, so on a truly empty grid there is no face to place against —
/// the ground-plane fallback in [`Editor::target_at`] covers that too, but a
/// visible starting cube is what makes the first click obvious rather than
/// something you have to know about.
///
/// At the centre of the volume, which is also the world origin and the height
/// of the work plane: the seed sits *on* the grid rather than floating over it
/// or buried under it.
pub fn new_model(size: u16) -> VoxelModel {
    let mut model = VoxelModel::new(size, size, size);
    let mid = (size / 2) as i32;
    model.set(mid, mid, mid, 1);
    model
}

/// The face a target highlight should outline, as four world-space corners.
pub fn face_corners(voxel: [i32; 3], face: Face, offset: Vec3) -> [Vec3; 4] {
    let c = voxel_render::face_corners(voxel, face);
    [c[0] + offset, c[1] + offset, c[2] + offset, c[3] + offset]
}

/// What a tool writes at one cell, or `None` to leave it alone.
///
/// A free function rather than a method, because the hand and the agent both
/// reach it and neither should get its own copy of the rule: a smooth that
/// rounded a corner at the window and not over MCP would be two tools wearing
/// one name.
///
/// Asked of the *active layer*, not of what is on screen: that is the grid
/// being written, and testing the composite would refuse to build under a voxel
/// a higher layer is showing — which is exactly the thing a lower layer is for.
fn sculpt_value(
    model: &VoxelModel,
    tool: Tool,
    color: u8,
    cell: [i32; 3],
    layer: usize,
    reference: Option<([i32; 3], [i32; 3])>,
) -> Option<u8> {
    let [x, y, z] = cell;
    let here = model.get_in(layer, x, y, z);
    match tool {
        // Build fills air and never repaints; erase and paint act on material
        // and never create it. With a single cell that holds by construction,
        // but a brush covers cells the ray never touched and a reflection lands
        // wherever the model happens to be, so it has to be stated.
        Tool::Build => (here == 0).then_some(color),
        Tool::Erase => (here != 0).then_some(0),
        Tool::Paint => (here != 0).then_some(color),
        Tool::Flatten => {
            let (origin, normal) = reference?;
            // Signed distance along the locked normal. Positive is in front of
            // the plane — outside the surface — and that is what a flatten
            // takes off; behind it is what it fills in.
            let d: i32 = (0..3).map(|a| (cell[a] - origin[a]) * normal[a]).sum();
            match (d > 0, here != 0) {
                (true, true) => Some(0),
                (false, false) => Some(color),
                // Already on the right side of the plane.
                _ => None,
            }
        }
        Tool::Smooth => {
            // Face neighbours only. Counting the twenty-six would let a
            // diagonal contact hold a spur on, which is the thing a smooth is
            // being asked to remove.
            let solid = (0..3)
                .flat_map(|a| [-1, 1].map(move |d| (a, d)))
                .filter(|(a, d)| {
                    let mut q = cell;
                    q[*a] += d;
                    model.get_in(layer, q[0], q[1], q[2]) != 0
                })
                .count();
            match here != 0 {
                // A spur: solid, and hanging off almost nothing.
                true if solid <= 2 => Some(0),
                // A notch: air, and all but walled in.
                false if solid >= 4 => Some(color),
                _ => None,
            }
        }
        Tool::Pick | Tool::Select => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with_floor() -> Editor {
        let mut model = VoxelModel::new(8, 8, 8);
        for x in 0..8 {
            for z in 0..8 {
                model.set(x, 0, z, 4);
            }
        }
        let mut e = Editor::new(model, PathBuf::from("test.vxm"));
        // Look straight down at the floor so pixel coordinates map predictably
        // onto the X/Z plane.
        e.camera.yaw = 0.0;
        e.camera.pitch = 1.4;
        e
    }

    /// Two parts on two layers inside one object. The case a cell selection
    /// cannot represent, which is why object mode exists.
    fn two_layer_part() -> Editor {
        let mut m = VoxelModel::new(16, 16, 16);
        let arm = m.add_object(0, "ARM").unwrap();
        let upper = m.add_layer(0, "UPPER").unwrap();
        m.set_layer_object(upper, arm);
        let lower = m.add_layer(upper, "LOWER").unwrap();
        m.set_layer_object(lower, arm);
        m.set_in(upper, 4, 8, 4, 7);
        m.set_in(lower, 4, 4, 4, 9);
        Editor::new(m, PathBuf::from("t.vxm"))
    }

    /// The whole reason object mode is not a cell selection: it has to carry
    /// every layer of the part, and a `Selection` is one layer by design.
    #[test]
    fn an_object_selection_spans_the_parts_layers() {
        let mut e = two_layer_part();
        assert_eq!(e.select_object_at([4, 8, 4]), Some(1), "picked by a voxel");
        let (lo, hi) = e.selected_object_bounds().expect("outlined");
        assert_eq!(lo, [4, 4, 4], "the lower layer is in the box");
        assert_eq!(hi, [4, 8, 4], "and so is the upper");

        // Moving it moves both layers, which is what a cell selection could
        // not have done — it would have taken the active layer's share.
        e.move_object(1, [1, 0, 0]).unwrap();
        assert_eq!(e.model().get_in(1, 5, 8, 4), 7);
        assert_eq!(e.model().get_in(2, 5, 4, 4), 9);
        assert_eq!(e.model().get_in(1, 4, 8, 4), 0);

        e.select_object(None);
        assert!(e.selected_object_bounds().is_none());
    }

    /// The select tool never writes. It is the one tool where a click is a
    /// choice, so it must not reach a stroke or spend an undo.
    #[test]
    fn selecting_never_edits_and_never_costs_an_undo() {
        let mut e = editor_with_floor();
        e.tool = Tool::Select;
        let before = e.model().filled_count();
        let depth = e.undo_depth();
        let target = e.target_at(160.0, 120.0, 320, 240).expect("a hit");

        // Even driven through the stroke path, which is what `app.rs` avoids.
        e.begin_stroke(target);
        e.end_stroke();
        assert_eq!(e.model().filled_count(), before, "nothing was written");
        assert_eq!(e.undo_depth(), depth, "and nothing is on the undo stack");
    }

    /// A click on nothing has to be a way to say "never mind". The select tool
    /// gets no ground-plane fallback — that exists so build has something to
    /// aim at on an empty layer, and here it would mean a click on the sky
    /// selected a cell of air instead of clearing.
    #[test]
    fn a_select_click_on_empty_space_has_no_target() {
        let mut e = editor_with_floor();
        e.tool = Tool::Select;
        // Straight down the middle finds the floor.
        assert!(e.target_at(160.0, 120.0, 320, 240).is_some());
        // The far corner, well off it, finds nothing — even though `plane_is_open`
        // would hand Build a target at the same pixel.
        e.tool = Tool::Build;
        e.model.clear();
        assert!(e.plane_is_open(), "the fallback is available to build");
        e.tool = Tool::Select;
        assert_eq!(
            e.target_at(4.0, 236.0, 320, 240),
            None,
            "select never falls back to the work plane"
        );
    }

    /// "All of it" is the commonest selection there is, and the one a flood
    /// fill needs a seed to reach.
    #[test]
    fn selecting_a_whole_layer_takes_that_layer_only() {
        let mut e = two_layer_part();
        assert_eq!(e.select_all_in_layer(1), 1);
        assert_eq!(e.selection.as_ref().unwrap().layer(), 1);
        assert_eq!(e.select_all_in_layer(2), 1);
        assert_eq!(e.selection.as_ref().unwrap().layer(), 2);
        // An empty layer selects nothing rather than pretending otherwise.
        e.add_layer();
        let empty = e.active_layer();
        assert_eq!(e.select_all_in_layer(empty), 0);
        assert!(e.selection.is_none());
    }

    /// Copy then paste is in place, so the arrows are the offset — which is
    /// what `duplicate_selection` takes as an argument over MCP.
    #[test]
    fn paste_puts_it_back_where_it_was_copied_from() {
        let mut e = two_layer_part();
        e.select_all_in_layer(1);
        e.copy_selection().unwrap();
        let before: Vec<_> = e.model().iter_filled().collect();

        e.paste_in_place();
        assert_eq!(
            e.model().iter_filled().collect::<Vec<_>>(),
            before,
            "a paste in place changes nothing"
        );
        assert_eq!(e.clipboard.as_ref().unwrap().origin(), [4, 8, 4]);
    }

    /// A build click must place *outside* the voxel it hit, never inside it.
    #[test]
    fn build_targets_the_cell_against_the_face() {
        let e = editor_with_floor();
        let t = e.target_at(160.0, 120.0, 320, 240).expect("should hit");
        assert_eq!(t.face, Face::PosY);
        assert_eq!(t.cell, [t.voxel[0], 1, t.voxel[2]]);
    }

    /// A stroke must keep aiming at the model it started on.
    ///
    /// The bug, reported from use: one click added two voxels. Placing one puts
    /// a new face under the pointer, and the next mouse event — a click emits
    /// one of its own — built against *that*, so a click laid down a voxel per
    /// event. The plane pin did not catch it because the new face was a side
    /// face, which puts the next cell on the same plane.
    #[test]
    fn a_click_places_one_voxel_however_many_events_it_takes() {
        for (label, pitch) in [("3/4 view", 0.6f32), ("low", 0.35), ("high", 1.0)] {
            let mut model = VoxelModel::new(16, 16, 16);
            for z in 0..16 {
                for x in 0..16 {
                    model.set(x, 0, z, 4);
                }
            }
            let mut e = Editor::new(model, PathBuf::from("t.vxm"));
            e.camera.yaw = 0.7;
            e.camera.pitch = pitch;
            let before = e.model().filled_count();

            let Some(t) = e.target_at(160.0, 120.0, 320, 240) else {
                continue;
            };
            e.begin_stroke(t);
            // The same pixel, several times: no movement at all, which is the
            // most a click can honestly claim.
            for _ in 0..4 {
                if let Some(t) = e.target_at(160.0, 120.0, 320, 240) {
                    e.continue_stroke(t);
                }
            }
            e.end_stroke();
            assert_eq!(
                e.model().filled_count(),
                before + 1,
                "{label}: one click placed {} voxels",
                e.model().filled_count() - before
            );
        }
    }

    /// The mirror of it: an erase opens a hole, and the next event must not
    /// reach the wall behind through it.
    #[test]
    fn a_click_erases_one_voxel_and_does_not_dig() {
        let mut model = VoxelModel::new(16, 16, 16);
        for z in 0..16 {
            for y in 0..16 {
                for x in 0..16 {
                    model.set(x, y, z, 4);
                }
            }
        }
        let mut e = Editor::new(model, PathBuf::from("t.vxm"));
        e.camera.yaw = 0.7;
        e.camera.pitch = 0.6;
        e.tool = Tool::Erase;
        let before = e.model().filled_count();

        let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(t);
        for _ in 0..4 {
            if let Some(t) = e.target_at(160.0, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();
        assert_eq!(
            e.model().filled_count(),
            before - 1,
            "one click erased {} voxels",
            before - e.model().filled_count()
        );
    }

    /// And the same on bare work plane, where a new layer starts.
    #[test]
    fn a_click_on_the_work_plane_places_one_voxel() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.7;
        e.camera.pitch = 0.6;
        let t = e.target_at(160.0, 120.0, 320, 240).expect("the plane");
        e.begin_stroke(t);
        for _ in 0..4 {
            if let Some(t) = e.target_at(160.0, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 1, "one click on the plane");
    }

    /// A drag that genuinely travels still draws. The fix must not turn every
    /// stroke into a single voxel.
    #[test]
    fn a_drag_that_moves_still_draws_a_run() {
        let mut e = editor_with_floor();
        let start = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(start);
        for px in (60..260).step_by(4) {
            if let Some(t) = e.target_at(px as f32, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();
        assert!(
            e.model().filled_count() > 64 + 3,
            "a real drag placed only {}",
            e.model().filled_count() - 64
        );
        assert_eq!(e.undo_depth(), 1);
    }

    #[test]
    fn erase_and_paint_target_the_voxel_that_was_hit() {
        let mut e = editor_with_floor();
        for tool in [Tool::Erase, Tool::Paint, Tool::Pick] {
            e.tool = tool;
            let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
            assert_eq!(t.cell, t.voxel, "{tool:?}");
        }
    }

    /// The bug this pair exists for: an empty model was impossible to work on,
    /// because building places against a face and there was no face.
    #[test]
    fn building_on_an_empty_model_falls_back_to_the_ground_plane() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 0.9;

        let t = e
            .target_at(160.0, 120.0, 320, 240)
            .expect("the work plane is a target");
        assert!(t.is_ground());
        assert_eq!(
            t.cell[1],
            e.ground_y(),
            "a ground build lands on the work plane"
        );
        assert!(e.model().contains(t.cell[0], t.cell[1], t.cell[2]));

        e.begin_stroke(t);
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 1);
        assert_eq!(e.model().get(t.cell[0], t.cell[1], t.cell[2]), e.color);
    }

    /// The plane runs through the middle of the volume, so it has two sides.
    /// Seen from below, a build must hang under it rather than land on top —
    /// the same rule as placing against the face of a voxel you can see.
    #[test]
    fn building_from_under_the_work_plane_places_on_the_underside() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = -0.9; // orbited below the plane, looking up at it

        let t = e
            .target_at(160.0, 120.0, 320, 240)
            .expect("the plane has an underside");
        assert_eq!(t.face, Face::NegY);
        assert_eq!(t.cell[1], e.ground_y() - 1);
    }

    /// The bug this fixes: with the plane always open, the whole viewport was a
    /// build surface, and a click meant for the camera placed a voxel.
    #[test]
    fn the_work_plane_closes_once_the_active_layer_has_something_to_build_on() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 0.9;
        assert!(
            e.target_at(160.0, 120.0, 320, 240)
                .is_some_and(|t| t.is_ground()),
            "an empty layer has nothing to aim at, so the plane is how you start"
        );

        // One voxel in a far corner — nowhere near this ray, but enough to make
        // the layer something you can build against.
        e.model.set(0, 0, 0, 1);
        assert!(
            e.target_at(160.0, 120.0, 320, 240).is_none(),
            "empty space stops being clickable once there is something to aim at"
        );
    }

    /// And a new layer opens it again, which is how a part of a scene gets its
    /// first voxel somewhere away from everything else.
    #[test]
    fn a_new_layer_opens_the_work_plane_again() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 0.9;
        e.model.set(0, 0, 0, 1);
        assert!(e.target_at(160.0, 120.0, 320, 240).is_none());

        e.add_layer();
        let t = e
            .target_at(160.0, 120.0, 320, 240)
            .expect("a layer with nothing in it");
        assert!(t.is_ground());
        e.begin_stroke(t);
        e.end_stroke();
        assert_eq!(
            e.model().get_in(1, t.cell[0], t.cell[1], t.cell[2]),
            e.color
        );
    }

    /// A stroke that started on the plane has to finish there: its own first
    /// placement fills the layer, which would otherwise close the plane out
    /// from under the rest of the drag.
    #[test]
    fn a_drag_that_began_on_the_work_plane_keeps_it_open() {
        let mut e = Editor::new(VoxelModel::new(16, 8, 16), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 1.2;

        let start = e.target_at(160.0, 120.0, 320, 240).expect("the plane");
        assert!(start.is_ground());
        e.begin_stroke(start);
        for px in (100..220).step_by(4) {
            if let Some(t) = e.target_at(px as f32, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();

        assert!(
            e.model().filled_count() > 3,
            "the drag stopped after its first voxel: {} placed",
            e.model().filled_count()
        );
        assert_eq!(e.undo_depth(), 1, "and it is still one stroke");
        // Once the button is up the plane is closed again.
        assert!(e.target_at(20.0, 200.0, 320, 240).is_none());
    }

    /// Only building falls back. The other tools act on a voxel, and pointing
    /// at bare floor is pointing at nothing for them.
    #[test]
    fn the_other_tools_have_no_target_over_empty_space() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 0.9;
        for tool in [Tool::Erase, Tool::Paint, Tool::Pick] {
            e.tool = tool;
            assert!(e.target_at(160.0, 120.0, 320, 240).is_none(), "{tool:?}");
        }
    }

    /// A ray that leaves the volume sideways, or points at the sky, meets no
    /// floor in front of the camera and must not invent one behind it.
    #[test]
    fn the_ground_fallback_stops_at_the_volumes_edge() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 0.9;
        // Far off to the side: the plane is met well outside the footprint.
        assert!(e.target_at(2.0, 120.0, 320, 240).is_none());
        // A level camera looks *along* the plane and never lands on it.
        e.camera.pitch = 0.0;
        assert!(e.target_at(160.0, 120.0, 320, 240).is_none());
    }

    /// Erasing the last voxel must not leave the model unrecoverable — the
    /// state the ground fallback exists to rescue.
    #[test]
    fn a_model_emptied_by_erasing_can_still_be_built_on() {
        let mut e = editor_with_floor();
        e.clear();
        assert_eq!(e.model().filled_count(), 0);

        e.tool = Tool::Build;
        let t = e
            .target_at(160.0, 120.0, 320, 240)
            .expect("the floor is still there");
        e.begin_stroke(t);
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 1);
    }

    #[test]
    fn a_new_model_is_seeded_with_one_voxel_at_its_centre() {
        let m = new_model(32);
        assert_eq!(m.filled_count(), 1);
        assert_eq!(m.get(16, 16, 16), 1);
    }

    /// The seed sits on the work plane rather than over or under it, so the
    /// first thing you see and the surface you build on are the same thing.
    #[test]
    fn the_seed_stands_on_the_work_plane() {
        let e = Editor::new(new_model(32), PathBuf::from("t.vxm"));
        assert_eq!(e.ground_y(), 16);
        assert_eq!(e.model().get(16, e.ground_y(), 16), 1);
    }

    /// Opening frames the volume rather than the contents: a one-voxel seed
    /// framed to its own bounds fills the window with a single cube.
    #[test]
    fn opening_frames_the_volume_not_the_seed() {
        let e = Editor::new(new_model(32), PathBuf::from("t.vxm"));
        assert!(
            e.camera.distance > 32.0,
            "framed to {} — that is the seed, not the volume",
            e.camera.distance
        );
    }

    #[test]
    fn a_click_is_one_undo_step_and_marks_the_model_dirty() {
        let mut e = editor_with_floor();
        let before = e.model().filled_count();
        let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(t);
        e.end_stroke();

        assert_eq!(e.model().filled_count(), before + 1);
        assert_eq!(e.undo_depth(), 1);
        assert!(e.is_dirty());

        e.undo();
        assert_eq!(e.model().filled_count(), before);
    }

    /// The whole point of `Drag::plane`: dragging a build across a floor must
    /// stay on one layer instead of climbing towards the camera.
    #[test]
    fn a_build_drag_stays_on_the_plane_it_started_on() {
        let mut e = editor_with_floor();
        let start = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(start);
        // Sweep across the floor; every placement must land on y == 1.
        for px in (60..260).step_by(4) {
            if let Some(t) = e.target_at(px as f32, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();

        let placed: Vec<_> = e
            .model()
            .iter_filled()
            .filter(|(p, _)| p[1] > 0)
            .map(|(p, _)| p[1])
            .collect();
        assert!(
            placed.len() > 1,
            "the drag should have placed several voxels"
        );
        assert!(
            placed.iter().all(|y| *y == 1),
            "climbed off the plane: {placed:?}"
        );
        assert_eq!(e.undo_depth(), 1, "a drag is one undo step");
    }

    /// `a_build_drag_stays_on_the_plane_it_started_on` passes with `Drag::plane`
    /// removed entirely — its floor is bare, and the mask alone already stops a
    /// stroke re-targeting onto its own work. So it does not test the pin.
    ///
    /// This does. The step is there **before** the stroke starts, so the mask
    /// cannot hide it: without the pin the drag walks off the step onto the
    /// floor and builds on both, which is the "climbing its own work" failure
    /// wearing different clothes. Measured, with the pin taken out:
    ///
    /// ```text
    /// pin present   levels placed on = {3}
    /// pin removed   levels placed on = {1, 3}
    /// ```
    ///
    /// at every camera pitch tried, so it is not an artefact of one angle.
    #[test]
    fn a_build_drag_stays_on_its_plane_over_geometry_that_was_already_there() {
        for pitch in [0.15f32, 0.3, 0.5, 0.8] {
            let mut e = editor_with_floor();
            for x in 4..8 {
                for z in 0..8 {
                    e.model.set(x, 1, z, 6);
                    e.model.set(x, 2, z, 6);
                }
            }
            e.camera.pitch = pitch;
            e.color = 9;
            let start = e
                .target_at(160.0, 150.0, 320, 240)
                .expect("a hit to start from");
            e.begin_stroke(start);
            for px in (40..280).step_by(3) {
                if let Some(t) = e.target_at(px as f32, 150.0, 320, 240) {
                    e.continue_stroke(t);
                }
            }
            e.end_stroke();

            let levels: std::collections::BTreeSet<u16> = e
                .model()
                .iter_filled()
                .filter(|(_, v)| *v == 9)
                .map(|(p, _)| p[1])
                .collect();
            assert_eq!(
                levels.len(),
                1,
                "pitch {pitch}: the drag left its plane and built on {levels:?}"
            );
        }
    }

    #[test]
    fn mirroring_places_the_reflected_voxel_too() {
        let mut e = editor_with_floor();
        e.mirror[0] = true;
        let t = e.target_at(120.0, 100.0, 320, 240).unwrap();
        e.begin_stroke(t);
        e.end_stroke();

        let [x, y, z] = t.cell;
        assert_eq!(e.model().get(x, y, z), e.color);
        assert_eq!(e.model().get(7 - x, y, z), e.color);
    }

    /// A voxel exactly on the mirror axis is its own reflection. Writing it
    /// twice must still be one cell, or an odd-width model builds undo steps
    /// full of duplicates.
    #[test]
    fn mirroring_a_centre_voxel_writes_one_cell() {
        let mut model = VoxelModel::new(7, 8, 8);
        for x in 0..7 {
            for z in 0..8 {
                model.set(x, 0, z, 4);
            }
        }
        let mut e = Editor::new(model, PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 1.4;
        e.mirror[0] = true;

        let before = e.model().filled_count();
        // Straight down the middle: x == 3 is the mirror axis of a 7-wide grid.
        let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
        assert_eq!(t.cell[0], 3, "expected the centre column");
        e.begin_stroke(t);
        e.end_stroke();
        assert_eq!(e.model().filled_count(), before + 1);
    }

    #[test]
    fn pick_takes_the_colour_and_makes_no_edit() {
        let mut e = editor_with_floor();
        e.tool = Tool::Pick;
        e.color = 1;
        let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(t);
        e.end_stroke();

        assert_eq!(e.color, 4);
        assert_eq!(e.undo_depth(), 0);
        assert!(!e.is_dirty());
    }

    /// Air is not a colour. A pick that resolved to index 0 would leave the
    /// build tool painting nothing at all.
    #[test]
    fn pick_never_selects_air() {
        let mut e = editor_with_floor();
        e.color = 7;
        e.tool = Tool::Pick;
        e.begin_stroke(Target {
            voxel: [0, -1, 0],
            face: Face::PosY,
            index: 0,
            cell: [0, 0, 0],
        });
        assert_eq!(e.color, 7);
    }

    /// A slice must hide layers *and* make them unclickable — picking a voxel
    /// you cannot see is the bug this guards.
    #[test]
    fn a_slice_hides_layers_from_both_the_mesh_and_the_pick() {
        let mut model = VoxelModel::new(8, 8, 8);
        for y in 0..8 {
            model.set(4, y, 4, 1);
        }
        let mut e = Editor::new(model, PathBuf::from("t.vxm"));
        // Frame the contents, not the volume: the column stands on the corner
        // of the volume's centre, so a ray aimed at the volume centre grazes
        // it rather than going down its axis.
        e.frame_model();
        e.camera.yaw = 0.0;
        e.camera.pitch = 1.4; // looking down the column
        e.tool = Tool::Erase;

        let full = e.target_at(160.0, 120.0, 320, 240).unwrap();
        assert!(
            full.voxel[1] >= 3,
            "the unsliced pick must be above the cut"
        );

        e.set_slice(Some(3));
        assert!(e.mesh().quads().all(|q| q.voxel[1] < 3));
        let sliced = e.target_at(160.0, 120.0, 320, 240).unwrap();
        assert_eq!(sliced.voxel[1], 2, "should pick the top of the slice");
    }

    #[test]
    fn the_slice_clamps_and_turns_itself_off_at_full_height() {
        let mut e = editor_with_floor();
        e.set_slice(Some(999));
        assert_eq!(e.slice, None, "a slice at full height hides nothing");
        e.set_slice(Some(0));
        assert_eq!(e.slice, Some(1));
        e.nudge_slice(-5);
        assert_eq!(e.slice, Some(1));
    }

    #[test]
    fn colour_stepping_wraps_within_the_paintable_range() {
        let mut e = editor_with_floor();
        e.color = 1;
        e.nudge_color(-1);
        assert_eq!(e.color, 255);
        e.nudge_color(1);
        assert_eq!(e.color, 1);
    }

    #[test]
    fn clear_is_one_undoable_step() {
        let mut e = editor_with_floor();
        let before = e.model().filled_count();
        e.clear();
        assert_eq!(e.model().filled_count(), 0);
        e.undo();
        assert_eq!(e.model().filled_count(), before);
    }

    #[test]
    fn a_missing_file_starts_a_seeded_model_of_the_requested_size() {
        let dir = std::env::temp_dir().join("voxeler-open-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = open_or_create(&dir.join("new.vxm"), 32).unwrap();
        assert_eq!(m.size(), [32, 32, 32]);
        assert_eq!(m.filled_count(), 1, "a new model gets one voxel to click");
        assert_eq!(m.get(16, 16, 16), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_clears_dirty_but_exporting_elsewhere_does_not() {
        let dir = std::env::temp_dir().join("voxeler-save-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut e = editor_with_floor();
        e.path = dir.join("m.vxm");
        let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(t);
        e.end_stroke();
        assert!(e.is_dirty());

        e.export_vox();
        assert!(e.is_dirty(), "an export is not a save of the working file");
        assert!(dir.join("m.vox").exists());

        e.save();
        assert!(!e.is_dirty());

        // And it round-trips back through the working path.
        let reloaded = format::load(&dir.join("m.vxm")).unwrap();
        assert_eq!(reloaded.filled_count(), e.model().filled_count());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A target aimed straight at a cell, without going through the camera.
    /// The span tests care about which cells an edit reaches, and routing that
    /// through a pixel coordinate would only make the fixture harder to read.
    fn hit(voxel: [i32; 3], index: u8) -> Target {
        Target {
            voxel,
            face: Face::PosY,
            index,
            cell: voxel,
        }
    }

    fn build_on(voxel: [i32; 3], index: u8) -> Target {
        Target {
            cell: [voxel[0], voxel[1] + 1, voxel[2]],
            ..hit(voxel, index)
        }
    }

    #[test]
    fn an_axis_fill_extrudes_to_the_far_wall() {
        let mut e = editor_with_floor();
        e.span = Span::Axis;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();

        for y in 1..8 {
            assert_eq!(e.model().get(3, y, 3), e.color, "y={y}");
        }
        assert_eq!(e.model().filled_count(), 64 + 7);
        assert_eq!(e.undo_depth(), 1, "a fill is one undo step");
    }

    #[test]
    fn a_plane_fill_covers_the_whole_face_and_undoes_in_one_step() {
        let mut e = editor_with_floor();
        e.span = Span::Plane;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();

        assert_eq!(
            e.model().filled_count(),
            128,
            "a second layer over the floor"
        );
        assert!(e.model().iter_filled().all(|(p, _)| p[1] < 2));
        e.undo();
        assert_eq!(e.model().filled_count(), 64);
    }

    /// The bounded-by-colour rule, seen from the editor: a continuous paint
    /// recolours the part it was aimed at and stops at the seam.
    #[test]
    fn a_volume_paint_stops_where_the_colour_does() {
        let mut e = editor_with_floor();
        for z in 0..8 {
            for x in 4..8 {
                e.model.set(x, 0, z, 5);
            }
        }
        e.tool = Tool::Paint;
        e.span = Span::Volume;
        e.color = 9;
        e.begin_stroke(hit([1, 0, 1], 4));
        e.end_stroke();

        assert_eq!(e.model().get(1, 0, 1), 9);
        assert_eq!(e.model().get(3, 0, 7), 9, "the whole half it was aimed at");
        assert_eq!(e.model().get(4, 0, 1), 5, "the other colour is a boundary");
        assert_eq!(e.model().filled_count(), 64, "a paint creates nothing");
    }

    // -- selection -------------------------------------------------------

    /// A box selects the voxels in it, not the box: air inside carries nothing,
    /// and moving the result must not drag a cube of nothing along.
    #[test]
    fn a_box_selects_the_voxels_inside_it_and_not_the_air() {
        let mut e = editor_with_floor();
        e.model.set(2, 3, 2, 7);
        assert_eq!(
            e.select_box([0, 0, 0], [7, 7, 7]),
            65,
            "the floor and the speck"
        );

        let sel = e.selection.as_ref().unwrap();
        assert_eq!(sel.layer(), 0);
        assert_eq!(sel.bounds(), Some(([0, 0, 0], [7, 3, 7])));
        assert!(sel
            .cells()
            .all(|c| e.model().get_in(0, c[0], c[1], c[2]) != 0));
    }

    /// Connected by material, not by colour — a part made of two colours is one
    /// part, and a selection that stopped at the seam would be the wrong answer.
    #[test]
    fn a_connected_selection_crosses_a_colour_seam_and_stops_at_air() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 2..6 {
            e.model.set(x, 1, 1, if x < 4 { 3 } else { 9 });
        }
        e.model.set(12, 1, 1, 3); // a separate piece

        assert_eq!(
            e.select_connected([2, 1, 1], None),
            4,
            "both colours, one part"
        );
        assert_eq!(
            e.selection.as_ref().unwrap().bounds(),
            Some(([2, 1, 1], [5, 1, 1]))
        );

        // Air is not a part.
        assert_eq!(e.select_connected([8, 8, 8], None), 0);
        assert!(e.selection.is_none());
    }

    /// The motivating case: a limb is attached to the body, so connectivity
    /// alone answers "the whole figure". The box is what makes "this arm"
    /// sayable.
    #[test]
    fn a_box_holds_a_connected_selection_to_one_limb() {
        let mut e = Editor::new(VoxelModel::new(24, 24, 24), PathBuf::from("t.vxm"));
        for z in 10..14 {
            for y in 4..16 {
                for x in 10..14 {
                    e.model.set(x, y, z, 4); // torso
                }
            }
        }
        for z in 11..13 {
            for y in 10..16 {
                for x in 7..10 {
                    e.model.set(x, y, z, 9); // an arm, touching it
                }
            }
        }
        let whole = e.select_connected([8, 12, 11], None);
        assert!(
            whole > 200,
            "unbounded, the arm is the whole figure: {whole}"
        );

        let arm = e.select_connected([8, 12, 11], Some(Bounds::new([7, 0, 0], [3, 24, 24])));
        assert_eq!(arm, 3 * 6 * 2, "just the arm");
        assert_eq!(
            e.selection.as_ref().unwrap().bounds(),
            Some(([7, 10, 11], [9, 15, 12]))
        );
    }

    #[test]
    fn moving_a_selection_takes_the_voxels_with_it_in_one_step() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 2..5 {
            e.model.set(x, 1, 1, 4);
        }
        e.select_connected([2, 1, 1], None);

        let r = e.move_selection([0, 5, 0]).unwrap();
        assert_eq!(r.moved, 3);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.overwritten, 0);
        assert_eq!(e.model().filled_count(), 3, "moved, not copied");
        for x in 2..5 {
            assert_eq!(e.model().get(x, 1, 1), 0, "the source is empty");
            assert_eq!(e.model().get(x, 6, 1), 4);
        }
        assert_eq!(e.undo_depth(), 1);

        // The selection follows, so the move can be repeated.
        assert_eq!(
            e.selection.as_ref().unwrap().bounds(),
            Some(([2, 6, 1], [4, 6, 1]))
        );
        e.move_selection([0, 1, 0]).unwrap();
        assert_eq!(e.model().get(2, 7, 1), 4);
    }

    /// The case a naive implementation eats: a move shorter than the selection
    /// overlaps itself, and reading colours as it went would carry a voxel
    /// along instead of leaving it where it landed.
    #[test]
    fn a_move_that_overlaps_itself_keeps_every_voxel() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 2..8 {
            e.model.set(x, 1, 1, (x as u8) + 10);
        }
        e.select_box([2, 1, 1], [7, 1, 1]);

        let r = e.move_selection([1, 0, 0]).unwrap();
        assert_eq!(r.moved, 6);
        assert_eq!(e.model().filled_count(), 6, "none lost, none duplicated");
        assert_eq!(e.model().get(2, 1, 1), 0, "the vacated cell");
        for x in 2..8 {
            assert_eq!(
                e.model().get(x + 1, 1, 1),
                (x as u8) + 10,
                "colour {x} arrived intact"
            );
        }
    }

    #[test]
    fn a_move_off_the_edge_loses_what_leaves_and_counts_it() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        for x in 0..4 {
            e.model.set(x, 1, 1, 4);
        }
        e.select_box([0, 1, 1], [3, 1, 1]);

        let r = e.move_selection([6, 0, 0]).unwrap();
        assert_eq!(r.moved, 2, "two fitted");
        assert_eq!(r.dropped, 2, "two went off the end");
        assert_eq!(e.model().filled_count(), 2);
    }

    #[test]
    fn a_move_onto_occupied_cells_reports_what_it_replaced() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        e.model.set(1, 1, 1, 4);
        e.model.set(5, 1, 1, 9);
        e.select_box([1, 1, 1], [1, 1, 1]);

        let r = e.move_selection([4, 0, 0]).unwrap();
        assert_eq!(r.overwritten, 1);
        assert_eq!(e.model().get(5, 1, 1), 4, "the mover won");
        assert_eq!(e.model().filled_count(), 1);
    }

    /// A selection names coordinates, and an undo changes what is at them.
    /// Keeping it would leave it pointing at cells it was not made from.
    #[test]
    fn undo_puts_the_voxels_back_and_drops_the_selection() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        e.model.set(2, 1, 1, 4);
        e.select_connected([2, 1, 1], None);
        e.move_selection([0, 4, 0]).unwrap();
        assert_eq!(e.model().get(2, 5, 1), 4);

        e.undo();
        assert_eq!(e.model().get(2, 1, 1), 4, "back where it was");
        assert_eq!(e.model().get(2, 5, 1), 0);
        assert!(e.selection.is_none());
    }

    #[test]
    fn moving_with_nothing_selected_says_so_and_changes_nothing() {
        let mut e = editor_with_floor();
        assert!(e.move_selection([1, 0, 0]).is_err());
        assert_eq!(e.undo_depth(), 0);
        // And a move of nowhere is not an undo step either.
        e.select_box([0, 0, 0], [7, 0, 7]);
        assert_eq!(e.move_selection([0, 0, 0]).unwrap(), MoveReport::default());
        assert_eq!(e.undo_depth(), 0);
    }

    /// The selection belongs to the layer it was made on, so a later change of
    /// active layer moves the voxels that were shown, not different ones.
    #[test]
    fn a_selection_keeps_the_layer_it_was_made_on() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.select_layer(0);
        e.select_box([0, 0, 0], [7, 0, 7]);
        assert_eq!(e.selection.as_ref().unwrap().layer(), 0);

        e.select_layer(1);
        e.move_selection([0, 4, 0]).unwrap();
        assert_eq!(e.model().get_in(0, 3, 4, 3), 4, "moved on layer 0");
        assert_eq!(e.model().layers()[1].filled_count(), 0, "layer 1 untouched");
    }

    /// Copy then paste at the corner it came from is exactly what was copied —
    /// which is what the relative-to-the-low-corner storage is for.
    #[test]
    fn pasting_where_it_came_from_puts_back_what_was_copied() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 4..7 {
            e.model.set(x, 2, 3, (x + 20) as u8);
        }
        e.select_box([0, 0, 0], [15, 15, 15]);
        let before: Vec<_> = e.model().iter_filled().collect();

        assert_eq!(e.copy_selection().unwrap(), 3);
        assert_eq!(e.clipboard.as_ref().unwrap().size(), [3, 1, 1]);
        e.paste([4, 2, 3]).unwrap();
        assert_eq!(e.model().iter_filled().collect::<Vec<_>>(), before);
    }

    #[test]
    fn a_paste_lands_selected_so_it_can_be_moved_at_once() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        e.model.set(1, 1, 1, 7);
        e.select_box([1, 1, 1], [1, 1, 1]);
        e.copy_selection().unwrap();

        let r = e.paste([8, 8, 8]).unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(e.model().filled_count(), 2, "the original is still there");
        assert_eq!(
            e.selection.as_ref().unwrap().bounds(),
            Some(([8, 8, 8], [8, 8, 8])),
            "what landed is what is selected"
        );

        // So the chain works without saying where anything went.
        e.move_selection([0, 1, 0]).unwrap();
        assert_eq!(e.model().get(8, 9, 8), 7);
        assert_eq!(e.model().get(1, 1, 1), 7);
    }

    /// A paste goes to the active layer, not the one the voxels came from —
    /// which is what makes copying between layers a paste rather than a
    /// separate tool.
    #[test]
    fn a_paste_writes_to_the_active_layer() {
        let mut e = editor_with_floor();
        e.select_box([0, 0, 0], [7, 0, 7]);
        e.copy_selection().unwrap();

        e.add_layer();
        e.paste([0, 4, 0]).unwrap();
        assert_eq!(
            e.model().layers()[1].filled_count(),
            64,
            "landed on the new layer"
        );
        assert_eq!(
            e.model().layers()[0].filled_count(),
            64,
            "the floor is untouched"
        );
        assert_eq!(e.selection.as_ref().unwrap().layer(), 1);
    }

    #[test]
    fn a_cut_takes_the_voxels_and_leaves_nothing_selected() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 2..5 {
            e.model.set(x, 1, 1, 4);
        }
        e.select_box([2, 1, 1], [4, 1, 1]);

        assert_eq!(e.cut_selection().unwrap(), 3);
        assert_eq!(e.model().filled_count(), 0);
        assert!(e.selection.is_none(), "nothing is there to be selected");
        assert_eq!(e.clipboard.as_ref().unwrap().len(), 3);

        e.paste([8, 1, 1]).unwrap();
        assert_eq!(e.model().filled_count(), 3);
        assert_eq!(e.model().get(8, 1, 1), 4);
    }

    /// The clipboard is not the document: undo puts the *model* back, and a
    /// clipboard that emptied itself when you undid the copy would be a
    /// surprise rather than a rule.
    #[test]
    fn the_clipboard_survives_undo_though_the_selection_does_not() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        e.model.set(1, 1, 1, 7);
        e.select_box([1, 1, 1], [1, 1, 1]);
        e.copy_selection().unwrap();
        e.paste([5, 5, 5]).unwrap();

        e.undo();
        assert_eq!(e.model().get(5, 5, 5), 0, "the paste was undone");
        assert!(e.selection.is_none());
        assert_eq!(
            e.clipboard.as_ref().unwrap().len(),
            1,
            "the clipboard stands"
        );
        // And it can be pasted again.
        e.paste([5, 5, 5]).unwrap();
        assert_eq!(e.model().get(5, 5, 5), 7);
    }

    /// The mirrored-pair case the tool exists for: duplicate, then flip, with
    /// no coordinate arithmetic in between.
    #[test]
    fn duplicate_then_flip_is_a_mirrored_pair() {
        let mut e = Editor::new(VoxelModel::new(24, 24, 24), PathBuf::from("t.vxm"));
        // An asymmetric "arm": three cells with a bend.
        for cell in [[4, 4, 4], [5, 4, 4], [6, 4, 4], [6, 5, 4]] {
            e.model.set(cell[0], cell[1], cell[2], 9);
        }
        e.select_connected([4, 4, 4], None);

        e.duplicate_selection([0, 0, 6]).unwrap();
        assert_eq!(e.model().filled_count(), 8, "two arms now");
        e.flip_selection(0).unwrap();

        // The copy is mirrored: its bend is at the other end.
        assert_eq!(e.model().get(6, 5, 4), 9, "the original still bends at +x");
        assert_eq!(e.model().get(4, 5, 10), 9, "the copy bends at -x");
        assert_eq!(e.model().filled_count(), 8);
    }

    #[test]
    fn pasting_off_the_edge_keeps_what_fits_and_counts_the_rest() {
        let mut e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        for x in 0..4 {
            e.model.set(x, 1, 1, 4);
        }
        e.select_box([0, 1, 1], [3, 1, 1]);
        e.copy_selection().unwrap();

        let r = e.paste([6, 1, 1]).unwrap();
        assert_eq!(r.moved, 2);
        assert_eq!(r.dropped, 2);
    }

    #[test]
    fn copying_and_pasting_need_something_to_work_with() {
        let mut e = editor_with_floor();
        assert!(e.copy_selection().is_err());
        assert!(e.cut_selection().is_err());
        assert!(e.duplicate_selection([1, 0, 0]).is_err());
        assert!(e.paste([0, 0, 0]).is_err(), "the clipboard is empty");
        assert_eq!(e.undo_depth(), 0);
    }

    fn filled(e: &Editor) -> std::collections::BTreeSet<[i32; 3]> {
        e.model()
            .iter_filled()
            .map(|(p, _)| [p[0] as i32, p[1] as i32, p[2] as i32])
            .collect()
    }

    /// An L, so the direction of a turn is pinned by the cells it produces
    /// rather than by a description of it. The sense is the right-hand rule
    /// about the positive axis — the same one face winding uses — so +X goes
    /// to -Z for a turn about +Y.
    #[test]
    fn a_quarter_turn_goes_counter_clockwise_about_the_positive_axis() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        // In the y = 1 plane, from a corner at (4, 1, 4): an arm along +X and
        // a shorter one along +Z.
        for x in 4..7 {
            e.model.set(x, 1, 4, 3);
        }
        e.model.set(4, 1, 5, 3);
        e.select_box([0, 0, 0], [15, 15, 15]);

        e.rotate_selection(1, 1).unwrap();
        assert_eq!(
            filled(&e),
            [[4, 1, 4], [4, 1, 5], [4, 1, 6], [5, 1, 6]]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the +X arm now runs toward -Z from its root"
        );
    }

    /// Turning something to look at it and turning it back has to be exact.
    /// Preserving the box's *centre* is not: a quarter turn swaps two extents,
    /// and where those differ in parity the centre falls between cells, so the
    /// rounding accumulates and a there-and-back came home a cell out.
    #[test]
    fn a_rotation_and_its_opposite_come_back_exactly() {
        for (w, d) in [(4usize, 1usize), (3, 1), (5, 2), (2, 2), (1, 1), (7, 4)] {
            let mut e = Editor::new(VoxelModel::new(24, 8, 24), PathBuf::from("t.vxm"));
            for z in 0..d {
                for x in 0..w {
                    e.model
                        .set(4 + x as i32, 1, 4 + z as i32, (x + z * 8 + 1) as u8);
                }
            }
            e.select_box([0, 0, 0], [23, 7, 23]);
            let start = filled(&e);

            e.rotate_selection(1, 1).unwrap();
            e.rotate_selection(1, -1).unwrap();
            assert_eq!(filled(&e), start, "{w}x{d}: there and back again");

            for _ in 0..4 {
                e.rotate_selection(1, 1).unwrap();
            }
            assert_eq!(filled(&e), start, "{w}x{d}: four quarter turns");
        }
    }

    #[test]
    fn a_turn_of_none_is_not_an_edit_and_turns_wrap() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for x in 4..8 {
            e.model.set(x, 1, 4, 3);
        }
        e.select_box([0, 0, 0], [15, 15, 15]);
        let steps = e.undo_depth();
        assert_eq!(e.rotate_selection(1, 0).unwrap(), MoveReport::default());
        assert_eq!(e.rotate_selection(1, 4).unwrap(), MoveReport::default());
        assert_eq!(e.undo_depth(), steps, "neither touched the model");

        // A shape with an arm on each axis, so no turn is a coincidental no-op
        // — a bar along X is genuinely unchanged by a turn about X.
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for cell in [[4, 1, 4], [6, 1, 4], [4, 3, 4], [4, 1, 6]] {
            e.model.set(cell[0], cell[1], cell[2], 3);
        }
        e.select_box([0, 0, 0], [15, 15, 15]);
        for axis in 0..3 {
            let before = filled(&e);
            e.rotate_selection(axis, 1).unwrap();
            assert_ne!(filled(&e), before, "axis {axis}");
            e.rotate_selection(axis, -1).unwrap();
            assert_eq!(filled(&e), before, "axis {axis} came back");
        }
    }

    /// A rotation of a squat shape overlaps its own source, which is the case
    /// that eats voxels if colours are read as the transform goes.
    #[test]
    fn a_rotation_that_overlaps_itself_keeps_every_voxel() {
        let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
        for z in 4..8 {
            for x in 4..8 {
                e.model.set(x, 1, z, (x * 4 + z) as u8);
            }
        }
        e.select_box([4, 1, 4], [7, 1, 7]);

        e.rotate_selection(1, 1).unwrap();
        assert_eq!(e.model().filled_count(), 16, "none lost, none duplicated");
        let colours: std::collections::HashSet<u8> =
            e.model().iter_filled().map(|(_, v)| v).collect();
        assert_eq!(colours.len(), 16, "and every colour is still distinct");
    }

    /// Flipping is about the selection, not the scene — the difference between
    /// turning a hand over and sending it to the far wall.
    #[test]
    fn a_flip_mirrors_within_the_selections_own_box() {
        let mut e = Editor::new(VoxelModel::new(32, 16, 16), PathBuf::from("t.vxm"));
        e.model.set(4, 1, 1, 3);
        e.model.set(5, 1, 1, 9);
        e.select_box([4, 1, 1], [5, 1, 1]);

        e.flip_selection(0).unwrap();
        assert_eq!(e.model().get(4, 1, 1), 9, "the two swapped");
        assert_eq!(e.model().get(5, 1, 1), 3);
        assert_eq!(
            e.model().filled_count(),
            2,
            "and nothing moved across the scene"
        );
    }

    /// An even extent has no centre cell, and `lo + hi - v` needs none — the
    /// rounding a rotation cannot avoid does not apply to a flip.
    #[test]
    fn a_flip_is_exact_at_any_extent_and_undoes_itself() {
        for width in [1usize, 2, 3, 8] {
            let mut e = Editor::new(VoxelModel::new(16, 16, 16), PathBuf::from("t.vxm"));
            for x in 0..width {
                e.model.set(2 + x as i32, 1, 1, (x + 1) as u8);
            }
            e.select_box([0, 0, 0], [15, 15, 15]);
            let before: Vec<_> = e.model().iter_filled().collect();

            e.flip_selection(0).unwrap();
            e.flip_selection(0).unwrap();
            assert_eq!(
                e.model().iter_filled().collect::<Vec<_>>(),
                before,
                "flipping twice at width {width} is where you started"
            );
        }
    }

    #[test]
    fn a_transform_needs_a_selection_and_is_one_undo_step() {
        let mut e = editor_with_floor();
        assert!(e.flip_selection(0).is_err());
        assert!(e.rotate_selection(1, 1).is_err());
        assert_eq!(e.undo_depth(), 0);

        e.select_box([0, 0, 0], [7, 0, 7]);
        e.rotate_selection(1, 1).unwrap();
        assert_eq!(e.undo_depth(), 1);
        e.flip_selection(2).unwrap();
        assert_eq!(e.undo_depth(), 2);
    }

    /// Every layer at once, and one `ctrl+Z` puts the whole thing back — scene
    /// size included, which is the part a cell-diff history could not record.
    #[test]
    fn subdividing_scales_every_layer_and_undoes_in_one_step() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        let before_size = e.model().size();
        let before_voxels = e.model().filled_count();
        let steps = e.undo_depth();

        e.subdivide(2).unwrap();
        assert_eq!(e.model().size(), [16, 16, 16]);
        assert_eq!(e.model().filled_count(), before_voxels * 8);
        assert_eq!(e.model().layer_count(), 2);
        assert_eq!(
            e.model().get_in(1, 6, 2, 6),
            6,
            "the upper layer scaled too"
        );
        assert_eq!(e.undo_depth(), steps + 1, "one step, not one per layer");
        assert!(e.is_dirty());

        e.undo();
        assert_eq!(e.model().size(), before_size, "the scene came back");
        assert_eq!(e.model().filled_count(), before_voxels);
        assert_eq!(e.model().get_in(1, 3, 1, 3), 6);
        // And the edits made before it still apply to the right cells.
        e.undo();
        assert_eq!(e.model().get_in(1, 3, 1, 3), 0);
    }

    /// The camera and the slice are measured in voxels, so they move with the
    /// scene — otherwise the model appears to leap towards you and the cut
    /// lands somewhere else in the shape.
    #[test]
    fn subdividing_carries_the_view_with_it() {
        let mut e = editor_with_floor();
        e.set_slice(Some(4));
        let distance = e.camera.distance;

        e.subdivide(2).unwrap();
        assert_eq!(e.camera.distance, distance * 2.0);
        assert_eq!(e.slice, Some(8));
    }

    #[test]
    fn a_subdivide_that_would_not_fit_changes_nothing() {
        let mut model = VoxelModel::new(200, 8, 8);
        model.set(1, 1, 1, 4);
        let mut e = Editor::new(model, PathBuf::from("t.vxm"));
        let err = e.subdivide(2).unwrap_err();
        assert!(err.contains("256"), "{err}");
        assert_eq!(e.model().size(), [200, 8, 8]);
        assert_eq!(e.undo_depth(), 0, "and it is not an undo step");
        assert!(!e.is_dirty());
    }

    /// A fill is a click, not a stroke. Dragging on after one must not re-flood
    /// from wherever the pointer has reached.
    #[test]
    fn a_region_span_applies_once_however_far_the_pointer_travels() {
        let mut e = editor_with_floor();
        e.span = Span::Plane;
        let start = e.target_at(160.0, 120.0, 320, 240).unwrap();
        e.begin_stroke(start);
        let after_the_click = e.model().filled_count();

        for px in (60..260).step_by(4) {
            if let Some(t) = e.target_at(px as f32, 120.0, 320, 240) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();

        assert_eq!(e.model().filled_count(), after_the_click);
        assert_eq!(e.undo_depth(), 1);
    }

    /// A brush covers cells the ray never touched, which is where "a build only
    /// fills air" stops being true by construction and has to be enforced.
    #[test]
    fn a_brush_build_fills_the_air_it_covers_and_repaints_nothing() {
        let mut e = editor_with_floor();
        e.brush = Brush {
            radius: 1,
            shape: BrushShape::Cube,
        };
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();

        // A 3³ centred at y = 1 covers nine cells of the floor and eighteen of
        // the air above it.
        assert_eq!(e.model().filled_count(), 64 + 18);
        assert_eq!(e.model().get(2, 2, 2), 6);
        assert_eq!(
            e.model().get(3, 0, 3),
            4,
            "the floor under the brush is not repainted"
        );
        assert_eq!(e.undo_depth(), 1);
    }

    /// And the other half of that rule: an erase clears material without
    /// leaving anything behind in the air it also covered.
    #[test]
    fn a_brush_erase_clears_material_and_creates_none() {
        let mut e = editor_with_floor();
        e.tool = Tool::Erase;
        e.brush = Brush {
            radius: 1,
            shape: BrushShape::Cube,
        };
        e.begin_stroke(hit([3, 0, 3], 4));
        e.end_stroke();

        assert_eq!(e.model().filled_count(), 64 - 9);
        assert_eq!(e.model().get(3, 0, 3), 0);
    }

    /// The point of mirroring: it reflects the *edit*, not the model. A model
    /// that is deliberately asymmetric has to come through a mirrored stroke
    /// with its asymmetry intact.
    #[test]
    fn mirroring_reflects_the_edit_and_leaves_the_rest_of_the_model_alone() {
        let mut e = editor_with_floor();
        // A lump on one side, with nothing facing it across the mirror plane.
        e.model.set(6, 1, 6, 7);
        e.mirror[0] = true;
        e.color = 3;

        e.begin_stroke(build_on([1, 0, 1], 4));
        e.end_stroke();

        assert_eq!(e.model().get(1, 1, 1), 3);
        assert_eq!(e.model().get(6, 1, 1), 3, "the reflection of the edit");
        assert_eq!(e.model().get(6, 1, 6), 7, "the lump is untouched");
        assert_eq!(
            e.model().get(1, 1, 6),
            0,
            "mirroring does not go back and symmetrise what was already there"
        );
    }

    #[test]
    fn two_mirror_planes_write_the_four_corners_of_one_edit() {
        let mut e = editor_with_floor();
        e.mirror = [true, false, true];
        e.color = 5;
        e.begin_stroke(build_on([2, 0, 1], 4));
        e.end_stroke();

        for (x, z) in [(2, 1), (5, 1), (2, 6), (5, 6)] {
            assert_eq!(e.model().get(x, 1, z), 5, "at ({x}, {z})");
        }
        assert_eq!(e.model().filled_count(), 64 + 4);
        assert_eq!(e.undo_depth(), 1, "one edit, however many times it lands");
    }

    /// Mirroring composes with a span rather than replacing it: the region is
    /// found once, and the whole region is what gets reflected.
    #[test]
    fn a_mirrored_fill_reflects_every_cell_the_span_reached() {
        let mut e = editor_with_floor();
        e.span = Span::Axis;
        e.mirror[0] = true;
        e.begin_stroke(build_on([1, 0, 1], 4));
        e.end_stroke();

        for y in 1..8 {
            assert_eq!(e.model().get(1, y, 1), e.color, "y={y}");
            assert_eq!(e.model().get(6, y, 1), e.color, "reflected, y={y}");
        }
        assert_eq!(e.model().filled_count(), 64 + 14);
    }

    #[test]
    fn resizing_the_brush_clamps_and_leaves_the_span_alone() {
        let mut e = editor_with_floor();
        e.span = Span::Volume;
        e.nudge_brush(1);
        assert_eq!(e.brush.radius, 1);
        assert_eq!(e.brush.edge(), 3);
        // It used to snap back to Voxel, because the brush was that span's
        // shape. It is a reach now, so resizing it under a flood is a
        // deliberate thing to do rather than a mistake to correct.
        assert_eq!(e.span, Span::Volume, "the span is left where it was");

        e.nudge_brush(-5);
        assert_eq!(e.brush.radius, 0, "a brush never shrinks past one cell");
        for _ in 0..20 {
            e.nudge_brush(1);
        }
        assert_eq!(e.brush.radius, MAX_BRUSH);
    }

    /// A bounded plane span is meant to be *worked* — dragged across a surface
    /// laying down a patch at a time. This drives the same sequence `app.rs`
    /// does: begin, then a target per pointer position, resolved through the
    /// same `target_at` the window uses.
    #[test]
    fn a_bounded_plane_span_strokes_without_climbing_its_own_work() {
        let mut e = editor_with_floor();
        e.tool = Tool::Build;
        e.span = Span::Plane;
        e.brush.radius = 1;
        e.color = 5;
        let (w, h) = (320, 240);

        let start = e.target_at(150.0, 120.0, w, h).expect("a floor hit");
        e.begin_stroke(start);
        for i in 1..10 {
            if let Some(t) = e.target_at(150.0 + i as f32 * 3.0, 120.0, w, h) {
                e.continue_stroke(t);
            }
        }
        e.end_stroke();

        let laid: Vec<[u16; 3]> = e
            .model()
            .iter_filled()
            .filter(|(_, v)| *v == 5)
            .map(|(p, _)| p)
            .collect();
        assert!(!laid.is_empty(), "the stroke laid something down");
        // It kept going rather than stopping after the first application —
        // which is the whole point of moving the seam to boundedness.
        assert!(laid.len() > 1, "a bounded region strokes: {laid:?}");
        // And every cell of it is one layer above the floor. A stroke that
        // re-targeted onto its own work would climb towards the camera.
        assert!(
            laid.iter().all(|p| p[1] == 1),
            "no staircase, all at y = 1: {laid:?}"
        );
    }

    /// A bump standing proud of a floor, and the flatten that takes it off
    /// without digging a hole where it stood.
    #[test]
    fn flatten_levels_to_the_plane_the_stroke_started_on() {
        let mut e = editor_with_floor();
        e.model.set(3, 1, 3, 7);
        e.model.set(3, 2, 3, 7);
        e.model.set(5, 0, 5, 0);
        e.tool = Tool::Flatten;
        e.brush.radius = 4;
        // Not the floor's own colour, so a fill is visible as a fill.
        e.color = 9;

        let start = e
            .target_at(160.0, 120.0, 320, 240)
            .expect("the floor is under the cursor");
        assert_eq!(start.face, Face::PosY);
        assert_eq!(start.voxel[1], 0, "the hit is the floor cell itself");
        e.begin_stroke(start);
        e.end_stroke();

        assert_eq!(e.model().get(3, 1, 3), 0, "the tower above the plane went");
        assert_eq!(e.model().get(3, 2, 3), 0);
        assert_eq!(e.model().get(5, 0, 5), 9, "the pit at the plane filled");
        assert_eq!(e.model().get(3, 0, 3), 4, "the floor itself is untouched");
    }

    /// Every decision read before any is applied. Written as it goes, one pass
    /// would cascade into itself and eat the surface in a single application.
    #[test]
    fn smooth_takes_a_spur_off_and_fills_a_notch_without_cascading() {
        let mut e = editor_with_floor();
        e.model.set(2, 1, 2, 7);
        e.model.set(5, 0, 5, 0);
        let before = e.model().filled_count();

        e.tool = Tool::Smooth;
        e.brush.radius = 6;
        e.color = 9;
        let start = e.target_at(160.0, 120.0, 320, 240).expect("a hit");
        e.begin_stroke(start);
        e.end_stroke();

        assert_eq!(e.model().get(2, 1, 2), 0, "the spur went");
        assert_eq!(e.model().get(5, 0, 5), 9, "the notch filled");
        // A corner of the slab has exactly two solid face neighbours, so a
        // smooth rounds it off. That is what a smooth is *for*, and worth
        // pinning: it is also why the radius matters, since a smaller brush
        // would never have reached the corners at all.
        assert_eq!(e.model().get(0, 0, 0), 0, "the corner rounded");
        assert!(e.model().get(1, 0, 0) != 0, "the edge beside it did not");
        assert!(e.model().get(4, 0, 4) != 0, "and the middle is untouched");
        // One spur out, one notch in, four corners rounded. A pass that
        // cascaded would have kept eating: the cell behind each rounded corner
        // becomes a corner itself, and a second application would take it.
        let after = e.model().filled_count();
        assert_eq!(
            after,
            before - 1 + 1 - 4,
            "exactly the cells the rule names"
        );
    }

    /// A sculpt tool needs to see material behind a cell as well as air in
    /// front, so it takes the brush ball whatever the span row says.
    #[test]
    fn a_sculpt_tool_ignores_the_span_row() {
        let mut e = editor_with_floor();
        e.model.set(2, 1, 2, 7);
        e.brush.radius = 3;
        e.color = 9;
        // Volume would flood the whole connected floor; the brush must not.
        e.span = Span::Volume;
        e.tool = Tool::Smooth;
        let start = e.target_at(160.0, 120.0, 320, 240).expect("a hit");
        e.begin_stroke(start);
        e.end_stroke();
        assert_eq!(e.model().get(2, 1, 2), 0, "the spur still went");
        // Far corners, outside a radius of three from the middle: a volume
        // flood would have reached them.
        assert!(e.model().get(0, 0, 0) != 0 && e.model().get(7, 0, 7) != 0);
    }

    /// The seam this refactor moved. What makes a fill unstrokeable is that it
    /// has no limit, not that it is a flood.
    #[test]
    fn a_region_is_a_stroke_exactly_when_it_is_bounded() {
        let mut e = editor_with_floor();
        for span in [Span::Axis, Span::Plane, Span::Volume] {
            e.span = span;
            e.brush.radius = 0;
            assert!(!e.region_is_bounded(), "{span:?} with no radius is a click");
            e.brush.radius = 2;
            assert!(e.region_is_bounded(), "{span:?} with a radius is a stroke");
        }
        // A voxel span is the brush, so it is bounded at any radius including
        // none — which is why dragging has always worked for it.
        e.span = Span::Voxel;
        for r in [0, 1, 4] {
            e.brush.radius = r;
            assert!(e.region_is_bounded());
        }
    }

    #[test]
    fn the_summary_names_the_span_and_the_mirror_planes() {
        let mut e = editor_with_floor();
        assert!(!e.summary().contains("MIRROR"));
        e.span = Span::Plane;
        e.toggle_mirror(0);
        e.toggle_mirror(2);
        let s = e.summary();
        assert!(s.contains("BUILD/PLANE"), "{s}");
        assert!(s.contains("MIRROR XZ"), "{s}");
    }

    // -- layers ----------------------------------------------------------

    /// The point of a layer being a grid of its own, seen from the editor: you
    /// can work over another layer without consuming it.
    #[test]
    fn building_on_a_new_layer_leaves_the_one_below_intact() {
        let mut e = editor_with_floor();
        e.add_layer();
        assert_eq!(e.active_layer(), 1);
        e.color = 6;
        e.begin_stroke(hit([3, 0, 3], 4));
        e.end_stroke();

        assert_eq!(e.model().get(3, 0, 3), 6, "the new layer is what shows");
        assert_eq!(e.model().get_in(0, 3, 0, 3), 4, "the floor is untouched");
        assert_eq!(e.model().filled_count(), 64, "still one voxel per cell");

        // And erasing on the upper layer gives the floor back rather than
        // leaving a hole.
        e.tool = Tool::Erase;
        e.begin_stroke(hit([3, 0, 3], 6));
        e.end_stroke();
        assert_eq!(e.model().get(3, 0, 3), 4);
    }

    /// A tool writes to the active layer, so pointing at a voxel some other
    /// layer holds does nothing — and has to say why, or it reads as a broken
    /// editor.
    #[test]
    fn a_click_on_another_layers_voxel_does_nothing_and_says_so() {
        let mut e = editor_with_floor();
        e.model.rename_layer(0, "FLOOR");
        e.add_layer();
        e.tool = Tool::Erase;
        // Adding the layer was itself one step; the click must add none.
        let before = e.undo_depth();

        e.begin_stroke(hit([3, 0, 3], 4));
        e.end_stroke();

        assert_eq!(e.model().get(3, 0, 3), 4, "the floor is on another layer");
        assert_eq!(e.undo_depth(), before);
        assert!(e.status().contains("FLOOR"), "{}", e.status());
    }

    /// A build must not be blocked by a voxel a *higher* layer is showing —
    /// working underneath something is what a lower layer is for.
    #[test]
    fn a_build_under_a_covering_layer_still_lands() {
        let mut e = editor_with_floor();
        e.add_layer(); // layer 1, the cover
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().get(3, 1, 3), 6);

        // Back down to the floor layer and build in the same cell.
        e.select_layer(0);
        e.color = 7;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().get_in(0, 3, 1, 3), 7, "written under the cover");
        assert_eq!(e.model().get(3, 1, 3), 6, "which still covers it");
    }

    /// Deleting a layer is the one destructive layer command, so it has to come
    /// back — including the edits made on it before it went.
    #[test]
    fn deleting_a_layer_is_undoable_with_its_contents() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 65);

        e.delete_layer();
        assert_eq!(e.model().layer_count(), 1);
        assert_eq!(e.model().filled_count(), 64);

        e.undo();
        assert_eq!(e.model().layer_count(), 2);
        assert_eq!(e.model().get_in(1, 3, 1, 3), 6);
        e.undo();
        assert_eq!(e.model().get_in(1, 3, 1, 3), 0, "and the edit before it");
    }

    #[test]
    fn the_last_layer_survives_a_delete() {
        let mut e = editor_with_floor();
        e.delete_layer();
        assert_eq!(e.model().layer_count(), 1);
        assert_eq!(e.undo_depth(), 0);
        assert!(e.status().contains("one layer"), "{}", e.status());
    }

    #[test]
    fn hiding_a_layer_takes_it_out_of_the_mesh_and_off_the_pick() {
        let mut e = editor_with_floor();
        e.toggle_layer_visible();
        assert!(e.mesh().is_empty(), "a hidden layer draws nothing");
        e.tool = Tool::Erase;
        assert!(e.target_at(160.0, 120.0, 320, 240).is_none());
        assert!(e.is_dirty(), "which layers are hidden is part of the model");
        assert_eq!(e.undo_depth(), 0, "but not an undo step");
    }

    /// Moving a layer changes what covers what, and is undoable.
    #[test]
    fn moving_a_layer_changes_what_shows_and_undoes() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.color = 6;
        e.begin_stroke(hit([3, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().get(3, 0, 3), 6);

        e.move_layer(false);
        assert_eq!(e.active_layer(), 0);
        assert_eq!(e.model().get(3, 0, 3), 4, "the floor is on top now");
        e.undo();
        assert_eq!(e.model().get(3, 0, 3), 6);
    }

    #[test]
    fn merging_down_folds_the_layer_and_undoes() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();

        e.merge_layer_down();
        assert_eq!(e.model().layer_count(), 1);
        assert_eq!(e.model().get_in(0, 3, 1, 3), 6);
        assert_eq!(e.model().get_in(0, 3, 0, 3), 4);

        e.undo();
        assert_eq!(e.model().layer_count(), 2);
        assert_eq!(e.model().get_in(0, 3, 1, 3), 0);
    }

    /// `ctrl+N` means "start this model again". A hidden layer left full would
    /// make the next save carry work the user believes they threw away.
    #[test]
    fn clearing_empties_the_hidden_layers_too() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        e.toggle_layer_visible();

        e.clear();
        assert_eq!(e.model().get_in(0, 3, 0, 3), 0);
        assert_eq!(e.model().get_in(1, 3, 1, 3), 0, "the hidden layer as well");
        e.undo();
        assert_eq!(e.model().get_in(1, 3, 1, 3), 6, "and it all comes back");
    }

    /// A fill selects what you can see and writes what you own.
    #[test]
    fn a_fill_is_found_on_the_composite_and_written_to_the_active_layer() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.span = Span::Plane;
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();

        assert_eq!(
            e.model().filled_count(),
            128,
            "the whole floor, seen from above"
        );
        assert_eq!(
            e.model().layers()[1].filled_count(),
            64,
            "all of it on the new layer"
        );
        assert_eq!(
            e.model().layers()[0].filled_count(),
            64,
            "and none of it on the old"
        );
    }

    #[test]
    fn cycling_the_layer_clamps_at_both_ends() {
        let mut e = editor_with_floor();
        e.add_layer();
        e.add_layer();
        assert_eq!(e.active_layer(), 2);
        e.cycle_layer(1);
        assert_eq!(e.active_layer(), 2, "the top of the stack is the top");
        e.cycle_layer(-5);
        assert_eq!(e.active_layer(), 0);
    }

    #[test]
    fn a_rename_takes_text_and_can_be_cancelled() {
        let mut e = editor_with_floor();
        e.begin_rename();
        assert_eq!(e.renaming(), Some("LAYER 1"));
        for _ in 0..7 {
            e.rename_backspace();
        }
        for c in "body
"
        .chars()
        {
            e.rename_push(c);
        }
        assert_eq!(
            e.renaming(),
            Some("body"),
            "a control character is not a name"
        );
        e.commit_rename();
        assert_eq!(e.model().layers()[0].name, "body");
        assert!(e.is_dirty());

        e.begin_rename();
        e.rename_push('x');
        e.cancel_rename();
        assert_eq!(e.model().layers()[0].name, "body");
        assert_eq!(e.renaming(), None);
    }

    #[test]
    fn a_rename_refuses_to_leave_a_layer_nameless() {
        let mut e = editor_with_floor();
        e.begin_rename();
        for _ in 0..10 {
            e.rename_backspace();
        }
        e.rename_push(' ');
        e.commit_rename();
        assert_eq!(e.model().layers()[0].name, "LAYER 1", "the old name stands");
    }

    /// The stack has to reach the file and come back, or layers are a thing
    /// that only exists while the editor is open.
    #[test]
    fn layers_survive_a_save_and_a_reload() {
        let dir = std::env::temp_dir().join("voxeler-layer-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut e = editor_with_floor();
        e.path = dir.join("m.vxm");
        e.add_layer();
        e.begin_rename();
        for _ in 0..7 {
            e.rename_backspace();
        }
        for c in "ARMOUR".chars() {
            e.rename_push(c);
        }
        e.commit_rename();
        e.color = 6;
        e.begin_stroke(build_on([3, 0, 3], 4));
        e.end_stroke();
        e.toggle_layer_visible();
        e.save();
        assert!(!e.is_dirty());

        e.reload();
        assert_eq!(e.model().layer_count(), 2);
        assert_eq!(e.model().layers()[1].name, "ARMOUR");
        assert!(
            !e.model().layers()[1].visible,
            "hidden, and still holding its work"
        );
        assert_eq!(e.model().get_in(1, 3, 1, 3), 6);
        assert_eq!(e.active_layer(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The point of per-layer boxes, from the editor: a scene you can build
    /// across costs only what is drawn in it.
    #[test]
    fn a_layer_grows_to_what_is_drawn_and_costs_only_that() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        assert_eq!(
            e.model().allocated_cells(),
            0,
            "an empty scene allocates nothing"
        );

        e.begin_stroke(hit([20, 5, 10], 0));
        e.end_stroke();
        assert_eq!(e.model().allocated_cells(), 1);
        assert!(e.model().layer_bounds(0).contains(20, 5, 10));

        e.span = Span::Voxel;
        e.brush = Brush {
            radius: 2,
            shape: BrushShape::Cube,
        };
        e.begin_stroke(hit([20, 5, 10], 0));
        e.end_stroke();
        assert_eq!(e.model().layer_bounds(0).size, [5, 5, 5]);
        assert_eq!(e.model().allocated_cells(), 125, "and not 64 cubed");
    }

    /// Boxes keep their high-water mark while you work; the trim key is what
    /// hands the space back.
    #[test]
    fn trimming_gives_back_the_space_an_erase_left_behind() {
        let mut e = editor_with_floor();
        assert_eq!(e.model().allocated_cells(), 64, "the 8x1x8 floor");

        // A *partial* erase, clearing the five columns nearest -X. Erasing the
        // lot is the one case that hands the box back on its own, so it would
        // not show what a trim is for.
        e.tool = Tool::Erase;
        e.span = Span::Voxel;
        e.brush = Brush {
            radius: 4,
            shape: BrushShape::Cube,
        };
        e.begin_stroke(hit([0, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 24, "the three columns left");
        assert_eq!(
            e.model().allocated_cells(),
            64,
            "the box stays put while there is still work in it"
        );

        e.trim_layer();
        assert_eq!(
            e.model().allocated_cells(),
            24,
            "the trim gives the rest back"
        );
        assert_eq!(e.model().filled_count(), 24, "and loses no voxel doing it");
        assert!(e.is_dirty());
        assert_eq!(
            e.undo_depth(),
            1,
            "a trim moves no voxels, so it is no undo step"
        );
    }

    /// A declared box is where a layer is expected to live, and the status line
    /// says so — but it is not a wall.
    #[test]
    fn a_layer_can_be_declared_and_still_grows() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        e.add_named_layer("TREE", Bounds::new([20, 5, 10], [16, 32, 16]));
        assert_eq!(e.active_layer(), 1);
        assert_eq!(
            e.model().layer_bounds(1),
            Bounds::new([20, 5, 10], [16, 32, 16])
        );
        assert!(e.status().contains("16x32x16"), "{}", e.status());

        e.begin_stroke(hit([19, 5, 10], 0));
        e.end_stroke();
        assert!(e.model().layer_bounds(1).contains(19, 5, 10), "it grew");
    }

    #[test]
    fn framing_an_empty_model_still_produces_a_usable_camera() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        e.frame_model();
        assert!(e.camera.distance.is_finite() && e.camera.distance > 1.0);
    }
}
