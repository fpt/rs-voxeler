//! Drawing one frame of the editor: backdrop, ground grid, model, gizmos.

use voxel_core::Span;
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
        draw_ground(fb, &scene, editor, size, offset);
    }

    let palette = editor.model().palette().clone();
    let mesh = editor.mesh().clone();
    raster::draw_mesh(fb, &scene, &mesh, &palette, offset);

    draw_volume_box(fb, &scene, editor, size, offset);

    if let Some(target) = hover {
        draw_target(fb, &scene, editor, target, offset);
    }
}

/// How many voxels between grid lines, and between the brighter ones.
///
/// Not every voxel. A line per cell is 65 lines each way on a 64³ volume, which
/// at any framing that fits the model is closer to a grey haze than to a grid —
/// and a grid you cannot count is not doing its job. Every fourth line, picked
/// out every sixteenth, gives roughly a dozen lines per axis at either size and
/// keeps both steps powers of two, so they line up with the volume's own edges.
const GRID_STEP: u16 = 4;
const GRID_COARSE_STEP: u16 = 16;

/// The work plane, drawn as a grid through the middle of the volume.
///
/// At `Editor::ground_y` rather than at the bottom of the box: the volume is
/// centred on the world origin, and so is the plane you build against.
fn draw_ground(fb: &mut Framebuffer, scene: &Scene, editor: &Editor, size: [u16; 3], offset: Vec3) {
    let y = offset.y + editor.ground_y() as f32 - LIFT;
    let (sx, sz) = (size[0] as f32, size[2] as f32);

    // The last line is the volume's own edge, so it is drawn whether or not the
    // step happens to divide the size -- a 30-wide model still gets a border.
    let lines = |n: u16| {
        (0..=n)
            .step_by(GRID_STEP as usize)
            .chain(std::iter::once(n))
            .map(move |i| (i, if i % GRID_COARSE_STEP == 0 || i == n { GRID_COARSE } else { GRID_FINE }))
    };

    for (i, color) in lines(size[0]) {
        let x = offset.x + i as f32;
        raster::draw_line(
            fb,
            scene,
            Vec3 { x, y, z: offset.z },
            Vec3 { x, y, z: offset.z + sz },
            color,
            0.0,
        );
    }
    for (i, color) in lines(size[2]) {
        let z = offset.z + i as f32;
        raster::draw_line(
            fb,
            scene,
            Vec3 { x: offset.x, y, z },
            Vec3 { x: offset.x + sx, y, z },
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
    let (fill, outline) = match editor.tool {
        Tool::Build => {
            let c = editor.model().palette().get(editor.color);
            (c.to_u32(), c.scaled(1.6).to_u32())
        }
        Tool::Erase => (ERASE_MARK, ERASE_MARK),
        Tool::Paint => {
            let c = editor.model().palette().get(editor.color);
            (c.to_u32(), 0xFFFFFF)
        }
        Tool::Pick => (0x000000, 0xFFFFFF),
    };

    // The face is only filled for tools that act on an existing surface. Build
    // shows an outline in the empty cell instead — that is where the voxel
    // lands — and the ground plane has no surface to shade at all.
    if editor.tool != Tool::Build && !target.is_ground() {
        let normal = target.face.normal();
        let lift = Vec3 {
            x: normal[0] as f32 * LIFT,
            y: normal[1] as f32 * LIFT,
            z: normal[2] as f32 * LIFT,
        };
        let face: Vec<Vec3> = face_corners(target.voxel, target.face, offset)
            .iter()
            .map(|c| *c + lift)
            .collect();
        raster::fill_polygon(fb, scene, &face, fill, 0.0);
    }

    // The cell the tool writes to, grown to the brush's extent. A region span
    // is left as the single seed cell: outlining a flood fill would mean
    // running it on every pointer move, and drawing a thousand boxes for the
    // answer. The span's name in the tool row is the honest signal there, and
    // the voxel count in the status line is the confirmation afterwards.
    let r = if editor.span == Span::Voxel {
        editor.brush.radius as f32
    } else {
        0.0
    };
    let [x, y, z] = target.cell;
    let min = Vec3 {
        x: offset.x + x as f32 - r,
        y: offset.y + y as f32 - r,
        z: offset.z + z as f32 - r,
    } - Vec3::splat(LIFT);
    // A box even for a ball brush: it is the extent that matters when you are
    // aiming, and an outline traced around a sphere of voxels is a thicket.
    let max = min + Vec3::splat(1.0 + r * 2.0 + LIFT * 2.0);
    raster::draw_box(fb, scene, min, max, outline, 0.0);
}
