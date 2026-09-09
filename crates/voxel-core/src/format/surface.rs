//! Turning voxels into the triangles and quads an interchange format wants.
//!
//! The renderer has a face extractor already, and this is deliberately not it.
//! `voxel-render` emits quads to *draw*: it culls back faces, it keeps them per
//! chunk for incremental rebuilds, and it knows nothing about which part a cell
//! belongs to. A file wants the opposite — every face whatever the camera is
//! doing, grouped by the thing it is part of, with shared corners welded so the
//! result is a solid rather than a heap of squares.
//!
//! `voxel-core` also cannot depend on `voxel-render`; the arrow points the
//! other way, and putting the exporters below the renderer is what keeps that
//! true.

use std::collections::BTreeMap;

use crate::VoxelModel;

/// One part of a model, ready to write out.
///
/// Corners are welded: a cube is eight vertices rather than twenty-four, so a
/// slicer sees a closed solid instead of six loose squares. That is not a size
/// optimisation — 3MF and STL both take an unwelded soup happily — it is what
/// makes the result *manifold*, which is the thing a printer needs.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    /// Corner positions in voxels, with the model's own origin.
    pub vertices: Vec<[i32; 3]>,
    /// Four vertex indices, counter-clockwise seen from outside.
    pub quads: Vec<[u32; 4]>,
    /// The palette index each quad takes its colour from.
    pub colors: Vec<u8>,
}

impl Part {
    /// The quads as triangles, for a format that has no quads. Two per quad,
    /// sharing the diagonal, keeping the winding.
    pub fn triangles(&self) -> impl Iterator<Item = ([u32; 3], u8)> + '_ {
        self.quads
            .iter()
            .zip(&self.colors)
            .flat_map(|(q, c)| [([q[0], q[1], q[2]], *c), ([q[0], q[2], q[3]], *c)])
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }
}

/// How to cut a model into parts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Grouping {
    /// Everything as one part.
    #[default]
    Whole,
    /// One part per object that holds voxels, named for its path in the tree.
    ///
    /// Each part is closed on its own: a face is emitted wherever the
    /// neighbouring cell is not in the *same* part, so two parts that touch
    /// each get their own wall. A shared wall would leave both of them open,
    /// and an open mesh is the thing a slicer cannot fill.
    Objects,
}

/// Cut a model into parts, welding corners as it goes.
///
/// Hidden layers are included. A file is the model, not the view — hiding a
/// layer to work on what is under it should not quietly drop it from an export,
/// which is the same rule `format::native` follows when it saves.
pub fn parts(model: &VoxelModel, grouping: Grouping) -> Vec<Part> {
    let groups = match grouping {
        Grouping::Whole => vec![(String::from("model"), None)],
        Grouping::Objects => (0..model.object_count())
            .filter(|o| !model.object_layers(*o).is_empty())
            .map(|o| (object_path(model, o), Some(o)))
            .collect(),
    };
    groups
        .into_iter()
        .map(|(name, object)| build(model, &name, object))
        .filter(|p| !p.is_empty())
        .collect()
}

/// An object's name with its ancestors, so a part is identifiable in a file
/// that has no tree of its own to put it in.
fn object_path(model: &VoxelModel, mut i: usize) -> String {
    let mut parts = Vec::new();
    for _ in 0..model.object_count() {
        let Some(o) = model.objects().get(i) else {
            break;
        };
        parts.push(o.name.clone());
        match o.parent {
            Some(p) => i = p,
            None => break,
        }
    }
    parts.reverse();
    parts.join("/")
}

