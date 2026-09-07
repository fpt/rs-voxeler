//! How far one edit reaches — the cells a tool acts on, given the cell it was
//! aimed at.
//!
//! The editor's tools answer *what* an edit does: add, remove, recolour. This
//! module answers *where*, and the two are independent. That is why a span is
//! not a tool: "fill this face" and "recolour this face" are the same set of
//! cells reached two different ways, and writing them as separate tools would
//! be the same flood fill copied once per verb.
//!
//! # A region is bounded by colour
//!
//! [`Span::Plane`] and [`Span::Volume`] grow over cells holding the same
//! palette index as the cell under the cursor. Building starts on air, so it
//! floods air; erasing and painting start on a voxel, so they stop where the
//! colour changes. On a single-colour model that is exactly "every connected
//! voxel"; on a model with a red panel on a blue body it is the panel, which is
//! the thing you were pointing at.
//!
//! # Nothing reaches what the slice hides
//!
//! A slice hides layers from the mesh and from the pick, and a span has to
//! agree with both or an edit silently changes voxels that are not on screen.
//! Hidden layers are excluded from every region — and they read as *air* to the
//! neighbour tests, so the top of a cross-section is a face a region can grow
//! along, exactly as it is a face you can click.

use crate::model::VoxelModel;
use crate::raycast::Face;

/// The largest brush radius, giving a 17³ cube.
///
/// Not a limit of the algorithm — it is where a brush stops being a brush. Half
/// the default volume's edge is already a stamp rather than a stroke, and the
/// slider has to stop somewhere the HUD can still label.
pub const MAX_BRUSH: u8 = 8;

/// How far an edit spreads from the cell under the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Span {
    /// That cell alone, or the [`Brush`] around it.
    #[default]
    Voxel,
    /// The run through it along the axis of the face that was clicked.
    Axis,
    /// The connected region of that run's layer that shows the same face.
    Plane,
    /// The connected region in three dimensions.
    Volume,
}

impl Span {
    pub fn name(self) -> &'static str {
        match self {
            Span::Voxel => "VOXEL",
            Span::Axis => "AXIS",
            Span::Plane => "PLANE",
            Span::Volume => "VOLUME",
        }
    }

    /// All four, in order of increasing reach — the order the keys `1`–`4` and
    /// the HUD row both use.
    pub const ALL: [Span; 4] = [Span::Voxel, Span::Axis, Span::Plane, Span::Volume];
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BrushShape {
    #[default]
    Cube,
    Sphere,
}

impl BrushShape {
    pub fn name(self) -> &'static str {
        match self {
            BrushShape::Cube => "CUBE",
            BrushShape::Sphere => "BALL",
        }
    }
}

/// A solid stamped around one cell: the volumetric form of a single-cell edit.
///
/// Sized by radius rather than by edge, so a brush is always odd-sized and
/// always has a centre cell. An even edge would have to round its centre to one
/// side, and the side it rounded to would show up as a half-voxel drift every
/// time the brush was resized mid-model.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Brush {
    pub radius: u8,
    pub shape: BrushShape,
}

impl Brush {
    /// The brush's edge in voxels: radius 0 is one cell, radius 1 is 3×3×3.
    pub fn edge(self) -> u32 {
        self.radius as u32 * 2 + 1
    }

    /// Whether a cell that far from the centre is inside the brush.
    fn covers(self, dx: i32, dy: i32, dz: i32) -> bool {
        match self.shape {
            BrushShape::Cube => true,
            // Measured to the far side of the centre cell rather than to its
            // middle. Comparing against `r²` gives a plus sign at radius 1 and
            // a lumpy cross at radius 2 — the sizes a voxel brush is actually
            // used at — where `(r + ½)²` keeps the faces and edges and drops
            // only the corners, which is what reads as a ball.
            BrushShape::Sphere => {
                let r = self.radius as f32 + 0.5;
                (dx * dx + dy * dy + dz * dz) as f32 <= r * r
            }
        }
    }
}

