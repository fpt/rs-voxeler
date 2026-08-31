//! Shooting a ray at the grid — how the editor knows which voxel you clicked.
//!
//! This is Amanatides–Woo grid traversal: clip the ray to the volume's box,
//! then walk cell to cell, always stepping the axis whose next boundary is
//! nearest. It visits exactly the cells the ray passes through, in order, so
//! the first solid one it finds *is* the nearest hit — no sorting, and no
//! dependence on how the model happens to be drawn.
//!
//! Picking against the grid rather than against the rendered polygons is the
//! one real departure from `voxeler`, which hit-tested last frame's projected
//! quads. Doing it in model space means the pick is exact at any resolution,
//! costs nothing per pixel, and — the part that matters for a build tool — hands
//! back the face that was hit, which is where a new voxel goes.

use crate::model::VoxelModel;

/// Which of a cube's six faces a ray entered through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Face {
    NegX,
    PosX,
    NegY,
    PosY,
    NegZ,
    PosZ,
}

impl Face {
    /// The outward unit normal, as integers — it is only ever used to step one
    /// cell, so a float normal would just be cast back at every call site.
    pub fn normal(self) -> [i32; 3] {
        match self {
            Face::NegX => [-1, 0, 0],
            Face::PosX => [1, 0, 0],
            Face::NegY => [0, -1, 0],
            Face::PosY => [0, 1, 0],
            Face::NegZ => [0, 0, -1],
            Face::PosZ => [0, 0, 1],
        }
    }

    /// Which axis this face is perpendicular to: 0=x, 1=y, 2=z.
    pub fn axis(self) -> usize {
        match self {
            Face::NegX | Face::PosX => 0,
            Face::NegY | Face::PosY => 1,
            Face::NegZ | Face::PosZ => 2,
        }
    }

    /// Whether it faces along the positive direction of its axis.
    pub fn is_positive(self) -> bool {
        matches!(self, Face::PosX | Face::PosY | Face::PosZ)
    }

    /// All six, in the order the mesh extractor walks them.
    pub const ALL: [Face; 6] = [
        Face::NegX,
        Face::PosX,
        Face::NegY,
        Face::PosY,
        Face::NegZ,
        Face::PosZ,
    ];

    /// The face on axis `axis` (0=x, 1=y, 2=z) facing `+` when `positive`.
    pub fn on_axis(axis: usize, positive: bool) -> Face {
        match (axis, positive) {
            (0, false) => Face::NegX,
            (0, true) => Face::PosX,
            (1, false) => Face::NegY,
            (1, true) => Face::PosY,
            (2, false) => Face::NegZ,
            _ => Face::PosZ,
        }
    }
}

/// Where a ray met a solid voxel.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RayHit {
    /// The solid cell. Erase and paint act here.
    pub voxel: [i32; 3],
    /// The face it was entered through.
    pub face: Face,
    /// Distance along the ray, in cells, where it crossed that face.
    pub distance: f32,
    /// Palette index of the cell that was hit.
    pub index: u8,
}

impl RayHit {
    /// The empty cell against that face — where a *new* voxel goes.
    ///
    /// It can be outside the volume (you clicked the outer face of a voxel on
    /// the boundary); `VoxelModel::set` treats that as a no-op, so the build
    /// tool needs no bounds check of its own.
    pub fn adjacent(&self) -> [i32; 3] {
        let n = self.face.normal();
        [
            self.voxel[0] + n[0],
            self.voxel[1] + n[1],
            self.voxel[2] + n[2],
        ]
    }
}

