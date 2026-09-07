//! The rasterizer: clip, project, fill, depth-test.
//!
//! Flat shading only. A voxel face is planar and single-coloured by
//! construction, so there is nothing to interpolate across it but depth — and
//! normalized device z *is* linear in screen space, which is what makes the
//! barycentric interpolation below exact rather than approximate.

use voxel_core::Palette;

use crate::camera::OrbitCamera;
use crate::framebuffer::Framebuffer;
use crate::math::{Mat4, Vec3, Vec4};
use crate::mesh::FaceMesh;

/// One directional light plus a floor.
///
/// The ambient term is deliberately high. With none, a face turned away from
/// the light is black and a voxel model turns into a silhouette; the shading
/// here exists to tell the six face directions apart, not to be physical.
#[derive(Clone, Copy, Debug)]
pub struct Light {
    /// The direction light travels, normalized.
    pub direction: Vec3,
    pub ambient: f32,
    pub diffuse: f32,
}

impl Default for Light {
    fn default() -> Self {
        Self {
            // Off-axis on all three, so no two faces of a cube get the same
            // brightness — an axis-aligned light leaves opposite pairs tied and
            // the model reads flat.
            direction: crate::math::vec3(-0.45, -1.0, -0.35).normalized(),
            ambient: 0.42,
            diffuse: 0.58,
        }
    }
}

impl Light {
    /// How much light a face with normal `n` receives.
    pub fn intensity(&self, n: Vec3) -> f32 {
        self.ambient + self.diffuse * n.dot(-self.direction).max(0.0)
    }
}

/// Everything a draw call needs that is not the geometry.
#[derive(Clone, Copy, Debug)]
pub struct Scene {
    pub view_proj: Mat4,
    pub eye: Vec3,
    pub light: Light,
}

impl Scene {
    pub fn new(camera: &OrbitCamera, aspect: f32, light: Light) -> Self {
        Self {
            view_proj: camera.view_projection(aspect),
            eye: camera.eye(),
            light,
        }
    }
}

/// Draw every quad of `mesh`, offset into world space by `offset`.
///
/// `offset` is how a model is placed: the grid's own coordinates start at the
/// origin, and the editor centres the volume by passing its negative half-size.
pub fn draw_mesh(
    fb: &mut Framebuffer,
    scene: &Scene,
    mesh: &FaceMesh,
    palette: &Palette,
    offset: Vec3,
) {
    for quad in mesh.quads() {
        let normal = quad.normal();
        // Back-face cull in world space, before any matrix work. It is one dot
        // product and it removes about half the quads — cheaper than projecting
        // them and finding out from the signed area.
        if normal.dot(scene.eye - (quad.center() + offset)) <= 0.0 {
            continue;
        }
        let color = palette
            .get(quad.index)
            .scaled(scene.light.intensity(normal))
            .to_u32();
        let c = quad.corners();
        let world = [
            c[0] + offset,
            c[1] + offset,
            c[2] + offset,
            c[3] + offset,
        ];
        fill_polygon(fb, scene, &world, color, 0.0);
    }
}

/// Fill a convex world-space polygon, depth-tested.
///
/// `bias` is subtracted from the depth, in ndc units: pass a small positive
/// value to lay something (a highlight, a wireframe) on top of coplanar
/// geometry without it fighting for the pixel.
pub fn fill_polygon(
    fb: &mut Framebuffer,
    scene: &Scene,
    poly: &[Vec3],
    color: u32,
    bias: f32,
) {
    let mut clip = [Vec4::default(); 8];
    let n = to_clip_space(&scene.view_proj, poly, &mut clip);
    let Some(n) = clip_near(&mut clip, n) else {
        return;
    };

    let mut screen = [Vertex::default(); 8];
    for i in 0..n {
        screen[i] = project(fb, clip[i], bias);
    }
    // Fan from the first vertex: valid because near-plane clipping of a convex
    // polygon leaves it convex.
    for i in 1..n - 1 {
        fill_triangle(fb, screen[0], screen[i], screen[i + 1], color);
    }
}