/// Everything a span needs to know about the click that started it.
#[derive(Clone, Copy, Debug)]
pub struct Reach {
    /// The cell the tool would write to first — the region grows from here.
    pub seed: [i32; 3],
    /// The face that was clicked. Its axis orients [`Span::Axis`] and picks the
    /// layer [`Span::Plane`] stays in.
    pub face: Face,
    /// The palette index a cell must hold to join the region: 0 when building,
    /// because a build grows over air, and the colour under the cursor when
    /// erasing or painting.
    pub matches: u8,
    /// Only consulted by [`Span::Voxel`].
    pub brush: Brush,
    /// Whether the face is the editor's work plane rather than a surface of the
    /// model. There is no material behind the work plane, so a plane span over
    /// it covers the whole layer instead of the part backing onto something —
    /// without which clicking bare plane would fill exactly one cell.
    pub grounded: bool,
    /// Layers at or above this Y are hidden by a slice. `u16::MAX` when the
    /// model is whole.
    pub y_limit: u16,
    /// Which grid the region is read from: `None` for the composite — what is
    /// on screen, which is what a click selects — or `Some(layer)` for that
    /// layer's own grid.
    ///
    /// A caller that writes to one layer and grows its region on the composite
    /// can pick up cells that layer does not own, and writing those makes a
    /// copy of another layer's shape rather than changing it. A click is
    /// allowed that, because the user can see both layers and undo. A caller
    /// naming coordinates sight-unseen should ask the layer it is writing to.
    pub layer: Option<usize>,
}

/// Every cell the span covers, starting with the seed.
///
/// The cells are candidates, not writes: whether a given cell is actually
/// touched is the tool's rule (a build only fills air, an erase only clears
/// material), and applying it here would mean this module knowing what a tool
/// is. The one thing that *is* guaranteed is that every cell is inside the
/// volume and outside the slice.
pub fn cells(model: &VoxelModel, span: Span, reach: Reach) -> Vec<[i32; 3]> {
    match span {
        Span::Voxel => brush(model, reach),
        Span::Axis => axis_run(model, reach),
        Span::Plane => flood(model, reach, true),
        Span::Volume => flood(model, reach, false),
    }
}

/// The index at a cell, on whichever grid this reach reads.
fn at(model: &VoxelModel, reach: &Reach, p: [i32; 3]) -> u8 {
    match reach.layer {
        Some(l) => model.get_in(l, p[0], p[1], p[2]),
        None => model.get(p[0], p[1], p[2]),
    }
}

/// The same, as the *screen* sees it: a layer a slice hides reads as air, so a
/// cross-section's top is a face like any other.
fn visible(model: &VoxelModel, reach: &Reach, p: [i32; 3]) -> u8 {
    if p[1] >= reach.y_limit as i32 {
        0
    } else {
        at(model, reach, p)
    }
}

/// Whether a cell can be part of the region at all: inside the volume, below
/// the cut, and holding the index the region is made of.
///
/// Note that this asks `model.get`, not [`visible`] — a hidden layer is not air
/// that a build may flood into, it is a layer the edit must not reach.
fn joins(model: &VoxelModel, reach: &Reach, p: [i32; 3]) -> bool {
    model.contains(p[0], p[1], p[2])
        && p[1] < reach.y_limit as i32
        && at(model, reach, p) == reach.matches
}

/// Whether `p` presents the same face the click landed on.
///
/// The two cases are not one test, because the material sits on opposite sides
/// of the region in them. Building grows over *air*, and what has to be solid
/// is the cell behind it — the thing the new voxel rests against — or the
/// region would spread across the whole empty layer rather than across the face
/// you clicked. Erasing and painting grow over the material itself, so what has
/// to be clear is the cell in front: the region is the visible surface, not
/// every voxel that happens to share the layer.
fn on_face(model: &VoxelModel, reach: &Reach, p: [i32; 3]) -> bool {
    let n = reach.face.normal();
    if reach.matches == 0 {
        reach.grounded || visible(model, reach, [p[0] - n[0], p[1] - n[1], p[2] - n[2]]) != 0
    } else {
        visible(model, reach, [p[0] + n[0], p[1] + n[1], p[2] + n[2]]) == 0
    }
}

