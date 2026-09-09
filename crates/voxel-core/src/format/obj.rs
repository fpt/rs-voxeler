//! `.obj` — Wavefront, for taking a model somewhere it can be edited.
//!
//! Old, and still the right answer for one job. OBJ keeps **quads**, which is
//! what a voxel surface is made of: exporting through a format that only has
//! triangles hands a modeller twice the faces and a diagonal through every one
//! of them. It is also read by everything, which for an interchange format is
//! most of the argument.
//!
//! Y is up in both, so unlike `.vox` there is no rotation here — and therefore
//! no chance of the mirroring that a swap instead of a rotation would cause.
//!
//! # Colour
//!
//! OBJ has no colour of its own; it names materials in a sibling `.mtl`. One
//! material per palette index actually used, so a model of three colours does
//! not ship two hundred and fifty three empty ones, and faces are written in
//! material order so each `usemtl` is said once rather than per face.

use crate::format::surface::{parts, Grouping, Part};
use crate::VoxelModel;

/// The two files an OBJ export is: the geometry, and the palette beside it.
pub struct Obj {
    pub obj: String,
    pub mtl: String,
}

/// Write `model` as OBJ, one group per object that holds voxels.
///
/// `mtl_name` is the file name the `.obj` will point at — the caller knows
/// where it is putting the pair, and a writer that guessed would be wrong the
/// first time somebody renamed one of them.
pub fn export(model: &VoxelModel, mtl_name: &str, scale: f32) -> Obj {
    let parts = parts(model, Grouping::Objects);
    let mut obj = String::from("# rs-voxeler\n");
    obj.push_str(&format!("mtllib {mtl_name}\n"));

    // OBJ indices are one-based and run across the whole file rather than
    // restarting per group, so parts are offset by everything written before.
    let mut written = 0u32;
    let mut used = std::collections::BTreeSet::new();
    for part in &parts {
        obj.push_str(&format!("o {}\n", sanitize(&part.name)));
        for v in &part.vertices {
            obj.push_str(&format!(
                "v {} {} {}\n",
                v[0] as f32 * scale,
                v[1] as f32 * scale,
                v[2] as f32 * scale
            ));
        }
        write_faces(&mut obj, part, written, &mut used);
        written += part.vertices.len() as u32;
    }

    let mut mtl = String::from("# rs-voxeler\n");
    for index in used {
        let c = model.palette().get(index);
        let (r, g, b) = (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
        mtl.push_str(&format!("newmtl {}\n", material(index)));
        mtl.push_str(&format!("Kd {r:.4} {g:.4} {b:.4}\n"));
        // Flat, unlit colour: a voxel model's colour is the model, not a
        // measurement of a surface, and a specular highlight invented here
        // would be one the editor never showed.
        mtl.push_str("Ks 0.0000 0.0000 0.0000\nillum 1\n\n");
    }
    Obj { obj, mtl }
}

fn write_faces(
    out: &mut String,
    part: &Part,
    offset: u32,
    used: &mut std::collections::BTreeSet<u8>,
) {
    // Grouped by material so `usemtl` is written once per colour rather than
    // once per face, which on a model of any size is most of the file.
    let mut by_color: std::collections::BTreeMap<u8, Vec<&[u32; 4]>> = Default::default();
    for (quad, color) in part.quads.iter().zip(&part.colors) {
        by_color.entry(*color).or_default().push(quad);
    }
    for (color, quads) in by_color {
        used.insert(color);
        out.push_str(&format!("usemtl {}\n", material(color)));
        for q in quads {
            out.push_str(&format!(
                "f {} {} {} {}\n",
                q[0] + offset + 1,
                q[1] + offset + 1,
                q[2] + offset + 1,
                q[3] + offset + 1
            ));
        }
    }
}

fn material(index: u8) -> String {
    format!("color_{index}")
}

/// A name OBJ can hold: it has no quoting, so a space would split the token.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        "part".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_voxel() -> VoxelModel {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(2, 3, 4, 7);
        m
    }

    /// Quads, not triangles. That is the whole reason OBJ is here: a modeller
    /// handed triangles has twice the faces and a diagonal through each of
    /// them, and cannot get the quads back.
    #[test]
    fn faces_are_quads_and_indices_are_one_based() {
        let out = export(&one_voxel(), "m.mtl", 1.0);
        let faces: Vec<&str> = out.obj.lines().filter(|l| l.starts_with("f ")).collect();
        assert_eq!(faces.len(), 6);
        for f in &faces {
            let n: Vec<u32> = f[2..].split(' ').map(|v| v.parse().unwrap()).collect();
            assert_eq!(n.len(), 4, "a quad: {f}");
            // One-based, and nothing may point past the vertices written.
            assert!(n.iter().all(|i| *i >= 1 && *i <= 8), "{f}");
        }
        assert_eq!(out.obj.lines().filter(|l| l.starts_with("v ")).count(), 8);
    }

    /// Y is up in both, so a voxel at (2,3,4) is at (2,3,4). `.vox` needs a
    /// rotation and gets mirrored when someone writes a swap instead; there is
    /// no such trap here, and this is what says so.
    #[test]
    fn coordinates_pass_through_unrotated() {
        let out = export(&one_voxel(), "m.mtl", 1.0);
        let vs: Vec<&str> = out.obj.lines().filter(|l| l.starts_with("v ")).collect();
        assert!(vs.contains(&"v 2 3 4"), "{vs:?}");
        assert!(vs.contains(&"v 3 4 5"), "the far corner: {vs:?}");

        // And the scale multiplies, rather than being ignored.
        let out = export(&one_voxel(), "m.mtl", 2.5);
        let vs: Vec<&str> = out.obj.lines().filter(|l| l.starts_with("v ")).collect();
        assert!(vs.contains(&"v 5 7.5 10"), "{vs:?}");
    }

    /// One material per colour actually used, said once rather than per face.
    #[test]
    fn materials_cover_the_colours_used_and_no_others() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 7);
        m.set(3, 1, 1, 9);
        let out = export(&m, "m.mtl", 1.0);

        let names: Vec<&str> = out
            .mtl
            .lines()
            .filter_map(|l| l.strip_prefix("newmtl "))
            .collect();
        assert_eq!(names, vec!["color_7", "color_9"], "only what was drawn");
        assert!(out.obj.contains("mtllib m.mtl"));
        // Twelve faces, two materials, two `usemtl` lines.
        assert_eq!(out.obj.matches("usemtl ").count(), 2);
    }

    /// A name with a space in it would split into two tokens and the file would
    /// not parse. Layer names come from the user, so this is not decoration.
    #[test]
    fn a_name_with_a_space_does_not_split_the_line() {
        let mut m = VoxelModel::new(8, 8, 8);
        let arm = m.add_object(0, "ARM L").unwrap();
        let layer = m.add_layer(0, "x").unwrap();
        m.set_layer_object(layer, arm);
        m.set_in(layer, 2, 2, 2, 4);
        let out = export(&m, "m.mtl", 1.0);
        let o: Vec<&str> = out.obj.lines().filter(|l| l.starts_with("o ")).collect();
        assert!(o.iter().all(|l| l.split(' ').count() == 2), "{o:?}");
        assert!(o.iter().any(|l| l.contains("ARM_L")), "{o:?}");
    }
}
