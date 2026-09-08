//! Turning a grid of voxels into the quads that are actually visible.
//!
//! The rule is one line long: emit a face where a solid voxel touches air. That
//! is enough to cut a 64³ solid from 1.5 million faces to 24 576, and it is the
//! only culling done here — coplanar neighbours are still separate quads.
//! Greedy meshing would merge them, and is the obvious next step, but it
//! changes the quads' extents and so has to be built on a face extractor that
//! is already known to be right.

use voxel_core::{Face, VoxelModel, CHUNK};

use crate::math::{vec3, Vec3};

/// One visible face: which voxel it belongs to, which way it points, and what
/// colour it is.
///
/// Ten bytes, because a full 64³ shell is 24 576 of them and this is rebuilt
/// every time the model changes. The corners are derived on demand
/// ([`FaceQuad::corners`]) rather than stored — four `Vec3`s would be 48 bytes
/// of cache traffic to save three additions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FaceQuad {
    pub voxel: [u16; 3],
    pub face: Face,
    pub index: u8,
}

impl FaceQuad {
    /// The four corners in world units, counter-clockwise seen from outside.
    ///
    /// Winding is derived rather than tabulated. For a face on axis `a`, the
    /// other two axes in cyclic order (`b`, `c`) satisfy `e_b × e_c = e_a`, so
    /// walking `base, +b, +b+c, +c` is counter-clockwise about `+a`; a negative
    /// face swaps them, which reverses the winding exactly as it must. Six
    /// hand-written corner lists would each be a chance to get one backwards.
    pub fn corners(&self) -> [Vec3; 4] {
        face_corners(
            [
                self.voxel[0] as i32,
                self.voxel[1] as i32,
                self.voxel[2] as i32,
            ],
            self.face,
        )
    }

    /// The outward unit normal.
    pub fn normal(&self) -> Vec3 {
        let n = self.face.normal();
        vec3(n[0] as f32, n[1] as f32, n[2] as f32)
    }

    /// The face's midpoint, used for back-face culling and for sorting.
    pub fn center(&self) -> Vec3 {
        let c = self.corners();
        (c[0] + c[2]) * 0.5
    }
}

/// The four corners of one face of the cell at `voxel`, in world units,
/// counter-clockwise seen from outside.
///
/// Signed coordinates, because callers legitimately name cells outside the
/// grid: the editor's ground-plane target is the top face of a row one *below*
/// the volume, and an unsigned parameter would wrap that to the far end of the
/// world.
pub fn face_corners(voxel: [i32; 3], face: Face) -> [Vec3; 4] {
    let a = face.axis();
    let (b, c) = if face.is_positive() {
        ((a + 1) % 3, (a + 2) % 3)
    } else {
        ((a + 2) % 3, (a + 1) % 3)
    };

    let mut base = [voxel[0] as f32, voxel[1] as f32, voxel[2] as f32];
    if face.is_positive() {
        base[a] += 1.0;
    }

    let mut unit_b = [0.0f32; 3];
    unit_b[b] = 1.0;
    let mut unit_c = [0.0f32; 3];
    unit_c[c] = 1.0;

    let at = |sb: f32, sc: f32| {
        vec3(
            base[0] + unit_b[0] * sb + unit_c[0] * sc,
            base[1] + unit_b[1] * sb + unit_c[1] * sc,
            base[2] + unit_b[2] * sb + unit_c[2] * sc,
        )
    };
    [at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)]
}

/// The extracted surface of one model, kept chunk by chunk.
///
/// Split so a change can be answered in proportion to itself. A full rebuild at
/// 256³ is about three hundred milliseconds, and almost every change is one
/// voxel; keeping the quads in the chunk they came from means only that chunk —
/// and whichever neighbour shares a face with it — has to be walked again.
///
/// The renderer wants one stream of quads and gets one from [`quads`](Self::quads);
/// nothing outside here needs to know the parts exist.
#[derive(Clone, Default, Debug)]
pub struct FaceMesh {
    chunks: Vec<Vec<FaceQuad>>,
    dims: [usize; 3],
}

impl FaceMesh {
    /// Every face, in no particular order. Order has never mattered: the
    /// rasterizer depth-tests.
    pub fn quads(&self) -> impl Iterator<Item = &FaceQuad> + '_ {
        self.chunks.iter().flatten()
    }

    pub fn len(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.iter().all(Vec::is_empty)
    }
}

/// What to leave out.
#[derive(Clone, Copy, Debug)]
pub struct ExtractOptions {
    /// Hide every voxel at or above this Y. Layers above the cut are treated as
    /// air rather than skipped, so the cut *surface* gets its own top faces and
    /// a slice view shows a solid cross-section instead of a hollow shell.
    pub y_limit: u16,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self { y_limit: u16::MAX }
    }
}

