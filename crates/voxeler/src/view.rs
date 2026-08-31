//! Drawing one frame of the editor: backdrop, ground grid, model, gizmos.

use voxel_render::raster::{self, Light, Scene};
use voxel_render::{Framebuffer, Vec3};

use crate::editor::{face_corners, Editor, Target, Tool};

const SKY_TOP: u32 = 0x2B3A52;
const SKY_BOTTOM: u32 = 0x141A24;
const GRID_FINE: u32 = 0x2E3648;
const GRID_COARSE: u32 = 0x4A5570;
const VOLUME_BOX: u32 = 0x5C6B8A;
const ERASE_MARK: u32 = 0xFF5A4A;

/// How far a gizmo is nudged towards the outside of the surface it marks.
///
/// A hundredth of a voxel is invisible at any zoom this editor allows, and it
/// removes the coplanar case entirely. The alternative — a depth bias — has to
/// be tuned against the near/far ratio, and a value that works at arm's length
/// fails when you zoom in.
const LIFT: f32 = 0.01;

pub fn render(fb: &mut Framebuffer, editor: &mut Editor, hover: Option<Target>) {
    fb.clear_gradient(SKY_TOP, SKY_BOTTOM);

    let scene = Scene {
        view_proj: editor.view_projection(fb.width(), fb.height()),
        eye: editor.camera.eye(),
        light: Light::default(),
    };
    let offset = editor.offset();
    let size = editor.model().size();

    if editor.show_grid {
        draw_ground(fb, &scene, size, offset);
    }

    let palette = editor.model().palette().clone();
    let mesh = editor.mesh().clone();
    raster::draw_mesh(fb, &scene, &mesh, &palette, offset);

    draw_volume_box(fb, &scene, editor, size, offset);

    if let Some(target) = hover {
        draw_target(fb, &scene, editor, target, offset);
    }
}

/// A grid on the floor of the volume, with every eighth line picked out so the
/// eye can count cells without following each one.
fn draw_ground(fb: &mut Framebuffer, scene: &Scene, size: [u16; 3], offset: Vec3) {
    let y = offset.y - LIFT;
    let (sx, sz) = (size[0] as f32, size[2] as f32);
    for i in 0..=size[0] {
        let x = offset.x + i as f32;
        let color = if i % 8 == 0 { GRID_COARSE } else { GRID_FINE };
        raster::draw_line(
            fb,
            scene,
            Vec3 { x, y, z: offset.z },
            Vec3 {
                x,
                y,
                z: offset.z + sz,
            },
            color,
            0.0,
        );
    }
    for i in 0..=size[2] {
        let z = offset.z + i as f32;
        let color = if i % 8 == 0 { GRID_COARSE } else { GRID_FINE };
        raster::draw_line(
            fb,
            scene,
            Vec3 { x: offset.x, y, z },
            Vec3 {
                x: offset.x + sx,
                y,
                z,
            },
            color,
            0.0,
        );
    }
}

/// The volume's outline, and the slice plane when one is active — without it
/// there is no way to see where the model's buildable space ends.
fn draw_volume_box(
    fb: &mut Framebuffer,
    scene: &Scene,
    editor: &Editor,
    size: [u16; 3],
    offset: Vec3,
) {
    let min = offset - Vec3::splat(LIFT);
    let max = Vec3 {
        x: offset.x + size[0] as f32,
        y: offset.y + size[1] as f32,
        z: offset.z + size[2] as f32,
    } + Vec3::splat(LIFT);
    raster::draw_box(fb, scene, min, max, VOLUME_BOX, 0.0);

    if let Some(cut) = editor.slice {
        let y = offset.y + cut as f32;
        raster::draw_box(
            fb,
            scene,
            Vec3 { x: min.x, y, z: min.z },
            Vec3 { x: max.x, y, z: max.z },
            crate::hud::accent(),
            0.0,
        );
    }
}

/// Mark what a click would do: the face under the cursor, and the cell the tool
/// would write to.
fn draw_target(
    fb: &mut Framebuffer,
    scene: &Scene,
    editor: &Editor,
    target: Target,
    offset: Vec3,
) {
    let normal = target.hit.face.normal();
    let lift = Vec3 {
        x: normal[0] as f32 * LIFT,
        y: normal[1] as f32 * LIFT,
        z: normal[2] as f32 * LIFT,
    };
    let face: Vec<Vec3> = face_corners(target.hit.voxel, target.hit.face, offset)
        .iter()
        .map(|c| *c + lift)
        .collect();

    let (fill, outline) = match editor.tool {
        Tool::Build => {
            let c = editor.model().palette().get(editor.color);
            (c.scaled(1.0).to_u32(), c.scaled(1.6).to_u32())
        }
        Tool::Erase => (ERASE_MARK, ERASE_MARK),
        Tool::Paint => {
            let c = editor.model().palette().get(editor.color);
            (c.to_u32(), 0xFFFFFF)
        }
        Tool::Pick => (0x000000, 0xFFFFFF),
    };

    // The face is only filled for tools that act on the *surface*; a build
    // shows its outline in empty space instead, which is where the voxel lands.
    if editor.tool != Tool::Build {
        raster::fill_polygon(fb, scene, &face, fill, 0.0);
    }

    let [x, y, z] = target.cell;
    let min = Vec3 {
        x: offset.x + x as f32,
        y: offset.y + y as f32,
        z: offset.z + z as f32,
    } - Vec3::splat(LIFT);
    let max = min + Vec3::splat(1.0 + LIFT * 2.0);
    raster::draw_box(fb, scene, min, max, outline, 0.0);
}
