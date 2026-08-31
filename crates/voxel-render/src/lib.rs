//! `voxel-render` — a software rasterizer for voxel models.
//!
//! The pipeline is the short one:
//!
//! ```text
//! model -> visible faces -> transform -> clip -> project -> triangles
//!       -> z-buffer -> framebuffer
//! ```
//!
//! # Why faces rather than rays
//!
//! Turning voxels into quads and running an ordinary rasterizer costs work
//! proportional to the model's *surface*, while ray casting costs work
//! proportional to the *screen*. For an editor showing one 64³ model in a
//! window, the surface is the smaller of the two by a wide margin, and the
//! result is a renderer that stays interactive at a real window size on a CPU.
//! It is also the shape a GPU wants later, if this is ever ported.
//!
//! Only the faces that touch air are emitted, so a solid 64³ block draws its
//! 24 576 outer quads rather than 1.5 million. Adjacent coplanar faces are not
//! yet merged — greedy meshing is a real win, but it is a later one, and it
//! would obscure the part of this that has to be right first.
//!
//! # Floats live here
//!
//! This crate uses `f32` freely. The plan keeps the eventual VM boundary in
//! integers, but that boundary is at the *edge* of the engine; carrying a
//! fixed-point constraint through a projection matrix would buy nothing and
//! cost accuracy.

pub mod camera;
pub mod framebuffer;
pub mod math;
pub mod mesh;
pub mod overlay;
pub mod png;
pub mod raster;

pub use camera::OrbitCamera;
pub use framebuffer::Framebuffer;
pub use math::{Mat4, Vec3, Vec4};
pub use mesh::{FaceMesh, FaceQuad};
pub use raster::{Light, Scene};
