//! Solids on a grid: which cells a sphere or a cylinder covers.
//!
//! Separate from [`region`](crate::region) because the two answer different
//! questions. A region is *found* — it grows from a cell the user pointed at,
//! over whatever is already there. A shape is *stated*: a centre, a radius, and
//! no reference to the model at all. One is a selection, the other a stamp.
//!
//! # One rule for "radius"
//!
//! Distance is measured to the far side of the centre cell — `(r + ½)²` rather
//! than `r²` — so a radius of 1 is three cells across and keeps the twelve edge
//! neighbours, where `r²` would draw a plus sign. That is not a detail to
//! restate: [`Brush`](crate::region::Brush) asks [`in_ball`] too, so the ball
//! brush and `put_sphere` mean the same thing by the same word.

/// Whether a cell that far from the centre is inside a ball of `radius`.
///
/// Takes as many offsets as the caller has axes, so a cylinder can ask the same
/// question about its two cross-section axes that a sphere asks about three.
pub fn in_ball(offsets: &[i32], radius: u16) -> bool {
    let r = radius as f32 + 0.5;
    let d: i32 = offsets.iter().map(|d| d * d).sum();
    d as f32 <= r * r
}

/// Which axis a cylinder runs along, and how the other two are ordered.
fn cross(axis: usize) -> (usize, usize) {
    ((axis + 1) % 3, (axis + 2) % 3)
}

/// Every cell of a filled ball centred on `center`.
///
/// A radius of 0 is the single centre cell, matching the brush, so "no radius"
/// and "one voxel" are the same statement rather than two.
pub fn sphere(center: [i32; 3], radius: u16) -> Vec<[i32; 3]> {
    let r = radius as i32;
    let mut out = Vec::new();
    for dz in -r..=r {
        for dy in -r..=r {
            for dx in -r..=r {
                if in_ball(&[dx, dy, dz], radius) {
                    out.push([center[0] + dx, center[1] + dy, center[2] + dz]);
                }
            }
        }
    }
    out
}

/// Every cell of a filled cylinder whose end-cap centres are `from` and `to`.
///
/// The two ends must differ on at most one axis; that axis is the cylinder's,
/// and its length is however many cells lie between them inclusive. Ends rather
/// than a centre and a height because an even height has no centre cell, and
/// "from here to there" is what a caller placing a trunk or a pillar already
/// knows. `from == to` is a disc one cell thick, which is a real thing to want.
///
/// Returns `None` when the ends differ on more than one axis — this draws
/// axis-aligned cylinders, and quietly projecting a diagonal onto an axis would
/// put the shape somewhere nobody asked for.
pub fn cylinder(from: [i32; 3], to: [i32; 3], radius: u16) -> Option<Vec<[i32; 3]>> {
    let differing: Vec<usize> = (0..3).filter(|a| from[*a] != to[*a]).collect();
    let axis = match differing.as_slice() {
        [] => 1, // a disc: any axis gives the same cell, so pick the up one
        [a] => *a,
        _ => return None,
    };
    let (b, c) = cross(axis);
    let (lo, hi) = (from[axis].min(to[axis]), from[axis].max(to[axis]));
    let r = radius as i32;

    let mut out = Vec::new();
    for along in lo..=hi {
        for dc in -r..=r {
            for db in -r..=r {
                if !in_ball(&[db, dc], radius) {
                    continue;
                }
                let mut cell = [0i32; 3];
                cell[axis] = along;
                cell[b] = from[b] + db;
                cell[c] = from[c] + dc;
                out.push(cell);
            }
        }
    }
    Some(out)
}

