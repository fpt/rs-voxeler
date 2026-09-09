//! `.3mf` — 3D Manufacturing Format, for getting a model printed.
//!
//! The format a slicer actually wants. Unlike STL it carries units, colour and
//! several named objects; unlike `.vox` it has no 256-cell ceiling; and unlike
//! OBJ it is what Bambu Studio, PrusaSlicer and Cura all treat as a project
//! rather than as an import.
//!
//! This writes **Core** only. The Volumetric extension can carry voxels as
//! voxels, which is the obvious home for a model like ours, and support for it
//! is still thin enough that a file using it would not open in the tools people
//! have. Core is a triangulated surface, which every one of them reads.
//!
//! # What the structure buys
//!
//! One 3MF object per object in the scene tree, and a `<build>` that places
//! each. A robot exports as a body, two arms and a sword rather than as one
//! welded lump, which is what makes per-object colour — and an AMS — possible
//! at the other end.
//!
//! Each object is closed on its own: [`surface::parts`] emits a face wherever
//! the neighbour is not in the *same* part, so two parts that touch each get
//! their own wall. Sharing it would leave both open, and an open mesh is the
//! thing a slicer cannot fill.
//!
//! # Millimetres
//!
//! 3MF names its unit, so a voxel has to become a length. One voxel is one
//! millimetre by default — a 32³ character is then 32 mm, which is about right
//! for a desk print and easy arithmetic to scale from.

use std::collections::BTreeSet;

use crate::format::surface::{parts, Grouping};
use crate::format::zip::Zip;
use crate::VoxelModel;

const CORE: &str = "http://schemas.microsoft.com/3dmanufacturing/core/2015/02";

/// Write `model` as a 3MF container, `scale` millimetres per voxel.
pub fn export(model: &VoxelModel, scale: f32) -> Vec<u8> {
    let mut zip = Zip::new();
    zip.add("[Content_Types].xml", content_types().as_bytes());
    zip.add("_rels/.rels", rels().as_bytes());
    zip.add("3D/3dmodel.model", model_xml(model, scale).as_bytes());
    zip.finish()
}

fn content_types() -> String {
    concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
        "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
        "<Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\"/>",
        "</Types>\n"
    )
    .to_string()
}

fn rels() -> String {
    concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        "<Relationship Id=\"rel0\" Target=\"/3D/3dmodel.model\" ",
        "Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>",
        "</Relationships>\n"
    )
    .to_string()
}

fn model_xml(model: &VoxelModel, scale: f32) -> String {
    let parts = parts(model, Grouping::Objects);

    // One material per palette index actually used. A slicer shows this list,
    // so shipping the 253 slots nobody drew with would bury the four that were.
    let used: Vec<u8> = parts
        .iter()
        .flat_map(|p| p.colors.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let slot = |index: u8| used.iter().position(|u| *u == index).unwrap_or(0);

    let mut out = String::with_capacity(4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"{CORE}\">\n"
    ));
    out.push_str(" <metadata name=\"Application\">rs-voxeler</metadata>\n");
    out.push_str(" <resources>\n");

    if !used.is_empty() {
        out.push_str("  <basematerials id=\"1\">\n");
        for &index in &used {
            let c = model.palette().get(index);
            out.push_str(&format!(
                "   <base name=\"color_{index}\" displaycolor=\"#{:02X}{:02X}{:02X}FF\"/>\n",
                c.r, c.g, c.b
            ));
        }
        out.push_str("  </basematerials>\n");
    }

    // Ids start at 2: the material group took 1, and they share a namespace.
    for (n, part) in parts.iter().enumerate() {
        out.push_str(&format!(
            "  <object id=\"{}\" type=\"model\" name=\"{}\" pid=\"1\" pindex=\"0\">\n",
            n + 2,
            escape(&part.name)
        ));
        out.push_str("   <mesh>\n    <vertices>\n");
        for v in &part.vertices {
            out.push_str(&format!(
                "     <vertex x=\"{}\" y=\"{}\" z=\"{}\"/>\n",
                number(v[0] as f32 * scale),
                number(v[1] as f32 * scale),
                number(v[2] as f32 * scale)
            ));
        }
        out.push_str("    </vertices>\n    <triangles>\n");
        // 3MF has no quads, so each face becomes two triangles sharing its
        // diagonal. The winding carries over unchanged — counter-clockwise
        // seen from outside is what both call outward, and a reversed one is a
        // solid a slicer fills the wrong side of.
        for ([a, b, c], color) in part.triangles() {
            out.push_str(&format!(
                "     <triangle v1=\"{a}\" v2=\"{b}\" v3=\"{c}\" p1=\"{}\"/>\n",
                slot(color)
            ));
        }
        out.push_str("    </triangles>\n   </mesh>\n  </object>\n");
    }
    out.push_str(" </resources>\n <build>\n");
    for n in 0..parts.len() {
        out.push_str(&format!("  <item objectid=\"{}\"/>\n", n + 2));
    }
    out.push_str(" </build>\n</model>\n");
    out
}

