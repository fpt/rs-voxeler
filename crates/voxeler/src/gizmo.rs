//! Handles you can grab, for the operations the keyboard already has.
//!
//! The arrow keys move a selection one cell a press. That is the right floor
//! and the wrong ceiling: the thing you are moving is on screen with a box
//! already drawn round it, and dragging it is what a hand expects.
//!
//! The one structural rule here is the one [`hud::panel_rows`] follows, for the
//! same reason. The panel's hit test used to be a hand-written *inverse* of its
//! layout — correct, tested row by row, and a standing invitation to drift. So
//! [`handles`] is built once and **both** the drawing and the hit test index
//! into it. They cannot disagree about where a handle is, because there is one
//! answer to that question.
//!
//! [`hud::panel_rows`]: crate::hud::panel_rows

use voxel_render::math::{Mat4, Vec3};

use crate::editor::Editor;

/// One draggable axis arrow, in world coordinates.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Handle {
    /// 0, 1 or 2 — the axis this arrow moves along.
    pub axis: usize,
    /// The anchor it grows from, and its tip.
    pub from: Vec3,
    pub to: Vec3,
    /// How many voxels long it is, which is what turns a screen-space drag
    /// back into a number of cells.
    pub voxels: f32,
}

/// How close to an arrow a click counts, in framebuffer pixels.
///
/// Generous, because an arrow is a one-pixel line and nobody can hit one. The
/// panel's swatches get the same treatment by being rectangles.
const GRAB_PIXELS: f32 = 7.0;

/// The handles for whatever is selected, or none.
///
/// Anchored at the **low corner** of the selection's box rather than its
/// middle. Two reasons: it is where the outline already is, so the gizmo reads
/// as attached to the thing it moves; and the middle of a solid part is inside
/// the part, where an arrow is buried and a click on it is ambiguous.
pub fn handles(editor: &Editor) -> Vec<Handle> {
    let Some((lo, hi)) = bounds(editor) else {
        return Vec::new();
    };
    let offset = editor.offset();
    let anchor = Vec3 {
        x: lo[0] as f32,
        y: lo[1] as f32,
        z: lo[2] as f32,
    } + offset;
    // Long enough to grab at any zoom: scaled by how far the camera is, and
    // never shorter than the thing it is attached to, so it stays reachable on
    // a large part as well as visible on a small one.
    let span = (0..3)
        .map(|a| (hi[a] - lo[a] + 1) as f32)
        .fold(0.0, f32::max);
    let voxels = (editor.camera.distance * 0.22).max(span * 0.6).max(3.0);
    (0..3)
        .map(|axis| {
            let mut tip = anchor;
            match axis {
                0 => tip.x += voxels,
                1 => tip.y += voxels,
                _ => tip.z += voxels,
            }
            Handle {
                axis,
                from: anchor,
                to: tip,
                voxels,
            }
        })
        .collect()
}

/// The box the handles hang off: the selected object's, or the selection's.
///
/// The same question the arrow keys ask, and answered by what is *actually*
/// selected rather than by which mode is showing — pressing an arrow and
/// dragging a handle have to move the same thing or one of them is lying.
fn bounds(editor: &Editor) -> Option<([i32; 3], [i32; 3])> {
    editor
        .selected_object_bounds()
        .or_else(|| editor.selection.as_ref().and_then(|s| s.bounds()))
}

/// A handle's endpoints in framebuffer pixels, or `None` behind the camera.
pub fn screen_segment(
    h: &Handle,
    view_proj: &Mat4,
    width: u32,
    height: u32,
) -> Option<((f32, f32), (f32, f32))> {
    Some((
        project(h.from, view_proj, width, height)?,
        project(h.to, view_proj, width, height)?,
    ))
}

fn project(p: Vec3, view_proj: &Mat4, width: u32, height: u32) -> Option<(f32, f32)> {
    let v = view_proj.transform_point(p);
    // Behind the near plane the divide mirrors the point across the screen, so
    // there is no honest answer — the same reason the rasterizer clips first.
    if v.w <= 1e-6 {
        return None;
    }
    let inv = 1.0 / v.w;
    Some((
        (v.x * inv * 0.5 + 0.5) * width as f32,
        (0.5 - v.y * inv * 0.5) * height as f32,
    ))
}

