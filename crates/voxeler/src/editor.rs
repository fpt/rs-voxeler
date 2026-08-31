//! The editor's state and every operation on it — everything except the window.
//!
//! Nothing here knows about `winit`. That is deliberate: the interesting
//! behaviour is which voxel a click lands on, whether a drag stays on its
//! plane, and what mirroring does, and all of it is testable by calling these
//! methods with a camera and a pixel coordinate. The window layer's job shrinks
//! to translating events into these calls.

use std::path::{Path, PathBuf};

use voxel_core::{format, Face, History, RayHit, Stroke, VoxelModel};
use voxel_render::mesh::{extract, ExtractOptions, FaceMesh};
use voxel_render::{Mat4, OrbitCamera, Vec3};

/// The default edge length for a new model. The plan's 32³ is enough for a
/// sword; 64³ is where a character with a face on it starts to work, and the
/// renderer handles a full 64³ shell without trouble.
pub const DEFAULT_SIZE: u16 = 64;

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
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub hit: RayHit,
    /// The cell the active tool would write to.
    pub cell: [i32; 3],
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
}

pub struct Editor {
    model: VoxelModel,
    history: History,
    mesh: FaceMesh,
    mesh_dirty: bool,

    pub camera: OrbitCamera,
    pub tool: Tool,
    pub color: u8,
    /// Mirror every edit across the middle of the X axis.
    pub mirror_x: bool,
    pub show_grid: bool,
    pub show_help: bool,
    /// Hide everything at or above this Y. `None` shows the whole model.
    pub slice: Option<u16>,

    path: PathBuf,
    dirty: bool,
    status: String,
    drag: Option<Drag>,
}