/// Extract every face of `model` that touches air, from scratch.
pub fn extract(model: &VoxelModel, opts: ExtractOptions) -> FaceMesh {
    let mut mesh = FaceMesh::default();
    rebuild(model, opts, &mut mesh, |_| true);
    mesh
}

/// Rebuild only the chunks the model says have changed.
///
/// The caller clears the model's dirty marks once it has the result — this does
/// not, because it takes the model by reference and because "who has caught up"
/// is the caller's business, not the model's.
pub fn extract_dirty(model: &VoxelModel, opts: ExtractOptions, mesh: &mut FaceMesh) {
    // A mesh built for a different scene shares nothing with this one.
    if mesh.dims != model.chunk_dims() {
        rebuild(model, opts, mesh, |_| true);
        return;
    }
    rebuild(model, opts, mesh, |i| model.chunk_is_dirty(i));
}

fn rebuild(
    model: &VoxelModel,
    opts: ExtractOptions,
    mesh: &mut FaceMesh,
    wanted: impl Fn(usize) -> bool,
) {
    let dims = model.chunk_dims();
    if mesh.dims != dims {
        mesh.dims = dims;
        mesh.chunks.clear();
        mesh.chunks.resize_with(dims.iter().product(), Vec::new);
    }
    let cut = model.size()[1].min(opts.y_limit) as i32;
    let chunk = CHUNK as i32;

    for cz in 0..dims[2] {
        for cy in 0..dims[1] {
            for cx in 0..dims[0] {
                let i = cx + cy * dims[0] + cz * dims[0] * dims[1];
                if !wanted(i) {
                    continue;
                }
                let origin = [cx as i32 * chunk, cy as i32 * chunk, cz as i32 * chunk];
                let quads = &mut mesh.chunks[i];
                quads.clear();
                if !touches_a_layer(model, origin, chunk) {
                    continue;
                }
                fill_chunk(model, cut, origin, chunk, quads);
            }
        }
    }
}

/// Whether any visible layer with anything in it reaches into this chunk.
///
/// The skip that keeps a sparse scene cheap: a layer is only as big as its
/// contents, so a chunk no layer's box touches is empty without looking at a
/// single cell — and a scene is mostly the room you left yourself.
///
/// The emptiness test is not redundant with the box test. A box is a
/// **high-water mark**: erasing a layer leaves it the size it was until someone
/// trims it, so a cleared scene would otherwise sweep every cell it used to
/// have to find nothing. `Layer::filled_count` is O(1), so asking costs
/// nothing.
fn touches_a_layer(model: &VoxelModel, origin: [i32; 3], chunk: i32) -> bool {
    model.layers().iter().any(|l| {
        if !l.shown() || l.bounds().is_empty() || l.is_empty() {
            return false;
        }
        let b = l.bounds();
        let end = b.end();
        (0..3).all(|a| (b.origin[a] as i32) < origin[a] + chunk && end[a] > origin[a])
    })
}