/// The first solid cell along `origin + t * dir`, `t >= 0`.
///
/// `dir` need not be normalized, but `distance` is reported in units of `dir`,
/// so normalize it if you want cells. `max_distance` bounds the walk.
pub fn cast(
    model: &VoxelModel,
    origin: [f32; 3],
    dir: [f32; 3],
    max_distance: f32,
) -> Option<RayHit> {
    let size = model.size();
    let bounds = [size[0] as f32, size[1] as f32, size[2] as f32];

    // Clip to the volume first. Without this a ray aimed past the model would
    // walk `max_distance` worth of empty cells before giving up, and one aimed
    // at a distant corner would spend most of its steps outside the grid.
    let (t_enter, entry_face) = clip_to_box(origin, dir, bounds)?;
    if t_enter > max_distance {
        return None;
    }

    // Nudge past the boundary so the starting cell is the one *inside* the box
    // rather than the one the plane belongs to, which floating point can put
    // either side of the edge.
    let t0 = t_enter + 1e-4;
    let mut cell = [0i32; 3];
    for a in 0..3 {
        let p = origin[a] + dir[a] * t0;
        cell[a] = (p.floor() as i32).clamp(0, size[a] as i32 - 1);
    }

    // Per-axis: which way we step, how far to the next boundary, and how far
    // between boundaries. A zero direction component never crosses a boundary
    // on that axis, so its `t_max` is infinity and it is never chosen.
    let mut step = [0i32; 3];
    let mut t_max = [f32::INFINITY; 3];
    let mut t_delta = [f32::INFINITY; 3];
    for a in 0..3 {
        if dir[a] > 0.0 {
            step[a] = 1;
            t_max[a] = ((cell[a] + 1) as f32 - origin[a]) / dir[a];
            t_delta[a] = 1.0 / dir[a];
        } else if dir[a] < 0.0 {
            step[a] = -1;
            t_max[a] = (cell[a] as f32 - origin[a]) / dir[a];
            t_delta[a] = -1.0 / dir[a];
        }
    }

    let mut face = entry_face;
    let mut distance = t_enter;
    loop {
        let index = model.get(cell[0], cell[1], cell[2]);
        if index != 0 {
            return Some(RayHit {
                voxel: cell,
                face,
                distance,
                index,
            });
        }

        // Step the axis whose boundary comes first.
        let a = if t_max[0] < t_max[1] && t_max[0] < t_max[2] {
            0
        } else if t_max[1] < t_max[2] {
            1
        } else {
            2
        };
        if t_max[a] > max_distance || !t_max[a].is_finite() {
            return None;
        }
        distance = t_max[a];
        cell[a] += step[a];
        // We entered the new cell through the face pointing back the way we
        // came: stepping +x enters through its -x face.
        face = Face::on_axis(a, step[a] < 0);
        t_max[a] += t_delta[a];
        if !model.contains(cell[0], cell[1], cell[2]) {
            return None;
        }
    }
}

/// Slab test against the box `[0, bounds]`. Returns the entry `t` (clamped to
/// 0 when the origin is already inside) and the face crossed to get in.
fn clip_to_box(origin: [f32; 3], dir: [f32; 3], bounds: [f32; 3]) -> Option<(f32, Face)> {
    let mut t_near = f32::NEG_INFINITY;
    let mut t_far = f32::INFINITY;
    let mut near_axis = 0usize;
    let mut near_positive = false;

    for a in 0..3 {
        if dir[a].abs() < 1e-9 {
            // Parallel to this pair of planes: either always inside the slab or
            // never, and no boundary crossing to record either way.
            if origin[a] < 0.0 || origin[a] > bounds[a] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dir[a];
        let mut t1 = (0.0 - origin[a]) * inv;
        let mut t2 = (bounds[a] - origin[a]) * inv;
        // `t1` is the near plane on this axis; for a negative direction the
        // roles of the two planes swap.
        let mut positive_face = dir[a] < 0.0;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
            positive_face = !positive_face;
        }
        if t1 > t_near {
            t_near = t1;
            near_axis = a;
            near_positive = positive_face;
        }
        t_far = t_far.min(t2);
        if t_near > t_far {
            return None;
        }
    }

    if t_far < 0.0 {
        return None; // the box is entirely behind the ray
    }
    if t_near < 0.0 {
        // Origin inside the box. There is no entry face; report the one facing
        // back along the ray's dominant axis so a hit on the very first cell
        // still gets a sane normal.
        let a = dominant_axis(dir);
        return Some((0.0, Face::on_axis(a, dir[a] < 0.0)));
    }
    Some((t_near, Face::on_axis(near_axis, near_positive)))
}