/// Which handle a click lands on, if any.
///
/// Nearest first, so overlapping arrows resolve to the one whose line the
/// pointer is actually closest to rather than to whichever came first.
pub fn hit(
    handles: &[Handle],
    view_proj: &Mat4,
    width: u32,
    height: u32,
    x: f32,
    y: f32,
) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, h) in handles.iter().enumerate() {
        let Some((a, b)) = screen_segment(h, view_proj, width, height) else {
            continue;
        };
        let d = distance_to_segment((x, y), a, b);
        if d <= GRAB_PIXELS && best.is_none_or(|(_, best)| d < best) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

fn distance_to_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-6 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + dx * t, a.1 + dy * t);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// How many whole voxels a pointer movement means, along a handle's axis.
///
/// Measured against the handle's own screen-space length, so it is right at any
/// camera angle and any zoom without a second projection to keep in agreement:
/// the arrow spans `voxels` cells, so a drag of that far along it is that many
/// cells. An arrow pointing almost at the camera is nearly a point on screen,
/// and dragging it would be enormously sensitive — that one returns nothing
/// rather than a wild number.
pub fn voxels_dragged(
    h: &Handle,
    view_proj: &Mat4,
    width: u32,
    height: u32,
    from: (f32, f32),
    to: (f32, f32),
) -> Option<i32> {
    let (a, b) = screen_segment(h, view_proj, width, height)?;
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 8.0 {
        return None;
    }
    let (mx, my) = (to.0 - from.0, to.1 - from.1);
    let along = (mx * dx + my * dy) / len;
    Some((along * h.voxels / len).round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use voxel_core::VoxelModel;

    fn editor_with_selection() -> Editor {
        let mut m = VoxelModel::new(32, 32, 32);
        for x in 8..12 {
            for y in 8..14 {
                for z in 8..11 {
                    m.set(x, y, z, 4);
                }
            }
        }
        let mut e = Editor::new(m, PathBuf::from("t.vxm"));
        e.select_box([8, 8, 8], [11, 13, 10]);
        e
    }

    /// The property the module exists for: what is drawn and what is hit are
    /// the same list. Every handle, asked for at the middle of the line the
    /// renderer would draw, comes back as itself.
    #[test]
    fn every_handle_drawn_is_a_handle_you_can_grab() {
        let e = editor_with_selection();
        let (w, h) = (640, 480);
        let vp = e.view_projection(w, h);
        let arrows = handles(&e);
        assert_eq!(arrows.len(), 3, "one per axis");

        for (i, handle) in arrows.iter().enumerate() {
            let (a, b) = screen_segment(handle, &vp, w, h).expect("on screen");
            // Two thirds along, which is past where the three arrows share an
            // anchor and could resolve to each other.
            let p = (a.0 + (b.0 - a.0) * 0.66, a.1 + (b.1 - a.1) * 0.66);
            assert_eq!(
                hit(&arrows, &vp, w, h, p.0, p.1),
                Some(i),
                "handle {i} on axis {}",
                handle.axis
            );
        }
    }

    #[test]
    fn a_click_away_from_every_handle_grabs_nothing() {
        let e = editor_with_selection();
        let (w, h) = (640, 480);
        let vp = e.view_projection(w, h);
        let arrows = handles(&e);
        assert_eq!(hit(&arrows, &vp, w, h, 2.0, 2.0), None, "a far corner");

        // And with nothing selected there is nothing to grab anywhere.
        let mut bare = editor_with_selection();
        bare.clear_all_selection();
        assert!(handles(&bare).is_empty());
    }

    /// Dragging the length of an arrow moves by the number of voxels it spans,
    /// which is what makes the scale right at any zoom without a second
    /// projection to keep in agreement.
    #[test]
    fn dragging_the_length_of_an_arrow_moves_its_whole_span() {
        let e = editor_with_selection();
        let (w, h) = (640, 480);
        let vp = e.view_projection(w, h);
        for handle in handles(&e) {
            let (a, b) = screen_segment(&handle, &vp, w, h).unwrap();
            let n = voxels_dragged(&handle, &vp, w, h, a, b).expect("a usable arrow");
            assert_eq!(
                n,
                handle.voxels.round() as i32,
                "axis {}: a drag from tail to tip",
                handle.axis
            );
            // Backwards is the same distance the other way.
            let back = voxels_dragged(&handle, &vp, w, h, b, a).unwrap();
            assert_eq!(back, -n, "axis {}", handle.axis);
            // Not moving is not a move.
            assert_eq!(voxels_dragged(&handle, &vp, w, h, a, a), Some(0));
        }
    }

    /// An arrow pointing nearly at the camera is nearly a point on screen, and
    /// a drag along it would be enormously sensitive. It declines instead.
    #[test]
    fn an_arrow_pointing_at_the_camera_declines_the_drag() {
        let mut e = editor_with_selection();
        // Look straight down the y axis, so the y handle collapses.
        e.camera.pitch = std::f32::consts::FRAC_PI_2 - 0.001;
        e.camera.yaw = 0.0;
        let (w, h) = (640, 480);
        let vp = e.view_projection(w, h);
        let arrows = handles(&e);
        let y = &arrows[1];
        let Some((a, b)) = screen_segment(y, &vp, w, h) else {
            return; // behind the camera is also a decline
        };
        let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if len < 8.0 {
            assert_eq!(voxels_dragged(y, &vp, w, h, a, (a.0 + 200.0, a.1)), None);
        }
    }
}