/// The surface of a solid: every cell with a face neighbour outside it.
///
/// Face neighbours only, never diagonal — a shell that kept only corner-touching
/// cells would have holes a ray could pass through, which is the whole thing a
/// shell is for.
pub fn shell(cells: &[[i32; 3]]) -> Vec<[i32; 3]> {
    let solid: std::collections::HashSet<[i32; 3]> = cells.iter().copied().collect();
    cells
        .iter()
        .copied()
        .filter(|c| {
            (0..3).any(|a| {
                [-1, 1].iter().any(|d| {
                    let mut n = *c;
                    n[a] += d;
                    !solid.contains(&n)
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(cells: &[[i32; 3]]) -> usize {
        let unique: std::collections::HashSet<_> = cells.iter().collect();
        assert_eq!(unique.len(), cells.len(), "a shape must not repeat a cell");
        cells.len()
    }

    /// The rule the ball brush uses, stated once and shared — `r²` would draw a
    /// plus sign at radius 1, which is not what anyone means by a sphere.
    #[test]
    fn a_radius_is_measured_to_the_far_side_of_the_centre_cell() {
        assert!(in_ball(&[1, 1, 0], 1), "an edge neighbour is in");
        assert!(!in_ball(&[1, 1, 1], 1), "a corner is not");
        assert!(in_ball(&[0, 0, 0], 0), "and a radius of 0 is one cell");
        assert!(!in_ball(&[1, 0, 0], 0));
    }

    #[test]
    fn a_sphere_of_radius_zero_is_the_single_centre_cell() {
        assert_eq!(sphere([4, 5, 6], 0), vec![[4, 5, 6]]);
    }

    #[test]
    fn a_sphere_is_a_cube_with_its_corners_taken_off() {
        let s = sphere([0, 0, 0], 1);
        assert_eq!(count(&s), 19, "27 less the eight corners");
        assert!(!s.contains(&[1, 1, 1]));
        assert!(s.contains(&[1, 1, 0]));
    }

    /// Symmetric about its centre on every axis, or the shape drifts when it is
    /// placed and the caller has to know which way.
    #[test]
    fn a_sphere_is_symmetric_about_its_centre() {
        let c = [10, 20, 30];
        let s = sphere(c, 3);
        for cell in &s {
            let mirrored = [
                2 * c[0] - cell[0],
                2 * c[1] - cell[1],
                2 * c[2] - cell[2],
            ];
            assert!(s.contains(&mirrored), "{cell:?} has no opposite");
        }
        assert_eq!(count(&s), s.len());
    }

    #[test]
    fn a_cylinder_runs_between_its_end_caps() {
        let c = cylinder([8, 0, 8], [8, 9, 8], 2).unwrap();
        let slice: Vec<_> = c.iter().filter(|p| p[1] == 0).collect();
        assert_eq!(c.len(), slice.len() * 10, "one disc per cell of height");
        assert!(c.iter().all(|p| (0..=9).contains(&p[1])));
        assert!(c.contains(&[8, 5, 8]) && c.contains(&[10, 5, 8]));
        assert!(!c.contains(&[11, 5, 8]), "past the radius");
    }

    /// The axis comes from the ends, so the same call draws along X or Z
    /// without a separate parameter to keep in agreement.
    #[test]
    fn a_cylinder_takes_its_axis_from_the_ends_it_was_given() {
        for axis in 0..3 {
            let mut to = [5, 5, 5];
            to[axis] = 11;
            let c = cylinder([5, 5, 5], to, 1).unwrap();
            assert!(c.iter().all(|p| (5..=11).contains(&p[axis])), "axis {axis}");
            let slice: Vec<_> = c.iter().filter(|p| p[axis] == 5).collect();
            // Nine, not five: in two dimensions a corner is only √2 away, which
            // the same `(r + ½)²` rule keeps. The smallest disc is a 3x3, and
            // has nowhere else to go — a plus would be too thin to be a
            // cylinder of any radius.
            assert_eq!(slice.len(), 9, "the smallest disc is three across");
            assert_eq!(c.len(), 9 * 7);
        }
    }

    #[test]
    fn ends_that_are_the_same_cell_give_a_disc_one_thick() {
        let c = cylinder([4, 4, 4], [4, 4, 4], 2).unwrap();
        assert!(c.iter().all(|p| p[1] == 4));
        assert!(c.len() > 1);
    }

    /// A diagonal is refused rather than projected onto an axis, which would
    /// put the shape somewhere nobody asked for.
    #[test]
    fn a_cylinder_between_two_axes_is_refused() {
        assert!(cylinder([0, 0, 0], [5, 5, 0], 1).is_none());
        assert!(cylinder([0, 0, 0], [1, 2, 3], 1).is_none());
    }

    /// A shell has to be watertight: a cell kept only because a *corner*
    /// neighbour was outside would leave holes a ray passes straight through.
    #[test]
    fn a_shell_is_the_surface_and_is_watertight() {
        let solid = sphere([0, 0, 0], 4);
        let shell = shell(&solid);
        assert!(shell.len() < solid.len(), "something was hollowed out");
        assert!(!shell.contains(&[0, 0, 0]), "and the middle went");

        // Every cell of the solid is either in the shell or has all six
        // neighbours in the solid — which is what "watertight" means here.
        let inside: std::collections::HashSet<_> = solid.iter().copied().collect();
        let surface: std::collections::HashSet<_> = shell.iter().copied().collect();
        for cell in &solid {
            if surface.contains(cell) {
                continue;
            }
            for a in 0..3 {
                for d in [-1, 1] {
                    let mut n = *cell;
                    n[a] += d;
                    assert!(inside.contains(&n), "{cell:?} is interior but exposed at {n:?}");
                }
            }
        }
    }

    #[test]
    fn a_shell_of_a_solid_with_no_interior_is_all_of_it() {
        let thin = cylinder([0, 0, 0], [0, 5, 0], 0).unwrap();
        assert_eq!(shell(&thin).len(), thin.len());
    }
}
