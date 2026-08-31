//! Turning a grid of voxels into the quads that are actually visible.
//!
//! The rule is one line long: emit a face where a solid voxel touches air. That
//! is enough to cut a 64³ solid from 1.5 million faces to 24 576, and it is the
//! only culling done here — coplanar neighbours are still separate quads.
//! Greedy meshing would merge them, and is the obvious next step, but it
//! changes the quads' extents and so has to be built on a face extractor that
//! is already known to be right.

use voxel_core::{Face, VoxelModel};

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
        let a = self.face.axis();
        let (b, c) = if self.face.is_positive() {
            ((a + 1) % 3, (a + 2) % 3)
        } else {
            ((a + 2) % 3, (a + 1) % 3)
        };

        let mut base = [
            self.voxel[0] as f32,
            self.voxel[1] as f32,
            self.voxel[2] as f32,
        ];
        if self.face.is_positive() {
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

/// The extracted surface of one model.
#[derive(Clone, Default, Debug)]
pub struct FaceMesh {
    pub quads: Vec<FaceQuad>,
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

/// Extract every face of `model` that touches air.
pub fn extract(model: &VoxelModel, opts: ExtractOptions) -> FaceMesh {
    let [sx, sy, sz] = model.size();
    let sy = sy.min(opts.y_limit);
    let mut quads = Vec::new();

    for z in 0..sz as i32 {
        for y in 0..sy as i32 {
            for x in 0..sx as i32 {
                let index = model.get(x, y, z);
                if index == 0 {
                    continue;
                }
                for face in Face::ALL {
                    let n = face.normal();
                    let (nx, ny, nz) = (x + n[0], y + n[1], z + n[2]);
                    // Above the cut counts as air, which is what puts a lid on
                    // the slice. Outside the grid already reads as air.
                    let occluded = ny < sy as i32 && model.is_solid(nx, ny, nz);
                    if !occluded {
                        quads.push(FaceQuad {
                            voxel: [x as u16, y as u16, z as u16],
                            face,
                            index,
                        });
                    }
                }
            }
        }
    }
    FaceMesh { quads }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_voxel_has_six_faces() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 1);
        assert_eq!(extract(&m, ExtractOptions::default()).quads.len(), 6);
    }

    /// Two neighbours share a face that neither should emit: 12 - 2 = 10.
    #[test]
    fn a_shared_face_is_dropped_from_both_sides() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 1);
        m.set(2, 1, 1, 1);
        assert_eq!(extract(&m, ExtractOptions::default()).quads.len(), 10);
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
        assert_eq!(extract(&m, ExtractOptions::default()).quads.len(), 6 * 8 * 8);
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
        assert!(mesh.quads.iter().all(|q| q.voxel[1] < 2));
        // Two voxels: 12 faces, minus the 2 they share.
        assert_eq!(mesh.quads.len(), 10);
        assert!(mesh
            .quads
            .iter()
            .any(|q| q.face == Face::PosY && q.voxel[1] == 1));
    }

    /// Every face must wind counter-clockwise seen from outside, or half the
    /// model is back-face culled away. Checked against the normal the face
    /// reports, which the raycaster independently agrees with.
    #[test]
    fn every_face_winds_outward() {
        let mut m = VoxelModel::new(3, 3, 3);
        m.set(1, 1, 1, 1);
        for q in extract(&m, ExtractOptions::default()).quads {
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
        for q in extract(&m, ExtractOptions::default()).quads {
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

    #[test]
    fn faces_carry_the_colour_of_the_voxel_they_belong_to() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 37);
        assert!(extract(&m, ExtractOptions::default())
            .quads
            .iter()
            .all(|q| q.index == 37));
    }
}