fn fill_chunk(model: &VoxelModel, cut: i32, origin: [i32; 3], chunk: i32, out: &mut Vec<FaceQuad>) {
    let size = model.size();
    let hi = [
        (origin[0] + chunk).min(size[0] as i32),
        (origin[1] + chunk).min(size[1] as i32).min(cut),
        (origin[2] + chunk).min(size[2] as i32),
    ];
    for z in origin[2]..hi[2] {
        for y in origin[1]..hi[1] {
            for x in origin[0]..hi[0] {
                // The composite, which is already one value per cell — so
                // where two layers overlap the face comes out once, from the
                // one on top, with no second question to ask.
                let index = model.get(x, y, z);
                if index == 0 {
                    continue;
                }
                for face in Face::ALL {
                    let f = face.normal();
                    let (nx, ny, nz) = (x + f[0], y + f[1], z + f[2]);
                    // Above the cut counts as air, which is what puts a lid on
                    // the slice. Outside the scene already reads as air.
                    let occluded = ny < cut && model.is_solid(nx, ny, nz);
                    if !occluded {
                        out.push(FaceQuad {
                            voxel: [x as u16, y as u16, z as u16],
                            face,
                            index,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole thing rests on: rebuilding only what changed has
    /// to give the same answer as rebuilding everything. A stale chunk is a
    /// hole in the model, or a face floating where a voxel used to be.
    #[test]
    fn an_incremental_rebuild_matches_a_full_one() {
        fn sorted(m: &FaceMesh) -> Vec<(([u16; 3], Face), u8)> {
            let mut v: Vec<_> = m.quads().map(|q| ((q.voxel, q.face), q.index)).collect();
            v.sort_by_key(|(k, _)| (k.0, format!("{:?}", k.1)));
            v
        }

        // A scene several chunks across, so edits land in different ones and on
        // the seams between them.
        let mut m = VoxelModel::new(48, 32, 48);
        for z in 10..40 {
            for x in 10..40 {
                m.set(x, 1, z, 4);
            }
        }
        let mut incremental = FaceMesh::default();
        extract_dirty(&m, ExtractOptions::default(), &mut incremental);
        m.clear_dirty();
        assert_eq!(
            sorted(&incremental),
            sorted(&extract(&m, ExtractOptions::default()))
        );

        // Every edit that could leave a chunk stale: interior, on a chunk face,
        // on an edge, on a corner, an erase, and a recolour.
        for (what, cell, colour) in [
            ("interior", [20, 5, 20], 7),
            ("on a chunk face", [15, 1, 20], 7),
            ("just past one", [16, 1, 20], 7),
            ("on a chunk edge", [15, 15, 20], 7),
            ("on a chunk corner", [31, 15, 31], 7),
            ("an erase", [20, 1, 20], 0),
            ("a recolour", [21, 1, 21], 9),
            ("out at the scene edge", [47, 0, 47], 5),
        ] {
            m.set(cell[0], cell[1], cell[2], colour);
            extract_dirty(&m, ExtractOptions::default(), &mut incremental);
            m.clear_dirty();
            assert_eq!(
                sorted(&incremental),
                sorted(&extract(&m, ExtractOptions::default())),
                "{what} at {cell:?} left the mesh wrong"
            );
        }
    }

    /// A slice, a hidden layer and a subdivide are not attributable to cells,
    /// so they mark everything — and the incremental path has to notice.
    #[test]
    fn changes_too_broad_for_a_chunk_still_come_out_right() {
        fn count(m: &FaceMesh) -> usize {
            m.len()
        }
        let mut m = VoxelModel::new(32, 32, 32);
        for z in 4..12 {
            for y in 4..12 {
                for x in 4..12 {
                    m.set(x, y, z, 4);
                }
            }
        }
        let mut mesh = FaceMesh::default();
        extract_dirty(&m, ExtractOptions::default(), &mut mesh);
        m.clear_dirty();

        let sliced = ExtractOptions { y_limit: 8 };
        // A slice is the caller's business to announce; the editor calls
        // `dirty_all`. Without it the old faces would stand.
        m.dirty_all();
        extract_dirty(&m, sliced, &mut mesh);
        m.clear_dirty();
        assert_eq!(count(&mesh), count(&extract(&m, sliced)));

        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(5, 5, 5, 9);
        extract_dirty(&m, sliced, &mut mesh);
        m.clear_dirty();
        assert_eq!(count(&mesh), count(&extract(&m, sliced)));

        m.set_layer_visible(top, false);
        extract_dirty(&m, sliced, &mut mesh);
        m.clear_dirty();
        assert_eq!(count(&mesh), count(&extract(&m, sliced)));

        // And a subdivide changes the chunk grid itself, so the old mesh shares
        // nothing with the new one.
        m.subdivide(2).unwrap();
        extract_dirty(&m, ExtractOptions::default(), &mut mesh);
        m.clear_dirty();
        assert_eq!(count(&mesh), count(&extract(&m, ExtractOptions::default())));
    }

    /// One voxel dirties its own chunk, and a neighbour only when it sits
    /// against a shared face. That is the whole saving.
    #[test]
    fn an_edit_dirties_its_chunk_and_only_a_touching_neighbour() {
        let mut m = VoxelModel::new(48, 48, 48);
        m.clear_dirty();
        m.set(20, 20, 20, 1);
        assert_eq!(m.dirty_chunk_count(), 1, "well inside one chunk");

        m.clear_dirty();
        m.set(16, 20, 20, 1);
        assert_eq!(m.dirty_chunk_count(), 2, "against the low face on x");

        m.clear_dirty();
        m.set(31, 31, 31, 1);
        assert_eq!(
            m.dirty_chunk_count(),
            4,
            "a corner: itself and three across"
        );

        m.clear_dirty();
        m.set(0, 0, 0, 1);
        assert_eq!(
            m.dirty_chunk_count(),
            1,
            "the scene's own corner has no neighbours"
        );

        m.clear_dirty();
        m.set(20, 20, 20, 1);
        assert_eq!(
            m.dirty_chunk_count(),
            0,
            "writing what is already there is not a change"
        );
    }

    /// Two layers sharing a cell must mesh it once, from the one on top —
    /// otherwise the lower copy z-fights the upper one at every shared face.
    #[test]
    fn a_cell_two_layers_share_is_meshed_once_by_its_owner() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);

        let mesh = extract(&m, ExtractOptions::default());
        assert_eq!(mesh.len(), 6, "one cube, not two");
        assert!(mesh.quads().all(|q| q.index == 8), "the top layer's colour");

        // Hide the cover and the one underneath takes over, still once.
        m.set_layer_visible(top, false);
        let mesh = extract(&m, ExtractOptions::default());
        assert_eq!(mesh.len(), 6);
        assert!(mesh.quads().all(|q| q.index == 3));
    }

    /// Extraction walks the layers, so a scene with room to spare costs what is
    /// drawn in it rather than what it could hold.
    #[test]
    fn layers_far_apart_in_a_large_scene_both_mesh() {
        let mut m = VoxelModel::new(256, 16, 16);
        m.set(0, 0, 0, 1);
        let far = m.add_layer(0, "far").unwrap();
        m.set_active_layer(far);
        m.set(255, 15, 15, 2);

        let mesh = extract(&m, ExtractOptions::default());
        assert_eq!(mesh.len(), 12, "two lone voxels, six faces each");
        assert_eq!(
            m.allocated_cells(),
            2,
            "and the scene between them costs nothing"
        );
    }

    #[test]
    fn a_lone_voxel_has_six_faces() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 1);
        assert_eq!(extract(&m, ExtractOptions::default()).len(), 6);
    }

    /// Two neighbours share a face that neither should emit: 12 - 2 = 10.
    #[test]
    fn a_shared_face_is_dropped_from_both_sides() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 1);
        m.set(2, 1, 1, 1);
        assert_eq!(extract(&m, ExtractOptions::default()).len(), 10);
    }

    /// The interior of a solid block must not be meshed at all — this is the
    /// entire point of the pass.
    #[test]
    fn a_solid_block_meshes_only_its_shell() {
        let mut m = VoxelModel::new(8, 8, 8);
        for x in 0..8 {
            for y in 0..8 {
                for z in 0..8 {
                    m.set(x, y, z, 1);
                }
            }
        }
        assert_eq!(extract(&m, ExtractOptions::default()).len(), 6 * 8 * 8);
    }

    /// A slice must be capped. Without the `ny < sy` guard the cut layer's top
    /// faces are suppressed by the (hidden) voxel above and you see through the
    /// model.
    #[test]
    fn a_slice_gets_a_lid() {
        let mut m = VoxelModel::new(1, 4, 1);
        for y in 0..4 {
            m.set(0, y, 0, 1);
        }
        let mesh = extract(&m, ExtractOptions { y_limit: 2 });
        assert!(mesh.quads().all(|q| q.voxel[1] < 2));
        // Two voxels: 12 faces, minus the 2 they share.
        assert_eq!(mesh.len(), 10);
        assert!(mesh
            .quads()
            .any(|q| q.face == Face::PosY && q.voxel[1] == 1));
    }

    /// Every face must wind counter-clockwise seen from outside, or half the
    /// model is back-face culled away. Checked against the normal the face
    /// reports, which the raycaster independently agrees with.
    #[test]
    fn every_face_winds_outward() {
        let mut m = VoxelModel::new(3, 3, 3);
        m.set(1, 1, 1, 1);
        for q in extract(&m, ExtractOptions::default()).quads() {
            let c = q.corners();
            let cross = (c[1] - c[0]).cross(c[2] - c[0]).normalized();
            let n = q.normal();
            assert!(
                (cross - n).length() < 1e-5,
                "{:?}: winding gives {cross:?}, normal is {n:?}",
                q.face
            );
        }
    }

    /// Corners must bound the unit cube the voxel occupies: a face of voxel
    /// (1,1,1) lies within [1,2]³ and is flat on its own axis.
    #[test]
    fn corners_sit_on_the_voxels_own_cube() {
        let mut m = VoxelModel::new(3, 3, 3);
        m.set(1, 1, 1, 1);
        for q in extract(&m, ExtractOptions::default()).quads() {
            let a = q.face.axis();
            let expect = 1.0 + if q.face.is_positive() { 1.0 } else { 0.0 };
            for c in q.corners() {
                let arr = c.to_array();
                assert_eq!(arr[a], expect, "{:?}", q.face);
                for v in arr {
                    assert!((1.0..=2.0).contains(&v), "{c:?}");
                }
            }
        }
    }

    /// The ground target the editor builds against lives one row below the
    /// volume, so negative coordinates have to survive the corner maths.
    #[test]
    fn corners_of_a_cell_below_the_origin_stay_below_it() {
        let c = face_corners([2, -1, 3], Face::PosY);
        for corner in c {
            assert_eq!(corner.y, 0.0, "the top face of y = -1 is the plane y = 0");
        }
    }

    #[test]
    fn faces_carry_the_colour_of_the_voxel_they_belong_to() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 37);
        assert!(extract(&m, ExtractOptions::default())
            .quads()
            .all(|q| q.index == 37));
    }
}
