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
use voxel_render::mesh::{extract, ExtractOptions, FaceMesh};
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
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Build => "BUILD",
            Tool::Erase => "ERASE",
            Tool::Paint => "PAINT",
            Tool::Pick => "PICK",
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
            camera: OrbitCamera::default(),
            tool: Tool::Build,
            span: Span::default(),
            brush: Brush::default(),
            color: 1,
            mirror: [false; 3],
            show_grid: true,
            show_help: false,
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

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn status(&self) -> &str {
        &self.status
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
            self.mesh = extract(
                &self.model,
                ExtractOptions {
                    y_limit: self.slice.unwrap_or(u16::MAX),
                },
            );
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
        let origin = [origin.x - offset.x, origin.y - offset.y, origin.z - offset.z];
        let dir = [dir.x, dir.y, dir.z];

        // While a stroke is in progress the ray must not see what that stroke
        // has done, or a drag re-aims at its own work — see `Drag::before`.
        let empty = std::collections::HashMap::new();
        let before = self.drag.as_ref().map_or(&empty, |d| &d.before);
        let mask = |cell: [i32; 3]| before.get(&cell).copied();
        let hit = voxel_core::raycast::cast_masked(&self.model, origin, dir, self.camera.far, &mask);
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
            Tool::Erase | Tool::Paint | Tool::Pick => hit.voxel,
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
            Tool::Build => "build",
            Tool::Erase => "erase",
            Tool::Paint => "paint",
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

    /// Apply the tool again, part-way through a drag.
    pub fn continue_stroke(&mut self, target: Target) {
        let Some(drag) = &self.drag else { return };
        if drag.last_cell == Some(target.cell) {
            return;
        }
        // A region span is a click, not a stroke. It already reached everything
        // connected to the cell it was aimed at, so re-running it as the pointer
        // moves would re-flood from a new seed several times a frame and turn
        // one intended fill into a wandering pile of them.
        if self.span != Span::Voxel && drag.last_cell.is_some() {
            return;
        }
        if let Some((axis, coord)) = drag.plane {
            if target.cell[axis] != coord {
                return;
            }
        }
        let value = match self.tool {
            Tool::Build | Tool::Paint => self.color,
            Tool::Erase => 0,
            Tool::Pick => return,
        };

        // Resolved before the stroke is borrowed: the span reads the model, and
        // writing through the stroke needs it mutably.
        let cells = self.write_cells(target);
        // Build fills air and never repaints; erase and paint act on material
        // and never create it. With a single cell that rule is already true by
        // construction, but a brush covers cells the ray never touched and a
        // reflection lands wherever the model happens to be, so it has to be
        // stated rather than assumed.
        //
        // Asked of the *active layer*, not of what is on screen: that is the
        // grid being written, and testing the composite would refuse to build
        // under a voxel a higher layer is showing — which is exactly the thing
        // a lower layer is for.
        let wants_solid = self.tool != Tool::Build;
        let layer = self.model.active_layer();
        let Some(drag) = &mut self.drag else { return };
        drag.last_cell = Some(target.cell);
        for [x, y, z] in cells {
            if (self.model.get_in(layer, x, y, z) != 0) != wants_solid {
                continue;
            }
            // The composite, not the layer: what the *ray* would have found
            // here before this stroke ran.
            let was = self.model.get(x, y, z);
            drag.before.entry([x, y, z]).or_insert(was);
            drag.stroke.set(&mut self.model, x, y, z, value);
        }
        self.invalidate_mesh();
    }

    /// Every cell this application of the tool may write to: the span around
    /// the target, and each of its reflections.
    fn write_cells(&self, target: Target) -> Vec<[i32; 3]> {
        let reach = region::Reach {
            seed: target.cell,
            face: target.face,
            // Building grows over air; erasing and painting grow over the
            // colour under the cursor, so a region stops where the colour does.
            matches: if self.tool == Tool::Build { 0 } else { target.index },
            brush: self.brush,
            grounded: target.is_ground(),
            // An edit must not reach a layer the slice has taken off screen.
            y_limit: self.slice.unwrap_or(u16::MAX),
            // The composite: a click selects what the user can see, and they
            // can see which layers are stacked and undo if it was not what
            // they meant. See `write_cells`.
            layer: None,
        };
        // The region is grown over the *composite*: you point at what you can
        // see, so that is what a fill selects. What it writes is then narrowed
        // to the active layer by the rule above. With one layer, or while
        // working on the layer you are looking at, the two are the same set.
        let cells = region::cells(&self.model, self.span, reach);
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
        match self.history.undo(&mut self.model) {
            Some(label) => {
                self.dirty = true;
                self.invalidate_mesh();
                self.status = format!("undo {label}");
            }
            None => self.status = "nothing to undo".into(),
        }
    }

    pub fn redo(&mut self) {
        match self.history.redo(&mut self.model) {
            Some(label) => {
                self.dirty = true;
                self.invalidate_mesh();
                self.status = format!("redo {label}");
            }
            None => self.status = "nothing to redo".into(),
        }
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
        let changed = self.history.edit(&mut self.model, "clear", |model, stroke| {
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
        self.model.set_active_layer(i);
        self.report_layer();
    }

    /// Step the active layer, clamped rather than wrapped.
    ///
    /// Wrapping would put the top of the stack one key away from the bottom,
    /// and a stack is a thing with ends — running off one and finding yourself
    /// at the other is how an edit lands on the wrong layer.
    pub fn cycle_layer(&mut self, delta: i32) {
        let n = self.model.layer_count() as i32;
        let next = (self.model.active_layer() as i32 + delta).clamp(0, n - 1);
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
        let changed = self.history.restructure(&mut self.model, "add layer", |model| {
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
        let changed = self.history.restructure(&mut self.model, "delete layer", |model| {
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
        let changed = self.history.restructure(&mut self.model, "move layer", |model| {
            model.move_layer(model.active_layer(), up);
        });
        if changed {
            self.after_structural();
            self.report_layer();
        } else {
            self.status = if up { "already on top" } else { "already at the bottom" }.into();
        }
    }

    pub fn merge_layer_down(&mut self) {
        let name = self.layer_name(self.model.active_layer());
        let changed = self.history.restructure(&mut self.model, "merge layer", |model| {
            model.merge_down(model.active_layer());
        });
        if changed {
            self.after_structural();
            self.status = format!("merged {name} down");
        } else {
            self.status = "the bottom layer has nothing to merge into".into();
        }
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
    pub fn apply_batch(
        &mut self,
        label: &'static str,
        color: u8,
        cells: impl FnOnce(&VoxelModel, usize) -> Vec<[i32; 3]>,
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
        }
        self.invalidate_mesh();
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
        self.invalidate_mesh();
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
        let Some(name) = self.rename.take() else { return };
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
                format!("{} brush {e}x{e}x{e}", self.brush.shape.name().to_lowercase())
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
    pub fn nudge_brush(&mut self, delta: i32) {
        self.brush.radius = (self.brush.radius as i32 + delta).clamp(0, MAX_BRUSH as i32) as u8;
        self.span = Span::Voxel;
        let e = self.brush.edge();
        self.status = format!("{} brush {e}x{e}x{e}", self.brush.shape.name().to_lowercase());
    }

    pub fn toggle_brush_shape(&mut self) {
        self.brush.shape = match self.brush.shape {
            BrushShape::Cube => BrushShape::Sphere,
            BrushShape::Sphere => BrushShape::Cube,
        };
        self.span = Span::Voxel;
        let e = self.brush.edge();
        self.status = format!("{} brush {e}x{e}x{e}", self.brush.shape.name().to_lowercase());
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
            if self.viewing { "VIEWING" } else { self.tool.name() },
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
        s.push_str(&format!("  UNDO {}/{}", self.undo_depth(), self.redo_depth()));
        s
    }
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

            let Some(t) = e.target_at(160.0, 120.0, 320, 240) else { continue };
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

        let t = e.target_at(160.0, 120.0, 320, 240).expect("the work plane is a target");
        assert!(t.is_ground());
        assert_eq!(t.cell[1], e.ground_y(), "a ground build lands on the work plane");
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

        let t = e.target_at(160.0, 120.0, 320, 240).expect("the plane has an underside");
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
            e.target_at(160.0, 120.0, 320, 240).is_some_and(|t| t.is_ground()),
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
        let t = e.target_at(160.0, 120.0, 320, 240).expect("a layer with nothing in it");
        assert!(t.is_ground());
        e.begin_stroke(t);
        e.end_stroke();
        assert_eq!(e.model().get_in(1, t.cell[0], t.cell[1], t.cell[2]), e.color);
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
        let t = e.target_at(160.0, 120.0, 320, 240).expect("the floor is still there");
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
        assert!(placed.len() > 1, "the drag should have placed several voxels");
        assert!(placed.iter().all(|y| *y == 1), "climbed off the plane: {placed:?}");
        assert_eq!(e.undo_depth(), 1, "a drag is one undo step");
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
        assert!(full.voxel[1] >= 3, "the unsliced pick must be above the cut");

        e.set_slice(Some(3));
        assert!(e.mesh().quads.iter().all(|q| q.voxel[1] < 3));
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

        assert_eq!(e.model().filled_count(), 128, "a second layer over the floor");
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
        assert_eq!(e.model().get(3, 0, 3), 4, "the floor under the brush is not repainted");
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
    fn resizing_the_brush_clamps_and_selects_the_span_it_belongs_to() {
        let mut e = editor_with_floor();
        e.span = Span::Volume;
        e.nudge_brush(1);
        assert_eq!(e.brush.radius, 1);
        assert_eq!(e.brush.edge(), 3);
        assert_eq!(e.span, Span::Voxel, "the brush is the voxel span's shape");

        e.nudge_brush(-5);
        assert_eq!(e.brush.radius, 0, "a brush never shrinks past one cell");
        for _ in 0..20 {
            e.nudge_brush(1);
        }
        assert_eq!(e.brush.radius, MAX_BRUSH);
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
        assert!(e.mesh().quads.is_empty(), "a hidden layer draws nothing");
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

        assert_eq!(e.model().filled_count(), 128, "the whole floor, seen from above");
        assert_eq!(e.model().layers()[1].filled_count(), 64, "all of it on the new layer");
        assert_eq!(e.model().layers()[0].filled_count(), 64, "and none of it on the old");
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
".chars() {
            e.rename_push(c);
        }
        assert_eq!(e.renaming(), Some("body"), "a control character is not a name");
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
        assert!(!e.model().layers()[1].visible, "hidden, and still holding its work");
        assert_eq!(e.model().get_in(1, 3, 1, 3), 6);
        assert_eq!(e.active_layer(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The point of per-layer boxes, from the editor: a scene you can build
    /// across costs only what is drawn in it.
    #[test]
    fn a_layer_grows_to_what_is_drawn_and_costs_only_that() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        assert_eq!(e.model().allocated_cells(), 0, "an empty scene allocates nothing");

        e.begin_stroke(hit([20, 5, 10], 0));
        e.end_stroke();
        assert_eq!(e.model().allocated_cells(), 1);
        assert!(e.model().layer_bounds(0).contains(20, 5, 10));

        e.span = Span::Voxel;
        e.brush = Brush { radius: 2, shape: BrushShape::Cube };
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

        e.tool = Tool::Erase;
        e.span = Span::Plane;
        e.begin_stroke(hit([3, 0, 3], 4));
        e.end_stroke();
        assert_eq!(e.model().filled_count(), 0);
        assert_eq!(e.model().allocated_cells(), 64, "the box stays put");

        e.trim_layer();
        assert_eq!(e.model().allocated_cells(), 0);
        assert!(e.is_dirty());
        assert_eq!(e.undo_depth(), 1, "a trim moves no voxels, so it is no undo step");
    }

    /// A declared box is where a layer is expected to live, and the status line
    /// says so — but it is not a wall.
    #[test]
    fn a_layer_can_be_declared_and_still_grows() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        e.add_named_layer("TREE", Bounds::new([20, 5, 10], [16, 32, 16]));
        assert_eq!(e.active_layer(), 1);
        assert_eq!(e.model().layer_bounds(1), Bounds::new([20, 5, 10], [16, 32, 16]));
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