fn dominant_axis(dir: [f32; 3]) -> usize {
    let (mut best, mut axis) = (dir[0].abs(), 0);
    for (a, d) in dir.iter().enumerate().skip(1) {
        if d.abs() > best {
            best = d.abs();
            axis = a;
        }
    }
    axis
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(cells: &[[i32; 3]]) -> VoxelModel {
        let mut m = VoxelModel::new(8, 8, 8);
        for c in cells {
            m.set(c[0], c[1], c[2], 1);
        }
        m
    }

    #[test]
    fn hits_the_nearest_voxel_not_the_first_stored() {
        let m = model_with(&[[5, 0, 0], [2, 0, 0]]);
        let hit = cast(&m, [-4.0, 0.5, 0.5], [1.0, 0.0, 0.0], 100.0).unwrap();
        assert_eq!(hit.voxel, [2, 0, 0]);
        assert_eq!(hit.face, Face::NegX);
        assert_eq!(hit.adjacent(), [1, 0, 0]);
    }

    /// The face is what a build tool places against, so getting the *sign*
    /// right matters more than getting the cell right: a flipped normal buries
    /// the new voxel inside the model.
    #[test]
    fn the_face_faces_the_ray() {
        let m = model_with(&[[4, 4, 4]]);
        let cases = [
            ([-1.0f32, 0.0, 0.0], Face::PosX, [5, 4, 4]),
            ([1.0, 0.0, 0.0], Face::NegX, [3, 4, 4]),
            ([0.0, -1.0, 0.0], Face::PosY, [4, 5, 4]),
            ([0.0, 1.0, 0.0], Face::NegY, [4, 3, 4]),
            ([0.0, 0.0, -1.0], Face::PosZ, [4, 4, 5]),
            ([0.0, 0.0, 1.0], Face::NegZ, [4, 4, 3]),
        ];
        for (dir, face, adjacent) in cases {
            // Start well outside the volume, aimed back at the lone voxel.
            let origin = [
                4.5 - dir[0] * 20.0,
                4.5 - dir[1] * 20.0,
                4.5 - dir[2] * 20.0,
            ];
            let hit = cast(&m, origin, dir, 100.0).unwrap_or_else(|| panic!("missed {dir:?}"));
            assert_eq!(hit.voxel, [4, 4, 4], "dir {dir:?}");
            assert_eq!(hit.face, face, "dir {dir:?}");
            assert_eq!(hit.adjacent(), adjacent, "dir {dir:?}");
        }
    }

    #[test]
    fn a_ray_that_misses_the_volume_hits_nothing() {
        let m = model_with(&[[4, 4, 4]]);
        assert!(cast(&m, [-5.0, 100.0, 4.5], [1.0, 0.0, 0.0], 1000.0).is_none());
    }

    #[test]
    fn an_empty_model_is_a_miss_and_terminates() {
        let m = VoxelModel::new(8, 8, 8);
        assert!(cast(&m, [-5.0, 4.5, 4.5], [1.0, 0.0, 0.0], 1000.0).is_none());
    }

    /// The walk must stop at the far side of the volume rather than marching
    /// `max_distance` cells into empty space.
    #[test]
    fn a_diagonal_ray_crosses_the_volume_and_stops() {
        let m = VoxelModel::new(8, 8, 8);
        let d = 1.0 / 3f32.sqrt();
        assert!(cast(&m, [-2.0, -2.0, -2.0], [d, d, d], 1000.0).is_none());
    }

    #[test]
    fn max_distance_stops_the_walk_short() {
        let m = model_with(&[[7, 0, 0]]);
        assert!(cast(&m, [-4.0, 0.5, 0.5], [1.0, 0.0, 0.0], 5.0).is_none());
        assert!(cast(&m, [-4.0, 0.5, 0.5], [1.0, 0.0, 0.0], 100.0).is_some());
    }

    #[test]
    fn a_ray_starting_inside_a_solid_cell_hits_it_immediately() {
        let m = model_with(&[[4, 4, 4]]);
        let hit = cast(&m, [4.5, 4.5, 4.5], [1.0, 0.0, 0.0], 100.0).unwrap();
        assert_eq!(hit.voxel, [4, 4, 4]);
        assert_eq!(hit.distance, 0.0);
    }

    /// A ray parallel to two axes must not be excluded by the slab test just
    /// because its direction has exact zeros in it.
    #[test]
    fn axis_aligned_rays_still_clip() {
        let m = model_with(&[[0, 0, 0]]);
        assert!(cast(&m, [0.5, 0.5, -3.0], [0.0, 0.0, 1.0], 100.0).is_some());
        assert!(cast(&m, [9.5, 0.5, -3.0], [0.0, 0.0, 1.0], 100.0).is_none());
    }

    #[test]
    fn a_voxel_behind_the_camera_is_not_hit() {
        let m = model_with(&[[4, 4, 4]]);
        assert!(cast(&m, [4.5, 4.5, 20.0], [0.0, 0.0, 1.0], 100.0).is_none());
    }
}