/// A number XML can hold and a slicer can read: no exponent, no trailing noise.
fn number(v: f32) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".into()
    } else {
        s.to_string()
    }
}

/// Escape the five characters XML reserves.
///
/// Layer and object names come from the user, so this is not decoration: a
/// part called `A & B` would otherwise write a file no parser accepts.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull one stored entry back out, the way a reader would.
    fn entry(zip: &[u8], name: &str) -> String {
        let mut at = 0usize;
        while at + 30 <= zip.len() && zip[at..at + 4] == 0x0403_4b50u32.to_le_bytes() {
            let size = u32::from_le_bytes([zip[at + 18], zip[at + 19], zip[at + 20], zip[at + 21]])
                as usize;
            let n = u16::from_le_bytes([zip[at + 26], zip[at + 27]]) as usize;
            let extra = u16::from_le_bytes([zip[at + 28], zip[at + 29]]) as usize;
            let start = at + 30 + n + extra;
            if &zip[at + 30..at + 30 + n] == name.as_bytes() {
                return String::from_utf8_lossy(&zip[start..start + size]).into_owned();
            }
            at = start + size;
        }
        panic!("no entry named {name}");
    }

    fn robot() -> VoxelModel {
        let mut m = VoxelModel::new(16, 16, 16);
        let body = m.add_object(0, "BODY").unwrap();
        let arm = m.add_object(body, "ARM").unwrap();
        let a = m.add_layer(0, "body").unwrap();
        m.set_layer_object(a, body);
        let b = m.add_layer(a, "arm").unwrap();
        m.set_layer_object(b, arm);
        for y in 4..8 {
            m.set_in(a, 5, y, 5, 3);
            m.set_in(b, 7, y, 5, 9);
        }
        m
    }

    /// The three files a 3MF must contain, and the relationship that ties them
    /// together. A reader that cannot find the model part rejects the whole
    /// archive, and the error it gives says nothing useful.
    #[test]
    fn the_container_has_the_parts_a_reader_looks_for() {
        let bytes = export(&robot(), 1.0);
        let types = entry(&bytes, "[Content_Types].xml");
        assert!(types.contains("3dmanufacturing-3dmodel+xml"));
        let rels = entry(&bytes, "_rels/.rels");
        assert!(rels.contains("/3D/3dmodel.model"));
        let model = entry(&bytes, "3D/3dmodel.model");
        assert!(model.contains("unit=\"millimeter\""));
        assert!(model.contains(CORE));
    }

    /// The tree survives as separate objects, and each is placed. A robot that
    /// arrived as one welded lump could not be printed a part at a time or
    /// coloured a part at a time, which is most of why 3MF is here.
    #[test]
    fn each_object_in_the_tree_becomes_an_object_that_is_built() {
        let model = entry(&export(&robot(), 1.0), "3D/3dmodel.model");
        assert_eq!(model.matches("<object id=").count(), 2);
        assert!(model.contains("name=\"SCENE/BODY\""));
        assert!(model.contains("name=\"SCENE/BODY/ARM\""), "{model}");
        // Every object is placed, or it is in the file and not in the print.
        assert_eq!(model.matches("<item objectid=").count(), 2);
        for id in ["2", "3"] {
            assert!(
                model.contains(&format!("<item objectid=\"{id}\"/>")),
                "{id}"
            );
        }
    }

    /// Colours become materials a slicer can show, and only the ones used.
    #[test]
    fn colours_become_materials_and_triangles_point_at_them() {
        let model = entry(&export(&robot(), 1.0), "3D/3dmodel.model");
        assert_eq!(model.matches("<base name=").count(), 2, "two colours drawn");
        // Every triangle names a material slot that exists.
        for line in model.lines().filter(|l| l.contains("<triangle ")) {
            let p = line
                .split("p1=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert!(p.parse::<usize>().unwrap() < 2, "{line}");
        }
        assert!(model.contains("<triangle "), "there are triangles at all");
    }

    /// Millimetres, and the scale reaches the coordinates. A model that
    /// exported at the wrong size prints at the wrong size and nothing says so.
    #[test]
    fn the_scale_reaches_the_vertices() {
        let one = entry(&export(&robot(), 1.0), "3D/3dmodel.model");
        let ten = entry(&export(&robot(), 10.0), "3D/3dmodel.model");
        assert!(one.contains("x=\"5\""), "{one}");
        assert!(ten.contains("x=\"50\""), "at ten millimetres a voxel");
        // No exponent notation: `1e2` is a number Rust prints happily and some
        // slicers read as zero. Checked on the values, not on the whole file,
        // which is full of the letter otherwise.
        for line in ten.lines().filter(|l| l.contains("<vertex ")) {
            for value in line.split('"').skip(1).step_by(2) {
                assert!(
                    value
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.' || c == '-'),
                    "{value:?} in {line}"
                );
            }
        }
    }

    /// Watertight, checked by a property rather than by a count.
    ///
    /// A closed surface of genus zero satisfies V - E + F = 2, and a triangle
    /// mesh where every edge is shared has E = 3F/2. A mesh with a hole, a
    /// duplicated vertex or a missing face fails it — where a face count only
    /// says the number is the one somebody expected. This is the property a
    /// slicer actually depends on: an open mesh has no inside to fill.
    #[test]
    fn every_exported_object_is_a_closed_surface() {
        let model = entry(&export(&robot(), 1.0), "3D/3dmodel.model");
        let mut checked = 0;
        for chunk in model.split("<object id=").skip(1) {
            let v = chunk.matches("<vertex ").count() as i64;
            let f = chunk.matches("<triangle ").count() as i64;
            assert!(v > 0 && f > 0);
            assert_eq!(f % 2, 0, "quads became pairs of triangles");
            assert_eq!(v - (3 * f / 2) + f, 2, "not a closed surface: V={v} F={f}");
            checked += 1;
        }
        assert_eq!(checked, 2, "both parts were checked");
    }

    /// Names come from the user, so a part called `A & B` must not write a file
    /// no parser will accept.
    #[test]
    fn a_name_with_xml_in_it_is_escaped() {
        let mut m = VoxelModel::new(8, 8, 8);
        let o = m.add_object(0, "A & <B>").unwrap();
        let l = m.add_layer(0, "x").unwrap();
        m.set_layer_object(l, o);
        m.set_in(l, 2, 2, 2, 1);
        let model = entry(&export(&m, 1.0), "3D/3dmodel.model");
        assert!(model.contains("A &amp; &lt;B&gt;"), "{model}");
        assert!(!model.contains("A & <B>"));
    }
}
