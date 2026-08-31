//! Just enough linear algebra: a 3-vector, a 4-vector, and a 4×4 matrix.
//!
//! Hand-rolled rather than pulled from `glam`, for the same reason the rest of
//! this workspace is: the surface actually used here is about twenty
//! operations, and a dependency that has to be kept in step is a worse trade
//! than a file you can read in one sitting.
//!
//! Matrices are row-major and multiply column vectors on the right, so
//! `p' = M * p` and composing is `proj * view * model` — read right to left.

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

impl Vec3 {
    pub const ZERO: Vec3 = vec3(0.0, 0.0, 0.0);

    pub const fn splat(v: f32) -> Vec3 {
        vec3(v, v, v)
    }

    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        vec3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Unit length, or zero for a zero vector — a `None` here would put an
    /// `unwrap` at every call site to handle a case none of them can produce.
    pub fn normalized(self) -> Vec3 {
        let l = self.length();
        if l <= f32::EPSILON {
            Vec3::ZERO
        } else {
            self * (1.0 / l)
        }
    }

    pub fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    pub fn from_array(a: [f32; 3]) -> Vec3 {
        vec3(a[0], a[1], a[2])
    }
}

impl std::ops::Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        vec3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        vec3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl std::ops::Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        vec3(-self.x, -self.y, -self.z)
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f32) -> Vec3 {
        vec3(self.x * s, self.y * s, self.z * s)
    }
}

/// A homogeneous point, as it exists between the projection matrix and the
/// perspective divide. Clipping happens here — after the divide the sign
/// information that says "behind the camera" is gone.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Vec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

pub const fn vec4(x: f32, y: f32, z: f32, w: f32) -> Vec4 {
    Vec4 { x, y, z, w }
}

impl Vec4 {
    /// Linear interpolation, used by the near-plane clipper to place the new
    /// vertex on the edge it cut.
    pub fn lerp(self, o: Vec4, t: f32) -> Vec4 {
        vec4(
            self.x + (o.x - self.x) * t,
            self.y + (o.y - self.y) * t,
            self.z + (o.z - self.z) * t,
            self.w + (o.w - self.w) * t,
        )
    }
}

/// A 4×4 matrix, row-major: `m[row][col]`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mat4(pub [[f32; 4]; 4]);

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]);

    pub fn mul(&self, o: &Mat4) -> Mat4 {
        let mut r = [[0.0f32; 4]; 4];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..4).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        Mat4(r)
    }

    /// Transform a point (w = 1), keeping the homogeneous w.
    pub fn transform_point(&self, p: Vec3) -> Vec4 {
        let m = &self.0;
        vec4(
            m[0][0] * p.x + m[0][1] * p.y + m[0][2] * p.z + m[0][3],
            m[1][0] * p.x + m[1][1] * p.y + m[1][2] * p.z + m[1][3],
            m[2][0] * p.x + m[2][1] * p.y + m[2][2] * p.z + m[2][3],
            m[3][0] * p.x + m[3][1] * p.y + m[3][2] * p.z + m[3][3],
        )
    }

    pub fn translation(t: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[0][3] = t.x;
        m.0[1][3] = t.y;
        m.0[2][3] = t.z;
        m
    }

    /// A right-handed view matrix looking from `eye` at `target`.
    ///
    /// View space is the OpenGL convention: +X right, +Y up, and the camera
    /// looking down **-Z**. The projection below assumes it, which is why the
    /// forward basis vector is negated here rather than there.
    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
        let f = (target - eye).normalized();
        let mut s = f.cross(up).normalized();
        // A camera looking straight down has f parallel to up, and the cross
        // product collapses. Falling back to a fixed right vector keeps the
        // view defined instead of producing a matrix full of NaN.
        if s.length() <= f32::EPSILON {
            s = vec3(1.0, 0.0, 0.0);
        }
        let u = s.cross(f);
        Mat4([
            [s.x, s.y, s.z, -s.dot(eye)],
            [u.x, u.y, u.z, -u.dot(eye)],
            [-f.x, -f.y, -f.z, f.dot(eye)],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    /// A right-handed perspective projection mapping the frustum to the
    /// `[-1, 1]` cube, `fov_y` in radians.
    pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
        let f = 1.0 / (fov_y * 0.5).tan();
        let mut m = Mat4([[0.0; 4]; 4]);
        m.0[0][0] = f / aspect;
        m.0[1][1] = f;
        m.0[2][2] = (far + near) / (near - far);
        m.0[2][3] = 2.0 * far * near / (near - far);
        m.0[3][2] = -1.0;
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_a_left_and_right_unit() {
        let m = Mat4::translation(vec3(1.0, 2.0, 3.0));
        assert_eq!(Mat4::IDENTITY.mul(&m), m);
        assert_eq!(m.mul(&Mat4::IDENTITY), m);
    }

    /// The composition order the whole renderer relies on: applying `a.mul(&b)`
    /// must be the same as applying `b` and then `a`.
    #[test]
    fn multiplication_composes_right_to_left() {
        let t = Mat4::translation(vec3(1.0, 0.0, 0.0));
        let p = Mat4::perspective(1.0, 1.0, 0.1, 10.0);
        let point = vec3(0.0, 0.0, -1.0);
        let composed = p.mul(&t).transform_point(point);
        let stepwise = {
            let v = t.transform_point(point);
            p.transform_point(vec3(v.x, v.y, v.z))
        };
        assert!((composed.x - stepwise.x).abs() < 1e-5);
        assert!((composed.z - stepwise.z).abs() < 1e-5);
    }

    /// The camera looks down -Z in view space. If this flips, everything is
    /// drawn behind the viewer and the screen is empty.
    #[test]
    fn look_at_puts_the_target_on_negative_z() {
        let m = Mat4::look_at(vec3(0.0, 0.0, 5.0), Vec3::ZERO, vec3(0.0, 1.0, 0.0));
        let v = m.transform_point(Vec3::ZERO);
        assert!((v.x).abs() < 1e-5 && (v.y).abs() < 1e-5);
        assert!((v.z + 5.0).abs() < 1e-5, "expected z = -5, got {}", v.z);
    }

    #[test]
    fn look_at_survives_a_camera_directly_overhead() {
        let m = Mat4::look_at(vec3(0.0, 5.0, 0.0), Vec3::ZERO, vec3(0.0, 1.0, 0.0));
        let v = m.transform_point(Vec3::ZERO);
        assert!(v.z.is_finite() && (v.z + 5.0).abs() < 1e-5);
    }

    /// The near plane must land on ndc z = -1 and the far plane on +1; a sign
    /// slip here inverts the depth test and draws the model inside out.
    #[test]
    fn perspective_maps_near_and_far_to_the_ndc_range() {
        let p = Mat4::perspective(std::f32::consts::FRAC_PI_2, 1.0, 1.0, 100.0);
        let near = p.transform_point(vec3(0.0, 0.0, -1.0));
        let far = p.transform_point(vec3(0.0, 0.0, -100.0));
        assert!((near.z / near.w + 1.0).abs() < 1e-4, "{}", near.z / near.w);
        assert!((far.z / far.w - 1.0).abs() < 1e-4, "{}", far.z / far.w);
    }

    #[test]
    fn a_point_behind_the_camera_has_a_negative_w() {
        let p = Mat4::perspective(1.0, 1.0, 0.1, 10.0);
        assert!(p.transform_point(vec3(0.0, 0.0, 1.0)).w < 0.0);
    }

    #[test]
    fn normalizing_a_zero_vector_does_not_produce_nan() {
        assert_eq!(Vec3::ZERO.normalized(), Vec3::ZERO);
    }
}
