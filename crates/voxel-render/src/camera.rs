//! An orbit camera: the one control scheme a model editor needs.
//!
//! The camera is defined by where it looks (`target`), how far away it is, and
//! two angles — never by a position and an orientation. That is the difference
//! between "the model turns as I drag" and "I am flying around and have lost
//! it", and it is why every modeller from `voxeler` on works this way.

use crate::math::{vec3, Mat4, Vec3};

/// How near the pole the pitch may get, in radians.
///
/// Straight down is a singularity: the up vector and the view direction become
/// parallel and the view matrix degenerates. `look_at` survives that, but the
/// horizon spins wildly as the camera crosses over, so the pitch is stopped
/// just short instead.
const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.01;

#[derive(Clone, Copy, Debug)]
pub struct OrbitCamera {
    /// The point being orbited, in world units (one unit is one voxel).
    pub target: Vec3,
    pub distance: f32,
    /// Rotation about +Y. 0 looks along -Z.
    pub yaw: f32,
    /// Elevation. Positive is above the target.
    pub pitch: f32,
    pub fov_y: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            distance: 96.0,
            // A three-quarter view, so the first frame shows three faces of the
            // volume and reads as a solid rather than as a square.
            yaw: std::f32::consts::FRAC_PI_4,
            pitch: 0.5,
            fov_y: 50f32.to_radians(),
            near: 0.1,
            far: 4000.0,
        }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        self.target + self.offset()
    }

    fn offset(&self) -> Vec3 {
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        vec3(cp * sy, sp, cp * cy) * self.distance
    }

    /// World up. Fixed rather than derived, so the horizon never rolls.
    pub fn up(&self) -> Vec3 {
        vec3(0.0, 1.0, 0.0)
    }

    /// Unit vector from the camera towards the target.
    pub fn forward(&self) -> Vec3 {
        (self.target - self.eye()).normalized()
    }

    /// Unit vector pointing right on screen.
    pub fn right(&self) -> Vec3 {
        self.forward().cross(self.up()).normalized()
    }

    /// Unit vector pointing up on screen — not the world up, once pitched.
    pub fn screen_up(&self) -> Vec3 {
        self.right().cross(self.forward()).normalized()
    }

    pub fn view(&self) -> Mat4 {
        Mat4::look_at(self.eye(), self.target, self.up())
    }

    pub fn projection(&self, aspect: f32) -> Mat4 {
        Mat4::perspective(self.fov_y, aspect.max(1e-3), self.near, self.far)
    }

    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        self.projection(aspect).mul(&self.view())
    }

    /// Drag the camera around the target, in radians.
    pub fn orbit(&mut self, d_yaw: f32, d_pitch: f32) {
        self.yaw = (self.yaw + d_yaw).rem_euclid(std::f32::consts::TAU);
        self.pitch = (self.pitch + d_pitch).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Slide the target across the view plane. `dx`/`dy` are in world units.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.target = self.target + self.right() * dx + self.screen_up() * dy;
    }

    /// Multiply the distance — geometric so a wheel notch feels the same
    /// whether you are close in or far out, which a fixed step does not.
    pub fn zoom(&mut self, factor: f32) {
        self.distance = (self.distance * factor).clamp(1.0, 4000.0);
    }

    /// Frame an axis-aligned box: look at its centre from far enough back that
    /// it fits vertically, with a little margin.
    pub fn frame(&mut self, min: Vec3, max: Vec3) {
        self.target = (min + max) * 0.5;
        let radius = ((max - min) * 0.5).length().max(1.0);
        self.distance = (radius / (self.fov_y * 0.5).tan() * 1.4).clamp(1.0, 4000.0);
    }

    /// The world-space ray through a pixel centre, for picking.
    ///
    /// This is the inverse of the projection the rasterizer applies, written
    /// out directly rather than as a matrix inverse: it is six multiplies
    /// instead of a 4×4 inversion, and — the reason that matters — the two can
    /// be read side by side and checked against each other.
    pub fn ray(&self, px: f32, py: f32, width: u32, height: u32) -> (Vec3, Vec3) {
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        let aspect = w / h;
        let tan_half = (self.fov_y * 0.5).tan();
        // Pixel centres, so the ray for pixel 0 goes through its middle rather
        // than its top-left corner — half a pixel of bias is visible when
        // picking a voxel at a glancing angle.
        let ndc_x = (2.0 * (px + 0.5) / w - 1.0) * aspect * tan_half;
        let ndc_y = (1.0 - 2.0 * (py + 0.5) / h) * tan_half;
        let dir = (self.forward() + self.right() * ndc_x + self.screen_up() * ndc_y).normalized();
        (self.eye(), dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Vec3, b: Vec3, tol: f32) {
        assert!((a - b).length() < tol, "{a:?} != {b:?}");
    }

    #[test]
    fn yaw_zero_and_pitch_zero_looks_down_negative_z() {
        let c = OrbitCamera {
            yaw: 0.0,
            pitch: 0.0,
            distance: 10.0,
            target: Vec3::ZERO,
            ..Default::default()
        };
        approx(c.eye(), vec3(0.0, 0.0, 10.0), 1e-4);
        approx(c.forward(), vec3(0.0, 0.0, -1.0), 1e-4);
        approx(c.right(), vec3(1.0, 0.0, 0.0), 1e-4);
    }

    /// The centre pixel's ray must be the view direction, or picking and
    /// drawing disagree everywhere but by an amount too small to notice until
    /// a click lands on the wrong voxel.
    #[test]
    fn the_centre_pixel_ray_is_the_view_direction() {
        let c = OrbitCamera {
            yaw: 0.7,
            pitch: 0.4,
            ..Default::default()
        };
        let (origin, dir) = c.ray(159.5, 119.5, 320, 240);
        approx(origin, c.eye(), 1e-4);
        approx(dir, c.forward(), 1e-4);
    }

    /// A ray through a known pixel must reproject to that pixel. This is the
    /// check that ties `ray` to `view_projection` — the two are written
    /// independently and nothing else would catch them drifting apart.
    #[test]
    fn a_pick_ray_reprojects_to_the_pixel_it_came_from() {
        let c = OrbitCamera {
            yaw: 0.9,
            pitch: -0.3,
            distance: 40.0,
            target: vec3(3.0, 4.0, 5.0),
            ..Default::default()
        };
        let (w, h) = (640u32, 400u32);
        let vp = c.view_projection(w as f32 / h as f32);
        for (px, py) in [(0.0, 0.0), (321.0, 87.0), (639.0, 399.0)] {
            let (origin, dir) = c.ray(px, py, w, h);
            let p = origin + dir * 25.0;
            let clip = vp.transform_point(p);
            let sx = (clip.x / clip.w * 0.5 + 0.5) * w as f32;
            let sy = (0.5 - clip.y / clip.w * 0.5) * h as f32;
            assert!((sx - (px + 0.5)).abs() < 0.01, "x: {sx} vs {px}");
            assert!((sy - (py + 0.5)).abs() < 0.01, "y: {sy} vs {py}");
        }
    }

    #[test]
    fn pitch_stops_short_of_the_pole() {
        let mut c = OrbitCamera::default();
        c.orbit(0.0, 100.0);
        assert!(c.pitch < std::f32::consts::FRAC_PI_2);
        c.orbit(0.0, -100.0);
        assert!(c.pitch > -std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn zoom_is_geometric_and_bounded() {
        let mut c = OrbitCamera {
            distance: 100.0,
            ..Default::default()
        };
        c.zoom(0.5);
        assert_eq!(c.distance, 50.0);
        for _ in 0..200 {
            c.zoom(0.5);
        }
        assert_eq!(c.distance, 1.0);
    }

    #[test]
    fn framing_a_box_centres_it_and_backs_off() {
        let mut c = OrbitCamera::default();
        c.frame(Vec3::ZERO, Vec3::splat(64.0));
        approx(c.target, Vec3::splat(32.0), 1e-4);
        assert!(c.distance > 64.0, "{}", c.distance);
    }
}