fn build(model: &VoxelModel, name: &str, object: Option<usize>) -> Part {
    // Which cells belong to this part, and what colour each is. Built once so
    // the neighbour test below is a lookup rather than a walk.
    let mut cells: BTreeMap<[i32; 3], u8> = BTreeMap::new();
    for (n, layer) in model.layers().iter().enumerate() {
        if object.is_some_and(|o| layer.object != o) {
            continue;
        }
        let _ = layer;
        // Bottom of the stack first, so a cell two layers hold ends up the
        // colour of the one on top — the rule `get` follows, arrived at by
        // letting the later insert win rather than by asking who owns it. A
        // shared cell written twice would leave a wall buried inside the solid.
        for (p, v) in model.iter_filled_in(n) {
            cells.insert(p.map(i32::from), v);
        }
    }

    let mut part = Part {
        name: name.to_string(),
        ..Default::default()
    };
    let mut index: BTreeMap<[i32; 3], u32> = BTreeMap::new();
    let mut vertex = |p: [i32; 3], part: &mut Part| -> u32 {
        *index.entry(p).or_insert_with(|| {
            part.vertices.push(p);
            part.vertices.len() as u32 - 1
        })
    };

    for (&[x, y, z], &color) in &cells {
        for axis in 0..3 {
            for positive in [false, true] {
                let mut n = [x, y, z];
                n[axis] += if positive { 1 } else { -1 };
                if cells.contains_key(&n) {
                    continue;
                }
                // The same derivation the renderer uses: for a face on axis
                // `a`, the other two in cyclic order satisfy e_b x e_c = e_a,
                // so base, +b, +b+c, +c winds counter-clockwise about +a and a
                // negative face swaps them. Six hand-written corner lists would
                // be six chances to get one inside out, and an inside-out face
                // is a solid a slicer fills the wrong side of.
                let (b, c) = if positive {
                    ((axis + 1) % 3, (axis + 2) % 3)
                } else {
                    ((axis + 2) % 3, (axis + 1) % 3)
                };
                let mut base = [x, y, z];
                if positive {
                    base[axis] += 1;
                }
                let corner = |sb: i32, sc: i32| {
                    let mut p = base;
                    p[b] += sb;
                    p[c] += sc;
                    p
                };
                let quad = [
                    vertex(corner(0, 0), &mut part),
                    vertex(corner(1, 0), &mut part),
                    vertex(corner(1, 1), &mut part),
                    vertex(corner(0, 1), &mut part),
                ];
                part.quads.push(quad);
                part.colors.push(color);
            }
        }
    }
    part
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(m: &mut VoxelModel, lo: [i32; 3], hi: [i32; 3], color: u8) {
        for x in lo[0]..=hi[0] {
            for y in lo[1]..=hi[1] {
                for z in lo[2]..=hi[2] {
                    m.set(x, y, z, color);
                }
            }
        }
    }

    /// A cube is eight corners and six faces, not twenty-four corners and six
    /// loose squares. Welding is what makes the result manifold, which is the
    /// thing a slicer needs — an unwelded soup renders identically and prints
    /// as nothing.
    #[test]
    fn a_single_voxel_is_a_welded_cube() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(3, 3, 3, 7);
        let parts = parts(&m, Grouping::Whole);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].quads.len(), 6);
        assert_eq!(parts[0].vertices.len(), 8, "corners are shared");
        assert!(parts[0].colors.iter().all(|c| *c == 7));
    }

    /// Interior faces are not emitted, or a solid has walls buried inside it
    /// and its volume is wrong.
    #[test]
    fn a_solid_block_has_only_its_shell() {
        let mut m = VoxelModel::new(16, 16, 16);
        cube(&mut m, [4, 4, 4], [7, 7, 7], 3);
        let parts = parts(&m, Grouping::Whole);
        // 4³ block: six faces of 4x4.
        assert_eq!(parts[0].quads.len(), 6 * 16);
        // A closed surface of quads has every edge shared by exactly two of
        // them. That is the property, rather than a vertex count that would
        // also pass for a shape with a hole in it.
        assert!(closed(&parts[0]), "the shell is not closed");
    }

    /// Every edge used exactly twice, in opposite directions — the definition
    /// of a closed, consistently wound surface.
    fn closed(part: &Part) -> bool {
        let mut edges: std::collections::BTreeMap<(u32, u32), i32> = Default::default();
        for q in &part.quads {
            for i in 0..4 {
                let (a, b) = (q[i], q[(i + 1) % 4]);
                let key = (a.min(b), a.max(b));
                *edges.entry(key).or_insert(0) += if a < b { 1 } else { -1 };
            }
        }
        edges.values().all(|n| *n == 0)
    }

    /// Two parts that touch each need their own wall. Sharing it would leave
    /// both of them open, and an open mesh is what a slicer cannot fill.
    #[test]
    fn touching_parts_are_each_closed_on_their_own() {
        let mut m = VoxelModel::new(16, 16, 16);
        let left = m.add_object(0, "LEFT").unwrap();
        let right = m.add_object(0, "RIGHT").unwrap();
        let a = m.add_layer(0, "L").unwrap();
        m.set_layer_object(a, left);
        let b = m.add_layer(a, "R").unwrap();
        m.set_layer_object(b, right);
        for y in 4..8 {
            for z in 4..8 {
                for x in 4..6 {
                    m.set_in(a, x, y, z, 1);
                }
                for x in 6..8 {
                    m.set_in(b, x, y, z, 2);
                }
            }
        }

        let parts = parts(&m, Grouping::Objects);
        assert_eq!(parts.len(), 2, "one per object that holds voxels");
        for p in &parts {
            assert!(closed(p), "{} is open", p.name);
        }
        // Whole, they share the wall and it is not emitted at all.
        let one = parts_whole(&m);
        assert!(one.quads.len() < parts.iter().map(|p| p.quads.len()).sum::<usize>());
        assert!(closed(&one));
    }

    fn parts_whole(m: &VoxelModel) -> Part {
        parts(m, Grouping::Whole).into_iter().next().unwrap()
    }

    /// A file is the model, not the view. Hiding a layer to work on what is
    /// under it must not quietly drop it from an export — the rule
    /// `format::native` already follows when it saves.
    #[test]
    fn a_hidden_layer_is_still_exported() {
        let mut m = VoxelModel::new(8, 8, 8);
        let top = m.add_layer(0, "TOP").unwrap();
        m.set_in(top, 2, 2, 2, 5);
        let shown = parts_whole(&m).quads.len();
        m.set_layer_visible(top, false);
        assert_eq!(parts_whole(&m).quads.len(), shown, "hiding is not deleting");
    }
}