impl Editor {
    pub fn new(model: VoxelModel, path: PathBuf) -> Self {
        let mut editor = Self {
            camera: OrbitCamera::default(),
            tool: Tool::Build,
            color: 1,
            mirror_x: false,
            show_grid: true,
            show_help: false,
            slice: None,
            mesh: FaceMesh::default(),
            mesh_dirty: true,
            history: History::default(),
            dirty: false,
            status: format!("{}", path.display()),
            path,
            model,
            drag: None,
        };
        editor.frame_model();
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

    /// What the tool would act on for a ray through a pixel, or `None` when the
    /// ray misses the model.
    pub fn target_at(&self, px: f32, py: f32, width: u32, height: u32) -> Option<Target> {
        let (origin, dir) = self.camera.ray(px, py, width, height);
        let offset = self.offset();
        // The raycaster works in the grid's own coordinates, so the ray is
        // moved into model space rather than the model into world space.
        let origin = [origin.x - offset.x, origin.y - offset.y, origin.z - offset.z];
        let mut hit = voxel_core::raycast::cast(
            &self.model,
            origin,
            [dir.x, dir.y, dir.z],
            self.camera.far,
        )?;
        // A slice hides the layers above the cut, and a ray must not pick a
        // voxel that is not on screen. Re-cast from just under the cut instead,
        // so clicking through the opening reaches the cross-section.
        if let Some(limit) = self.slice {
            if hit.voxel[1] >= limit as i32 {
                hit = self.cast_below_slice(origin, [dir.x, dir.y, dir.z], limit)?;
            }
        }
        let cell = match self.tool {
            Tool::Build => hit.adjacent(),
            Tool::Erase | Tool::Paint | Tool::Pick => hit.voxel,
        };
        Some(Target { hit, cell })
    }

    /// Re-cast against a copy of the model with the hidden layers removed.
    ///
    /// Building a temporary model is the honest way to do this: the alternative
    /// is a second traversal that skips cells by height, which duplicates the
    /// traversal logic for one caller. A slice click is a user action at human
    /// speed, so the copy costs nothing anyone can perceive.
    fn cast_below_slice(&self, origin: [f32; 3], dir: [f32; 3], limit: u16) -> Option<RayHit> {
        let mut sliced = self.model.clone();
        let [sx, sy, sz] = sliced.size();
        for y in limit..sy {
            for z in 0..sz {
                for x in 0..sx {
                    sliced.set(x as i32, y as i32, z as i32, 0);
                }
            }
        }
        voxel_core::raycast::cast(&sliced, origin, dir, self.camera.far)
    }

    // -- editing ---------------------------------------------------------

    /// Begin a stroke and apply the tool once.
    pub fn begin_stroke(&mut self, target: Target) {
        let label = match self.tool {
            Tool::Build => "build",
            Tool::Erase => "erase",
            Tool::Paint => "paint",
            Tool::Pick => {
                self.color = target.hit.index;
                self.status = format!("picked colour {}", self.color);
                return;
            }
        };
        self.drag = Some(Drag {
            stroke: Stroke::new(label),
            // Build is the only tool that grows the surface it is aimed at, so
            // it is the only one that needs pinning; erase and paint follow the
            // pointer over whatever it is actually over.
            plane: (self.tool == Tool::Build)
                .then(|| (target.hit.face.axis(), target.cell[target.hit.face.axis()])),
            last_cell: None,
        });
        self.continue_stroke(target);
    }

    /// Apply the tool again, part-way through a drag.
    pub fn continue_stroke(&mut self, target: Target) {
        let Some(drag) = &self.drag else { return };
        if drag.last_cell == Some(target.cell) {
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

        let [x, y, z] = target.cell;
        let mirror = self.mirror_x.then(|| self.model.size()[0] as i32 - 1 - x);
        let Some(drag) = &mut self.drag else { return };
        drag.last_cell = Some(target.cell);
        drag.stroke.set(&mut self.model, x, y, z, value);
        if let Some(mx) = mirror {
            drag.stroke.set(&mut self.model, mx, y, z, value);
        }
        self.invalidate_mesh();
    }

    /// Finish the drag, committing it as one undo step.
    pub fn end_stroke(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        let label = if self.history.push(drag.stroke) {
            self.dirty = true;
            Some(self.tool.name())
        } else {
            None
        };
        if let Some(label) = label {
            self.status = format!("{} ({} voxels)", label, self.model.filled_count());
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
    pub fn clear(&mut self) {
        let filled: Vec<_> = self.model.iter_filled().map(|(p, _)| p).collect();
        let changed = self.history.edit(&mut self.model, "clear", |model, stroke| {
            for [x, y, z] in filled {
                stroke.set(model, x as i32, y as i32, z as i32, 0);
            }
        });
        if changed {
            self.dirty = true;
            self.invalidate_mesh();
        }
        self.status = "cleared".into();
    }

    // -- view ------------------------------------------------------------

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
        self.frame_model();
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
    pub fn export_vox(&mut self) {
        let path = self.path.with_extension("vox");
        self.save_as(&path);
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
                self.frame_model();
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
            self.tool.name(),
        );
        if self.mirror_x {
            s.push_str("  MIRROR");
        }
        if let Some(cut) = self.slice {
            s.push_str(&format!("  SLICE {cut}"));
        }
        // Whether undo has anywhere to go is worth a glance before a big edit.
        s.push_str(&format!("  UNDO {}/{}", self.undo_depth(), self.redo_depth()));
        s
    }
}

/// Load `path`, or start an empty `size`³ model if it does not exist yet.
///
/// A missing file is not an error: `voxeler robot.vxm` on a fresh directory is
/// how you start a model, and refusing would mean the tool could only ever open
/// something another tool had made.
pub fn open_or_create(path: &Path, size: u16) -> Result<VoxelModel, String> {
    if !path.exists() {
        return Ok(VoxelModel::new(size, size, size));
    }
    format::load(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The face a target highlight should outline, as four world-space corners.
pub fn face_corners(voxel: [i32; 3], face: Face, offset: Vec3) -> [Vec3; 4] {
    let quad = voxel_render::mesh::FaceQuad {
        voxel: [voxel[0] as u16, voxel[1] as u16, voxel[2] as u16],
        face,
        index: 1,
    };
    let c = quad.corners();
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
        assert_eq!(t.hit.face, Face::PosY);
        assert_eq!(t.cell, [t.hit.voxel[0], 1, t.hit.voxel[2]]);
    }

    #[test]
    fn erase_and_paint_target_the_voxel_that_was_hit() {
        let mut e = editor_with_floor();
        for tool in [Tool::Erase, Tool::Paint, Tool::Pick] {
            e.tool = tool;
            let t = e.target_at(160.0, 120.0, 320, 240).unwrap();
            assert_eq!(t.cell, t.hit.voxel, "{tool:?}");
        }
    }

    #[test]
    fn a_ray_into_empty_space_has_no_target() {
        let e = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        assert!(e.target_at(0.0, 0.0, 320, 240).is_none());
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
        e.mirror_x = true;
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
        e.mirror_x = true;

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

    /// A slice must hide layers *and* make them unclickable — picking a voxel
    /// you cannot see is the bug this guards.
    #[test]
    fn a_slice_hides_layers_from_both_the_mesh_and_the_pick() {
        let mut model = VoxelModel::new(8, 8, 8);
        for y in 0..8 {
            model.set(4, y, 4, 1);
        }
        let mut e = Editor::new(model, PathBuf::from("t.vxm"));
        e.camera.yaw = 0.0;
        e.camera.pitch = 1.4; // looking down the column
        e.tool = Tool::Erase;

        let full = e.target_at(160.0, 120.0, 320, 240).unwrap();
        assert!(full.hit.voxel[1] >= 3, "the unsliced pick must be above the cut");

        e.set_slice(Some(3));
        assert!(e.mesh().quads.iter().all(|q| q.voxel[1] < 3));
        let sliced = e.target_at(160.0, 120.0, 320, 240).unwrap();
        assert_eq!(sliced.hit.voxel[1], 2, "should pick the top of the slice");
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
    fn a_missing_file_starts_an_empty_model_of_the_requested_size() {
        let dir = std::env::temp_dir().join("voxeler-open-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = open_or_create(&dir.join("new.vxm"), 32).unwrap();
        assert_eq!(m.size(), [32, 32, 32]);
        assert_eq!(m.filled_count(), 0);
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

    #[test]
    fn framing_an_empty_model_still_produces_a_usable_camera() {
        let mut e = Editor::new(VoxelModel::new(64, 64, 64), PathBuf::from("t.vxm"));
        e.frame_model();
        assert!(e.camera.distance.is_finite() && e.camera.distance > 1.0);
    }
}