fn brush(model: &VoxelModel, reach: Reach) -> Vec<[i32; 3]> {
    let r = reach.brush.radius as i32;
    let [sx, sy, sz] = reach.seed;
    let mut out = Vec::new();
    for dz in -r..=r {
        for dy in -r..=r {
            for dx in -r..=r {
                if !reach.brush.covers(dx, dy, dz) {
                    continue;
                }
                let p = [sx + dx, sy + dy, sz + dz];
                // No colour test: a brush is a shape, and which of the cells
                // under it change is the tool's business. The bounds and the
                // slice still apply — those are not negotiable for any edit.
                if model.contains(p[0], p[1], p[2]) && p[1] < reach.y_limit as i32 {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// The uninterrupted run through the seed along the clicked face's axis.
///
/// Both directions, not just outward. The face only says which axis the run
/// follows; where it stops is where the material changes, and a build aimed at
/// the top of a floor runs up to the ceiling because that is where the air
/// ends — no rule about the normal's sign is needed to get there.
fn axis_run(model: &VoxelModel, reach: Reach) -> Vec<[i32; 3]> {
    let axis = reach.face.axis();
    let mut out = Vec::new();
    if !joins(model, &reach, reach.seed) {
        return out;
    }
    out.push(reach.seed);
    for dir in [-1, 1] {
        let mut p = reach.seed;
        loop {
            p[axis] += dir;
            if !joins(model, &reach, p) {
                break;
            }
            out.push(p);
        }
    }
    out
}

/// Flood fill from the seed, over the whole volume or confined to one layer.
///
/// Four- or six-connected, never diagonal: two voxels touching only at a corner
/// share no face, and a region that leaked through corners would cross a
/// one-voxel-thick diagonal wall that the eye reads as solid.
fn flood(model: &VoxelModel, reach: Reach, planar: bool) -> Vec<[i32; 3]> {
    let axis = reach.face.axis();
    let [sx, sy, sz] = model.size();
    let stride = (sx as usize, sx as usize * sy as usize);
    // A flat bitmap rather than a set: the grid is dense and small, so one byte
    // per cell is 32 KiB at the default size and every lookup is an index.
    let mut seen = vec![false; sx as usize * sy as usize * sz as usize];

    let mut out = Vec::new();
    let mut stack = vec![reach.seed];
    while let Some(p) = stack.pop() {
        if !model.contains(p[0], p[1], p[2]) {
            continue;
        }
        let i = p[0] as usize + p[1] as usize * stride.0 + p[2] as usize * stride.1;
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        if !joins(model, &reach, p) || (planar && !on_face(model, &reach, p)) {
            continue;
        }
        out.push(p);
        for a in 0..3 {
            if planar && a == axis {
                continue;
            }
            for d in [-1, 1] {
                let mut q = p;
                q[a] += d;
                stack.push(q);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8³ volume with a solid floor at y = 0, half of it recoloured.
    fn floor() -> VoxelModel {
        let mut m = VoxelModel::new(8, 8, 8);
        for z in 0..8 {
            for x in 0..8 {
                m.set(x, 0, z, if x < 4 { 1 } else { 2 });
            }
        }
        m
    }

    fn reach(seed: [i32; 3], face: Face, matches: u8) -> Reach {
        Reach {
            seed,
            face,
            matches,
            brush: Brush::default(),
            grounded: false,
            y_limit: u16::MAX,
            layer: None,
        }
    }

    fn sorted(mut v: Vec<[i32; 3]>) -> Vec<[i32; 3]> {
        v.sort();
        v
    }

    #[test]
    fn the_default_span_is_the_single_cell_it_was_aimed_at() {
        let m = floor();
        let r = reach([3, 1, 3], Face::PosY, 0);
        assert_eq!(cells(&m, Span::Voxel, r), vec![[3, 1, 3]]);
    }

    /// A build runs along the air, an erase along the material — from the same
    /// rule, not from two.
    #[test]
    fn an_axis_run_stops_where_the_material_changes() {
        let mut m = VoxelModel::new(8, 8, 8);
        for y in 0..5 {
            m.set(2, y, 2, 1);
        }

        // Building on top of the column: the air above it, up to the ceiling.
        let up = cells(&m, Span::Axis, reach([2, 5, 2], Face::PosY, 0));
        assert_eq!(sorted(up), vec![[2, 5, 2], [2, 6, 2], [2, 7, 2]]);

        // Erasing the same face: the column itself, all the way down.
        let down = cells(&m, Span::Axis, reach([2, 4, 2], Face::PosY, 1));
        assert_eq!(down.len(), 5);
        assert!(down.iter().all(|p| p[0] == 2 && p[2] == 2));
    }

    /// The run follows the clicked face's axis, not gravity.
    #[test]
    fn an_axis_run_follows_the_face_it_was_given() {
        let m = floor();
        let along_x = cells(&m, Span::Axis, reach([4, 0, 3], Face::PosX, 2));
        // Colour 2 occupies x = 4..8 of the floor, and the run stops at the
        // colour boundary rather than running the whole width.
        assert_eq!(sorted(along_x), vec![[4, 0, 3], [5, 0, 3], [6, 0, 3], [7, 0, 3]]);
    }

    /// The bounded-by-colour rule, which is the whole reason a region is not
    /// just "every connected voxel".
    #[test]
    fn a_region_stops_at_a_colour_boundary() {
        let m = floor();
        let red = cells(&m, Span::Plane, reach([1, 0, 1], Face::PosY, 1));
        assert_eq!(red.len(), 4 * 8, "half the floor, not all of it");
        assert!(red.iter().all(|p| p[0] < 4));
    }

    /// A plane region is the *surface*, not the layer: a voxel with something
    /// on top of it does not show the face that was clicked.
    #[test]
    fn a_plane_region_covers_only_what_shows_the_clicked_face() {
        let mut m = VoxelModel::new(8, 8, 8);
        for z in 0..8 {
            for x in 0..8 {
                m.set(x, 0, z, 1);
            }
        }
        // A lid over one corner of the floor, hiding it from above.
        m.set(0, 1, 0, 1);

        let top = cells(&m, Span::Plane, reach([4, 0, 4], Face::PosY, 1));
        assert_eq!(top.len(), 63, "the covered cell is not part of the top face");
        assert!(!top.contains(&[0, 0, 0]));
    }

    /// Building across a face must not spread over the empty part of the layer.
    #[test]
    fn a_plane_build_covers_the_face_and_not_the_air_beside_it() {
        let mut m = VoxelModel::new(8, 8, 8);
        for z in 2..5 {
            for x in 2..5 {
                m.set(x, 0, z, 1);
            }
        }
        let over = cells(&m, Span::Plane, reach([3, 1, 3], Face::PosY, 0));
        assert_eq!(over.len(), 9, "one cell over each of the nine below");
        assert!(over.iter().all(|p| p[1] == 1 && (2..5).contains(&p[0])));
    }

    /// Over the work plane there is nothing to back onto, so the layer itself
    /// is the surface — otherwise clicking bare plane would fill one cell and
    /// look broken.
    #[test]
    fn a_plane_build_on_the_work_plane_covers_the_whole_layer() {
        let m = VoxelModel::new(8, 8, 8);
        let mut r = reach([3, 4, 3], Face::PosY, 0);
        r.grounded = true;
        assert_eq!(cells(&m, Span::Plane, r).len(), 64);
    }

    #[test]
    fn a_volume_region_is_connected_by_faces_and_not_by_corners() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 1);
        m.set(2, 2, 1, 1); // touches the first only along an edge
        let got = cells(&m, Span::Volume, reach([1, 1, 1], Face::PosY, 1));
        assert_eq!(got, vec![[1, 1, 1]]);
    }

    #[test]
    fn a_volume_region_follows_the_shape_through_three_dimensions() {
        let mut m = VoxelModel::new(8, 8, 8);
        for i in 0..6 {
            m.set(1 + i, 1, 1, 1);
            m.set(6, 1 + i, 1, 1);
            m.set(6, 6, 1 + i, 1);
        }
        m.set(0, 7, 7, 1); // a separate speck, which must not join
        let got = cells(&m, Span::Volume, reach([1, 1, 1], Face::PosY, 1));
        assert_eq!(got.len(), m.filled_count() - 1);
        assert!(!got.contains(&[0, 7, 7]));
    }

    /// The slice rule, in both directions: a hidden layer is not reachable, and
    /// it does not count as material either — the cut face is a face.
    #[test]
    fn a_span_never_reaches_past_the_slice() {
        let mut m = VoxelModel::new(8, 8, 8);
        for y in 0..8 {
            for x in 0..8 {
                m.set(x, y, 3, 1);
            }
        }
        let mut r = reach([4, 2, 3], Face::PosY, 1);
        r.y_limit = 3;

        let got = cells(&m, Span::Plane, r);
        assert_eq!(got.len(), 8, "the top of the cross-section, one row of it");
        assert!(got.iter().all(|p| p[1] == 2));

        let column = cells(&m, Span::Axis, r);
        assert_eq!(sorted(column), vec![[4, 0, 3], [4, 1, 3], [4, 2, 3]]);
    }

    #[test]
    fn a_brush_stamps_a_cube_of_the_size_it_advertises() {
        let m = VoxelModel::new(16, 16, 16);
        let mut r = reach([8, 8, 8], Face::PosY, 0);
        r.brush = Brush {
            radius: 2,
            shape: BrushShape::Cube,
        };
        assert_eq!(r.brush.edge(), 5);
        assert_eq!(cells(&m, Span::Voxel, r).len(), 125);
    }

    /// A ball keeps the faces and edges of its bounding cube and drops the
    /// corners — the difference between a ball and a plus sign at the radii a
    /// voxel brush is used at.
    #[test]
    fn a_ball_brush_is_a_cube_with_its_corners_taken_off() {
        let m = VoxelModel::new(16, 16, 16);
        let mut r = reach([8, 8, 8], Face::PosY, 0);
        r.brush = Brush {
            radius: 1,
            shape: BrushShape::Sphere,
        };
        let got = cells(&m, Span::Voxel, r);
        assert_eq!(got.len(), 19, "27 less the eight corners");
        assert!(!got.contains(&[7, 7, 7]));
        assert!(got.contains(&[7, 8, 8]));
    }

    /// A brush at the edge of the volume is clipped, not wrapped — the cells it
    /// would cover outside simply are not there.
    #[test]
    fn a_brush_is_clipped_by_the_volume() {
        let m = VoxelModel::new(8, 8, 8);
        let mut r = reach([0, 0, 0], Face::PosY, 0);
        r.brush = Brush {
            radius: 1,
            shape: BrushShape::Cube,
        };
        let got = cells(&m, Span::Voxel, r);
        assert_eq!(got.len(), 8, "one octant of the 3x3x3");
        assert!(got.iter().all(|p| p.iter().all(|c| (0..2).contains(c))));
    }

    /// Reading one layer's own grid rather than the composite: the difference
    /// between changing a shape and copying it onto the layer above.
    #[test]
    fn a_region_can_be_grown_on_one_layers_own_grid() {
        let mut m = VoxelModel::new(8, 8, 8);
        for x in 0..4 {
            m.set(x, 0, 0, 1);
        }
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(2, 0, 0, 1);

        // Along X, so the run is the row rather than one cell of a column.
        // On the composite it is the whole four-cell row.
        let mut r = reach([0, 0, 0], Face::PosX, 1);
        assert_eq!(cells(&m, Span::Axis, r).len(), 4);

        // On the upper layer's own grid there is nothing at the seed at all.
        r.layer = Some(top);
        assert!(cells(&m, Span::Axis, r).is_empty());
        // And seeded where that layer does hold something, the run is its own.
        r.seed = [2, 0, 0];
        assert_eq!(cells(&m, Span::Axis, r), vec![[2, 0, 0]]);
    }

    /// A click on something that is not what the span is made of yields
    /// nothing, rather than a region grown from the wrong seed.
    #[test]
    fn a_seed_that_does_not_match_reaches_nothing() {
        let m = floor();
        assert!(cells(&m, Span::Axis, reach([1, 0, 1], Face::PosY, 2)).is_empty());
        assert!(cells(&m, Span::Plane, reach([1, 0, 1], Face::PosY, 2)).is_empty());
        assert!(cells(&m, Span::Volume, reach([1, 0, 1], Face::PosY, 2)).is_empty());
    }
}