/// Draw a world-space line segment, depth-tested.
pub fn draw_line(fb: &mut Framebuffer, scene: &Scene, a: Vec3, b: Vec3, color: u32, bias: f32) {
    let mut clip = [
        scene.view_proj.transform_point(a),
        scene.view_proj.transform_point(b),
    ];
    if !clip_segment_near(&mut clip) {
        return;
    }
    let (p, q) = (project(fb, clip[0], bias), project(fb, clip[1], bias));

    // Step along whichever axis is longer, so the line has no gaps.
    let steps = (q.x - p.x).abs().max((q.y - p.y).abs()).ceil().max(1.0);
    if !steps.is_finite() || steps > 1e6 {
        return; // a segment nearly edge-on to the near plane can blow up
    }
    let inv = 1.0 / steps;
    for i in 0..=steps as u32 {
        let t = i as f32 * inv;
        let x = p.x + (q.x - p.x) * t;
        let y = p.y + (q.y - p.y) * t;
        let z = p.z + (q.z - p.z) * t;
        if x >= 0.0 && y >= 0.0 {
            fb.test_and_set(x as u32, y as u32, z, color);
        }
    }
}

/// Draw the twelve edges of an axis-aligned box.
pub fn draw_box(fb: &mut Framebuffer, scene: &Scene, min: Vec3, max: Vec3, color: u32, bias: f32) {
    let corner = |i: usize| {
        crate::math::vec3(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        )
    };
    // Every pair of corners differing in exactly one bit is an edge.
    for i in 0..8usize {
        for bit in [1usize, 2, 4] {
            let j = i | bit;
            if j != i {
                draw_line(fb, scene, corner(i), corner(j), color, bias);
            }
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct Vertex {
    x: f32,
    y: f32,
    z: f32,
}

fn to_clip_space(vp: &Mat4, poly: &[Vec3], out: &mut [Vec4; 8]) -> usize {
    let n = poly.len().min(8);
    for i in 0..n {
        out[i] = vp.transform_point(poly[i]);
    }
    n
}

/// Clip a convex polygon against the near plane, `z + w >= 0`.
///
/// This has to happen before the perspective divide: a vertex behind the camera
/// has a negative `w`, and dividing by it mirrors the vertex to the wrong side
/// of the screen — a triangle that should be partly visible instead draws as a
/// wild wedge across the whole window.
fn clip_near(poly: &mut [Vec4; 8], n: usize) -> Option<usize> {
    let dist = |v: Vec4| v.z + v.w;
    if (0..n).all(|i| dist(poly[i]) >= 0.0) {
        return (n >= 3).then_some(n);
    }
    let mut out = [Vec4::default(); 8];
    let mut m = 0;
    for i in 0..n {
        let (cur, next) = (poly[i], poly[(i + 1) % n]);
        let (dc, dn) = (dist(cur), dist(next));
        if dc >= 0.0 && m < 8 {
            out[m] = cur;
            m += 1;
        }
        // Sign change: the edge crosses the plane, so emit the crossing point.
        if (dc >= 0.0) != (dn >= 0.0) && m < 8 {
            out[m] = cur.lerp(next, dc / (dc - dn));
            m += 1;
        }
    }
    *poly = out;
    (m >= 3).then_some(m)
}

/// The two-vertex case, which the polygon clipper's fan logic does not cover.
/// Returns false when the whole segment is behind the near plane.
fn clip_segment_near(seg: &mut [Vec4; 2]) -> bool {
    let dist = |v: Vec4| v.z + v.w;
    let (d0, d1) = (dist(seg[0]), dist(seg[1]));
    if d0 < 0.0 && d1 < 0.0 {
        return false;
    }
    if d0 < 0.0 {
        seg[0] = seg[0].lerp(seg[1], d0 / (d0 - d1));
    } else if d1 < 0.0 {
        seg[1] = seg[1].lerp(seg[0], d1 / (d1 - d0));
    }
    true
}

fn project(fb: &Framebuffer, v: Vec4, bias: f32) -> Vertex {
    let inv_w = 1.0 / v.w;
    Vertex {
        x: (v.x * inv_w * 0.5 + 0.5) * fb.width() as f32,
        // Y flips: ndc grows upward, rows grow downward.
        y: (0.5 - v.y * inv_w * 0.5) * fb.height() as f32,
        z: v.z * inv_w - bias,
    }
}

#[inline]
fn edge(a: Vertex, b: Vertex, x: f32, y: f32) -> f32 {
    (b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x)
}

fn fill_triangle(fb: &mut Framebuffer, a: Vertex, b: Vertex, c: Vertex, color: u32) {
    let area = edge(a, b, c.x, c.y);
    if area.abs() < 1e-6 {
        return; // degenerate, or exactly edge-on
    }
    // Dividing by the *signed* area makes the barycentrics positive inside the
    // triangle for either winding, so this fills a back-facing polygon too —
    // which is what a highlight overlay or a gizmo needs.
    let inv_area = 1.0 / area;

    let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as u32;
    let max_x = (a.x.max(b.x).max(c.x).ceil() as i64).clamp(0, fb.width() as i64) as u32;
    let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as u32;
    let max_y = (a.y.max(b.y).max(c.y).ceil() as i64).clamp(0, fb.height() as i64) as u32;

    for py in min_y..max_y {
        let y = py as f32 + 0.5;
        for px in min_x..max_x {
            let x = px as f32 + 0.5;
            let w0 = edge(b, c, x, y) * inv_area;
            let w1 = edge(c, a, x, y) * inv_area;
            let w2 = edge(a, b, x, y) * inv_area;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            fb.test_and_set(px, py, w0 * a.z + w1 * b.z + w2 * c.z, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::vec3;
    use crate::mesh::{extract, ExtractOptions};
    use voxel_core::VoxelModel;

    fn scene(fb: &Framebuffer, camera: &OrbitCamera) -> Scene {
        Scene::new(camera, fb.width() as f32 / fb.height() as f32, Light::default())
    }

    fn one_voxel_scene() -> (Framebuffer, Scene, FaceMesh, Palette) {
        let mut m = VoxelModel::new(1, 1, 1);
        m.set(0, 0, 0, 1);
        let fb = Framebuffer::new(64, 64);
        let camera = OrbitCamera {
            target: vec3(0.5, 0.5, 0.5),
            distance: 6.0,
            ..Default::default()
        };
        let s = scene(&fb, &camera);
        (
            fb,
            s,
            extract(&m, ExtractOptions::default()),
            m.palette().clone(),
        )
    }

    #[test]
    fn a_voxel_at_the_centre_of_the_view_lands_at_the_centre_pixel() {
        let (mut fb, s, mesh, pal) = one_voxel_scene();
        fb.clear(0);
        draw_mesh(&mut fb, &s, &mesh, &pal, Vec3::ZERO);
        assert_ne!(fb.color_at(32, 32), 0);
        assert_eq!(fb.color_at(0, 0), 0, "the corner should still be background");
    }

    /// Back-face culling plus the depth buffer must leave only the three faces
    /// nearest the camera, and each a different brightness — that is what makes
    /// a cube read as a cube.
    #[test]
    fn a_cube_shows_three_distinguishable_faces() {
        let (mut fb, s, mesh, pal) = one_voxel_scene();
        fb.clear(0);
        draw_mesh(&mut fb, &s, &mesh, &pal, Vec3::ZERO);
        let mut seen: Vec<u32> = (0..64)
            .flat_map(|y| (0..64).map(move |x| (x, y)))
            .map(|(x, y)| fb.color_at(x, y))
            .filter(|c| *c != 0)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 3, "expected 3 face shades, got {seen:?}");
    }

    /// The nearer of two overlapping quads must win no matter which is drawn
    /// first. Submitted far-then-near and near-then-far.
    #[test]
    fn the_depth_buffer_orders_overlapping_quads_either_way() {
        let mut fb = Framebuffer::new(32, 32);
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            distance: 10.0,
            yaw: 0.0,
            pitch: 0.0,
            ..Default::default()
        };
        let s = scene(&fb, &camera);
        let near: Vec<Vec3> = vec![
            vec3(-2.0, -2.0, 2.0),
            vec3(2.0, -2.0, 2.0),
            vec3(2.0, 2.0, 2.0),
            vec3(-2.0, 2.0, 2.0),
        ];
        let far: Vec<Vec3> = near.iter().map(|p| vec3(p.x, p.y, -2.0)).collect();

        // The colours travel with the geometry, not with the draw order --
        // that is the whole point of the assertion.
        for order in [[(&far, 0x111111u32), (&near, 0x222222u32)], [(&near, 0x222222), (&far, 0x111111)]] {
            fb.clear(0);
            for (poly, color) in order {
                fill_polygon(&mut fb, &s, poly, color, 0.0);
            }
            assert_eq!(fb.color_at(16, 16), 0x222222, "near quad must win");
        }
    }

    /// A polygon straddling the near plane must be clipped, not mirrored. The
    /// symptom of no clipping is a wedge covering the far corners of the
    /// screen, so this asserts the *background* survives where it should.
    #[test]
    fn a_polygon_crossing_the_near_plane_is_clipped_not_mirrored() {
        let mut fb = Framebuffer::new(64, 64);
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            distance: 5.0,
            yaw: 0.0,
            pitch: 0.0,
            near: 0.1,
            ..Default::default()
        };
        let s = scene(&fb, &camera);
        // A horizontal strip below eye level, running from well behind the
        // camera to well in front of it.
        let poly = [
            vec3(-0.2, -0.2, 20.0),
            vec3(0.2, -0.2, 20.0),
            vec3(0.2, -0.2, -20.0),
            vec3(-0.2, -0.2, -20.0),
        ];
        fb.clear(0);
        fill_polygon(&mut fb, &s, &poly, 0xFFFFFF, 0.0);

        // The strip is entirely below the eye and the camera is level, so no
        // part of it can project above the horizon -- which sits on the middle
        // row. The half behind the camera has a negative w; unclipped, the
        // divide mirrors it into the *upper* half, which is exactly what this
        // catches. (It does legitimately fill the bottom corners: a strip
        // passing within a near plane's distance of the eye subtends
        // everything below.)
        for y in 0..32 {
            for x in 0..64 {
                assert_eq!(fb.color_at(x, y), 0, "({x},{y}) is above the horizon");
            }
        }
        assert_ne!(fb.color_at(32, 63), 0, "the strip should still be drawn");
    }

    #[test]
    fn geometry_entirely_behind_the_camera_draws_nothing() {
        let mut fb = Framebuffer::new(32, 32);
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            distance: 5.0,
            yaw: 0.0,
            pitch: 0.0,
            ..Default::default()
        };
        let s = scene(&fb, &camera);
        fb.clear(0);
        let behind = [
            vec3(-1.0, -1.0, 10.0),
            vec3(1.0, -1.0, 10.0),
            vec3(1.0, 1.0, 10.0),
        ];
        fill_polygon(&mut fb, &s, &behind, 0xFFFFFF, 0.0);
        draw_line(&mut fb, &s, behind[0], behind[1], 0xFFFFFF, 0.0);
        assert!((0..32).all(|y| (0..32).all(|x| fb.color_at(x, y) == 0)));
    }

    /// A bias must let a line win the depth test against the face it lies on;
    /// without one the wireframe of a box dashes in and out along its edges.
    #[test]
    fn a_biased_line_draws_over_coplanar_geometry() {
        let mut fb = Framebuffer::new(32, 32);
        let camera = OrbitCamera {
            target: Vec3::ZERO,
            distance: 8.0,
            yaw: 0.0,
            pitch: 0.0,
            ..Default::default()
        };
        let s = scene(&fb, &camera);
        let quad = [
            vec3(-2.0, -2.0, 0.0),
            vec3(2.0, -2.0, 0.0),
            vec3(2.0, 2.0, 0.0),
            vec3(-2.0, 2.0, 0.0),
        ];
        fb.clear(0);
        fill_polygon(&mut fb, &s, &quad, 0x111111, 0.0);
        draw_line(&mut fb, &s, vec3(-2.0, 0.0, 0.0), vec3(2.0, 0.0, 0.0), 0xFF0000, 1e-3);
        assert_eq!(fb.color_at(16, 16), 0xFF0000);
    }

    #[test]
    fn a_light_never_makes_a_face_darker_than_ambient() {
        let l = Light::default();
        for n in [
            vec3(1.0, 0.0, 0.0),
            vec3(-1.0, 0.0, 0.0),
            vec3(0.0, 1.0, 0.0),
            vec3(0.0, -1.0, 0.0),
            vec3(0.0, 0.0, 1.0),
            vec3(0.0, 0.0, -1.0),
        ] {
            let i = l.intensity(n);
            assert!(i >= l.ambient && i <= l.ambient + l.diffuse, "{n:?} -> {i}");
        }
    }

    #[test]
    fn drawing_into_a_zero_sized_framebuffer_does_not_panic() {
        let mut fb = Framebuffer::new(0, 0);
        let camera = OrbitCamera::default();
        let s = Scene::new(&camera, 1.0, Light::default());
        fill_polygon(
            &mut fb,
            &s,
            &[Vec3::ZERO, vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0)],
            0xFF,
            0.0,
        );
        draw_box(&mut fb, &s, Vec3::ZERO, Vec3::splat(1.0), 0xFF, 0.0);
    }
}
