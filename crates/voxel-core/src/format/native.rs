//! `.vxm` — the editor's own format.
//!
//! ```text
//! "VXM4"                       magic
//! u16 u16 u16                  scene range x, y, z
//! u8                           object count, 1..=MAX_OBJECTS; index 0 is root
//! objects * {
//!   u8                         flags; bit 0 is "visible"
//!   u8                         parent index + 1, or 0 for the root
//!   u8 + bytes                 name length, then UTF-8
//! }
//! u8                           layer count, 1..=MAX_LAYERS
//! u8                           the layer that was being edited
//! layers * {
//!   u8                         flags; bit 0 is "visible"
//!   u8                         the object this layer is part of
//!   u8 + bytes                 name length, then UTF-8
//!   u16 u16 u16                the layer's origin in the scene
//!   u16 u16 u16                the layer's own size
//!   u32                        voxel count
//!   count * { u8 u8 u8 u8 }    x, y, z **within the layer**, palette index
//! }
//! 256 * { u8 u8 u8 }           palette, RGB
//! ```
//!
//! Sparse on disk even though a layer is dense in memory: a model is mostly
//! air, and four bytes per *solid* voxel keeps a typical character under a few
//! tens of KiB.
//!
//! # The scene is a range; the layers are the grids
//!
//! The header's size says where voxels may go, not how much was allocated —
//! that is each layer's own origin and size, and storing them is what lets a
//! 64x64x5 ground and a 16³ character share a 64³ scene and come back the same
//! shapes rather than as two 64³ grids. Coordinates are relative to the layer's
//! origin, so a one-byte field covers a layer anywhere in the scene.
//!
//! Each layer stores its own voxels rather than the composite, so hiding a
//! layer and saving does not throw away what was under it. Its flags and name
//! ride along, and so does the active layer: reopening puts you back where you
//! left off.
//!
//! # The object tree
//!
//! An object says what a thing *is*; a layer says how pixels combine. The tree
//! is written as a flat table in arena order with a parent index, because that
//! is how it is held in memory and a nested encoding would be a second shape to
//! keep in agreement with the first. `parent + 1` rather than `parent`, so the
//! root's "no parent" is a zero rather than a sentinel that could be mistaken
//! for object 0.
//!
//! A **cycle is refused on the way in**. `reparent_object` cannot make one, but
//! a file is not a caller, and a chain that loops would make the visibility walk
//! and every tree draw run forever.
//!
//! # `VXM3`, `VXM2` and `VXM1`
//!
//! `VXM3` had layers with boxes but no objects. `VXM2` had layers but no boxes —
//! every layer was the size of the scene. `VXM1` had no layers at all. All three
//! still load: an older file arrives as a single unnamed root object holding
//! every layer, and the two oldest are additionally **trimmed** on the way in, so
//! they gain the smaller shape simply by being opened. A format nobody else
//! implements is one we are free to extend; a file already on disk is not free
//! to rewrite itself. That rule has now held four times.

use crate::model::{Bounds, Object, MAX_LAYERS, MAX_OBJECTS};
use crate::palette::{Palette, Rgb8};
use crate::{Result, VoxelError, VoxelModel};

use super::Reader;

const MAGIC: &[u8; 4] = b"VXM4";
const MAGIC_V3: &[u8; 4] = b"VXM3";
const MAGIC_V2: &[u8; 4] = b"VXM2";
const MAGIC_V1: &[u8; 4] = b"VXM1";

/// A name longer than this is truncated on the way out. The field is one byte
/// long, and a layer label nobody can read in the panel is not a name.
const MAX_NAME: usize = 64;

/// A name cut to [`MAX_NAME`] on a **character** boundary, not a byte one: half
/// a multi-byte character would make the file's own name field invalid UTF-8.
fn clip(name: &str) -> String {
    name.chars()
        .scan(0usize, |used, c| {
            *used += c.len_utf8();
            (*used <= MAX_NAME).then_some(c)
        })
        .collect()
}

pub fn encode(model: &VoxelModel) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(MAGIC);
    for d in model.size() {
        out.extend_from_slice(&d.to_le_bytes());
    }
    out.push(model.object_count() as u8);
    for object in model.objects() {
        out.push(u8::from(object.visible));
        // Plus one, so the root's "no parent" is a zero and not an index that
        // happens to point at the root itself.
        out.push(object.parent.map_or(0, |p| p as u8 + 1));
        let name = clip(&object.name);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }

    out.push(model.layer_count() as u8);
    out.push(model.active_layer() as u8);

    for (n, layer) in model.layers().iter().enumerate() {
        out.push(u8::from(layer.visible));
        out.push(layer.object as u8);
        let name = clip(&layer.name);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());

        let b = layer.bounds();
        for d in b.origin.iter().chain(b.size.iter()) {
            out.extend_from_slice(&d.to_le_bytes());
        }

        let filled: Vec<_> = model.iter_filled_in(n).collect();
        out.extend_from_slice(&(filled.len() as u32).to_le_bytes());
        for ([x, y, z], index) in filled {
            // Relative to the layer's origin, so one byte covers a layer
            // wherever in the scene it sits.
            out.extend_from_slice(&[
                (x - b.origin[0]) as u8,
                (y - b.origin[1]) as u8,
                (z - b.origin[2]) as u8,
                index,
            ]);
        }
    }

    for c in model.palette().colors() {
        out.extend_from_slice(&[c.r, c.g, c.b]);
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<VoxelModel> {
    let mut r = Reader::new(bytes);
    let magic = r.take(4)?;
    let version = match magic {
        m if m == MAGIC => 4,
        m if m == MAGIC_V3 => 3,
        m if m == MAGIC_V2 => 2,
        m if m == MAGIC_V1 => 1,
        _ => {
            return Err(VoxelError::Format(
                "not a .vxm file (bad magic); a MagicaVoxel file needs a .vox extension".into(),
            ))
        }
    };
    let size = read_size(&mut r)?;
    let mut model = VoxelModel::new(size[0], size[1], size[2]);

    if version >= 4 {
        let count = r.u8()? as usize;
        if count == 0 || count > MAX_OBJECTS {
            return Err(VoxelError::Format(format!(
                "a scene needs 1..={MAX_OBJECTS} objects, the file claims {count}"
            )));
        }
        let mut objects = Vec::with_capacity(count);
        for _ in 0..count {
            let flags = r.u8()?;
            let parent = r.u8()?;
            let name_len = r.u8()? as usize;
            let name = String::from_utf8_lossy(r.take(name_len)?).into_owned();
            objects.push(Object {
                name,
                visible: flags & 1 != 0,
                parent: (parent > 0).then(|| parent as usize - 1),
            });
        }
        // Refused rather than repaired: a tree whose parents do not resolve, or
        // whose chain loops, is a file this build cannot mean anything by.
        if !model.set_objects(objects) {
            return Err(VoxelError::Format(
                "the file's object tree does not resolve — a parent is missing, \
                 or a chain of them loops"
                    .into(),
            ));
        }
    }

    if version == 1 {
        read_voxels(&mut r, &mut model, 0, [0; 3])?;
    } else {
        let count = r.u8()? as usize;
        let active = r.u8()? as usize;
        if count == 0 || count > MAX_LAYERS {
            return Err(VoxelError::Format(format!(
                "a scene needs 1..={MAX_LAYERS} layers, the file claims {count}"
            )));
        }
        for n in 0..count {
            // The first layer is the one `VoxelModel::new` already made; the
            // rest are added as they are read, so the stack ends up in file
            // order with no separate allocation pass.
            let flags = r.u8()?;
            // Before v4 there were no objects, so every layer is at the root.
            let object = if version >= 4 { r.u8()? as usize } else { 0 };
            let name_len = r.u8()? as usize;
            let name = String::from_utf8_lossy(r.take(name_len)?).into_owned();
            // Before v3 a layer had no box of its own — it was the scene's size
            // — so the origin is zero and the voxels are already scene-relative.
            let bounds = if version >= 3 {
                Bounds::new(
                    [r.u16()?, r.u16()?, r.u16()?],
                    [r.u16()?, r.u16()?, r.u16()?],
                )
            } else {
                Bounds::new([0; 3], size)
            };
            if n > 0 && model.add_layer_with(n - 1, "", bounds).is_none() {
                return Err(VoxelError::Format("too many layers".into()));
            }
            if n == 0 {
                model.set_layer_bounds(0, bounds);
            }
            model.rename_layer(n, name);
            // An object index past the end of the tree puts the layer at the
            // root, which loses a label rather than the work under it.
            model.set_layer_object(n, object.min(model.object_count() - 1));
            read_voxels(&mut r, &mut model, n, bounds.origin)?;
            // Set after the voxels: a hidden layer still has to be written to,
            // and visibility has no bearing on that.
            model.set_layer_visible(n, flags & 1 != 0);
        }
        model.set_active_layer(active);
    }

    // An older file gave every layer the scene's size. Trimming here is what
    // makes opening one enough to gain the smaller shape.
    if version < 3 {
        for n in 0..model.layer_count() {
            model.trim_layer(n);
        }
    }

    // The palette is optional so a hand-written file can omit it.
    if r.remaining() >= 768 {
        let mut colors = [Rgb8::default(); 256];
        for c in colors.iter_mut() {
            *c = Rgb8::new(r.u8()?, r.u8()?, r.u8()?);
        }
        model.set_palette(Palette::from_colors(colors));
    }
    Ok(model)
}

fn read_size(r: &mut Reader) -> Result<[u16; 3]> {
    let size = [r.u16()?, r.u16()?, r.u16()?];
    for d in size {
        if d == 0 || d > crate::MAX_DIM {
            return Err(VoxelError::Format(format!(
                "size {}x{}x{} is out of range 1..={}",
                size[0],
                size[1],
                size[2],
                crate::MAX_DIM
            )));
        }
    }
    Ok(size)
}

fn read_voxels(
    r: &mut Reader,
    model: &mut VoxelModel,
    layer: usize,
    origin: [u16; 3],
) -> Result<()> {
    let count = r.u32()? as usize;
    // Check the count against what is actually left before allocating from it:
    // a corrupt header claiming four billion voxels should be an error, not an
    // out-of-memory abort.
    if r.remaining() < count * 4 {
        return Err(VoxelError::Format(format!(
            "layer {layer} claims {count} voxels but only {} bytes follow",
            r.remaining()
        )));
    }
    for _ in 0..count {
        let (x, y, z, index) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
        // Out-of-scene records are dropped rather than rejected: they can only
        // come from a file whose scene was shrunk by hand, and losing the stray
        // voxels beats refusing to open the model.
        model.set_in(
            layer,
            x as i32 + origin[0] as i32,
            y as i32 + origin[1] as i32,
            z as i32 + origin[2] as i32,
            index,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VoxelModel {
        let mut m = VoxelModel::new(64, 32, 16);
        m.set(0, 0, 0, 1);
        m.set(63, 31, 15, 200);
        m.set(10, 20, 5, 42);
        m.palette_mut().set(42, Rgb8::new(1, 2, 3));
        m
    }

    /// A model whose layers overlap, are named, and are not all shown — every
    /// field the format carries, in one fixture.
    fn layered() -> VoxelModel {
        let mut m = VoxelModel::new(9, 5, 7);
        m.rename_layer(0, "body");
        m.set(1, 1, 1, 3);
        m.set(2, 1, 1, 3);
        let cover = m.add_layer(0, "armour").unwrap();
        m.set_active_layer(cover);
        m.set(1, 1, 1, 8); // over the body
        m.set(4, 2, 3, 8);
        let hidden = m.add_layer(cover, "scaffold").unwrap();
        m.set_active_layer(hidden);
        m.set(8, 4, 6, 200);
        m.set_layer_visible(hidden, false);
        m.set_active_layer(cover);
        m
    }

    #[test]
    fn round_trips_geometry_and_palette() {
        let m = sample();
        assert_eq!(decode(&encode(&m)).unwrap(), m);
    }

    /// Each layer's own grid, not the composite: a layer hidden behind another
    /// has to come back with what was under it intact.
    #[test]
    fn round_trips_the_whole_layer_stack() {
        let m = layered();
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back, m);

        assert_eq!(back.layer_count(), 3);
        assert_eq!(back.layers()[0].name, "body");
        assert_eq!(back.layers()[1].name, "armour");
        assert!(!back.layers()[2].visible, "the hidden layer stayed hidden");
        assert_eq!(back.active_layer(), 1, "and the layer being edited");
        assert_eq!(back.get_in(0, 1, 1, 1), 3, "the covered body survived");
        assert_eq!(back.get(1, 1, 1), 8);
        assert_eq!(back.get(8, 4, 6), 0, "a hidden layer is still not shown");
    }

    /// A hidden layer's voxels are in the file, so saving while it is hidden
    /// does not quietly delete them — the failure this format is shaped to
    /// avoid.
    #[test]
    fn saving_while_a_layer_is_hidden_keeps_its_voxels() {
        let mut m = layered();
        m.set_layer_visible(1, false);
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.get_in(1, 4, 2, 3), 8);
    }

    /// Boxes are the point of v3: a layer has to come back the shape it was
    /// declared, not merely holding the right voxels.
    #[test]
    fn round_trips_each_layers_own_box() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.rename_layer(0, "GROUND");
        m.set_layer_bounds(0, Bounds::new([0, 0, 0], [64, 5, 64]));
        m.set(3, 1, 3, 4);

        let tree = m.add_layer_with(0, "TREE", Bounds::new([20, 5, 10], [16, 32, 16])).unwrap();
        m.set_active_layer(tree);
        m.set(24, 20, 14, 7);

        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.layer_bounds(0), Bounds::new([0, 0, 0], [64, 5, 64]));
        assert_eq!(back.layer_bounds(1), Bounds::new([20, 5, 10], [16, 32, 16]));
        assert_eq!(back.get(24, 20, 14), 7, "in scene coordinates, wherever the box is");
        assert_eq!(back.get(3, 1, 3), 4);
    }

    /// A layer far from the origin stores coordinates relative to its own box,
    /// so one byte covers it wherever in the scene it sits.
    #[test]
    fn a_layer_far_from_the_origin_round_trips() {
        let mut m = VoxelModel::new(256, 256, 256);
        m.set_layer_bounds(0, Bounds::new([250, 250, 250], [6, 6, 6])) ;
        m.set(255, 255, 255, 9);
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.get(255, 255, 255), 9);
        assert_eq!(back.layer_bounds(0), Bounds::new([250, 250, 250], [6, 6, 6]));
    }

    /// An empty layer costs a header and nothing else, and comes back empty
    /// rather than as a scene-sized grid.
    #[test]
    fn an_empty_layer_survives_as_an_empty_box() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.add_layer(0, "SPARE").unwrap();
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.layer_count(), 2);
        assert_eq!(back.layer_bounds(1), Bounds::default());
        assert_eq!(back.allocated_cells(), 0);
    }

    /// The tree is what v4 is for: names, visibility, parents and which object
    /// each layer belongs to all have to come back the same.
    #[test]
    fn round_trips_the_object_tree() {
        let mut m = VoxelModel::new(32, 32, 32);
        let robot = m.add_object(0, "ROBOT").unwrap();
        let arm = m.add_object(robot, "LEFT ARM").unwrap();
        let sword = m.add_object(0, "SWORD").unwrap();
        m.set_object_visible(sword, false);

        m.rename_layer(0, "BODY");
        m.set_layer_object(0, robot);
        m.set(4, 4, 4, 1);
        let skin = m.add_layer(0, "SKIN").unwrap();
        m.set_layer_object(skin, arm);
        m.set_in(skin, 6, 4, 4, 2);
        let blade = m.add_layer(skin, "BLADE").unwrap();
        m.set_layer_object(blade, sword);
        m.set_in(blade, 9, 4, 4, 3);

        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back, m, "the whole document, tree included");
        assert_eq!(back.object_count(), 4);
        assert_eq!(back.objects()[arm].name, "LEFT ARM");
        assert_eq!(back.objects()[arm].parent, Some(robot), "two levels deep");
        assert_eq!(back.objects()[0].parent, None, "and the root has none");
        assert!(!back.objects()[sword].visible);
        assert_eq!(back.layers()[blade].object, sword);
        assert!(
            !back.layers()[blade].shown(),
            "a layer in a hidden object comes back hidden"
        );
        assert_eq!(back.get(9, 4, 4), 0, "so the composite agrees");
        assert_eq!(back.get_in(blade, 9, 4, 4), 3, "and the voxel is still there");
    }

    /// A tree read off a disk is not a tree a caller built. A parent that does
    /// not resolve, or a chain that loops, has to be refused on the way in —
    /// surviving it would mean every walk over the tree needed a depth guard.
    #[test]
    fn an_object_tree_that_loops_or_dangles_is_refused() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.add_object(0, "A").unwrap();
        m.add_object(1, "B").unwrap();
        let good = encode(&m);
        assert!(decode(&good).is_ok());

        // The parent byte of object `i`, one past its flags. Walked rather than
        // counted, because the records carry variable-length names.
        let parent_at = |bytes: &[u8], want: usize| {
            let mut at = 4 + 6 + 1;
            for i in 0..want {
                at += 3 + bytes[at + 2] as usize;
                debug_assert!(i < bytes[10] as usize);
            }
            at + 1
        };

        // A's parent becomes B, and B's parent is already A.
        let mut looped = good.clone();
        looped[parent_at(&good, 1)] = 2 + 1;
        assert!(decode(&looped).is_err(), "a two-object cycle");

        let mut dangling = good.clone();
        dangling[parent_at(&good, 1)] = 200;
        assert!(decode(&dangling).is_err(), "a parent that does not exist");

        // The root claiming a parent is the same fault by another name.
        let mut rooted = good.clone();
        rooted[parent_at(&good, 0)] = 2;
        assert!(decode(&rooted).is_err(), "a root with a parent");

        // An object that is its own parent is the shortest cycle there is.
        let mut itself = good.clone();
        itself[parent_at(&good, 1)] = 1 + 1;
        assert!(decode(&itself).is_err(), "an object parented to itself");
    }

    /// The version before objects. It still loads, and everything in it arrives
    /// at the root — one unnamed object holding the whole stack.
    #[test]
    fn a_vxm3_file_loads_with_every_layer_at_the_root() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VXM3");
        for d in [16u16, 16, 16] {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        bytes.push(2); // layers
        bytes.push(1); // active
        for (name, origin, size, voxel) in [
            ("GROUND", [0u16, 0, 0], [16u16, 2, 16], [3u8, 1, 3, 4]),
            ("TREE", [4, 2, 4], [8, 8, 8], [1, 1, 1, 7]),
        ] {
            bytes.push(1); // visible; v3 had no object byte
            bytes.push(name.len() as u8);
            bytes.extend_from_slice(name.as_bytes());
            for d in origin.iter().chain(size.iter()) {
                bytes.extend_from_slice(&d.to_le_bytes());
            }
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&voxel);
        }

        let m = decode(&bytes).unwrap();
        assert_eq!(m.object_count(), 1, "one root object and nothing else");
        assert_eq!(m.objects()[0].parent, None);
        assert_eq!(m.layer_count(), 2);
        assert!(m.layers().iter().all(|l| l.object == 0));
        assert_eq!(m.layers()[1].name, "TREE");
        assert_eq!(m.layer_bounds(1), Bounds::new([4, 2, 4], [8, 8, 8]));
        assert_eq!(m.get(3, 1, 3), 4, "the ground voxel");
        assert_eq!(m.get(5, 3, 5), 7, "and the tree's, in scene coordinates");
        assert_eq!(m.active_layer(), 1);
    }

    /// The version before boxes: every layer was the scene's size. Opening one
    /// trims it, so an old file gains the smaller shape by being read.
    #[test]
    fn a_vxm2_file_loads_and_is_trimmed_to_its_contents() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VXM2");
        for d in [64u16, 64, 64] {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        bytes.extend_from_slice(&[2, 1]); // two layers, the second active
        for (name, cells) in [
            ("GROUND", vec![[1u8, 0, 1, 4], [3, 0, 3, 4]]),
            ("TREE", vec![[20, 5, 10, 7]]),
        ] {
            bytes.push(1);
            bytes.push(name.len() as u8);
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(cells.len() as u32).to_le_bytes());
            for c in cells {
                bytes.extend_from_slice(&c);
            }
        }
        bytes.extend(std::iter::repeat_n(0u8, 768));

        let m = decode(&bytes).unwrap();
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.layers()[0].name, "GROUND");
        assert_eq!(m.active_layer(), 1);
        assert_eq!(m.get(1, 0, 1), 4);
        assert_eq!(m.get(20, 5, 10), 7);
        // Trimmed on the way in: neither layer is a 64-cubed grid any more.
        assert_eq!(m.layer_bounds(0), Bounds::new([1, 0, 1], [3, 1, 3]));
        assert_eq!(m.layer_bounds(1), Bounds::new([20, 5, 10], [1, 1, 1]));
        assert!(m.allocated_cells() < 100, "was 2 x 262144");
    }

    /// The layerless format that shipped first still opens, as one layer.
    #[test]
    fn a_vxm1_file_loads_as_a_single_layer() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VXM1");
        for d in [8u16, 6, 4] {
            bytes.extend_from_slice(&d.to_le_bytes());
        }
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 9]);
        bytes.extend_from_slice(&[7, 5, 3, 4]);
        bytes.extend(std::iter::repeat_n(0u8, 768));

        let m = decode(&bytes).unwrap();
        assert_eq!(m.size(), [8, 6, 4]);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.get(1, 2, 3), 9);
        assert_eq!(m.get(7, 5, 3), 4);
        assert_eq!(m.filled_count(), 2);
    }

    #[test]
    fn the_maximum_coordinate_survives_the_byte_it_is_stored_in() {
        let mut m = VoxelModel::new(256, 256, 256);
        m.set(255, 255, 255, 7);
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.get(255, 255, 255), 7);
        assert_eq!(back.filled_count(), 1);
    }

    /// The name field is one byte long, and a name is cut on a character
    /// boundary — half a multi-byte character would make the file's own name
    /// field invalid UTF-8.
    #[test]
    fn an_overlong_name_is_truncated_without_splitting_a_character() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.rename_layer(0, "é".repeat(200));
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.layers()[0].name, "é".repeat(MAX_NAME / 2));
    }

    #[test]
    fn a_foreign_file_is_rejected_by_name() {
        let err = decode(b"VOX \x96\x00\x00\x00").unwrap_err().to_string();
        assert!(err.contains(".vxm"), "{err}");
    }

    /// Where the layer count sits: past the magic, the scene size and the
    /// variable-length object table. Re-derived rather than hard-coded, because
    /// an offset written as a number is a test that breaks every time the
    /// header grows and says nothing about why.
    fn layer_count_at(bytes: &[u8]) -> usize {
        let mut at = 4 + 6;
        let objects = bytes[at] as usize;
        at += 1;
        for _ in 0..objects {
            at += 3 + bytes[at + 2] as usize; // flags, parent, name length, name
        }
        at
    }

    /// A count field that outruns the file must be an error, not an attempt to
    /// reserve four gigabytes.
    #[test]
    fn an_absurd_count_is_an_error_not_an_allocation() {
        let mut m = sample();
        m.rename_layer(0, "");
        let mut bytes = encode(&m);
        // Past the layer count and active layer, then the first layer's flags,
        // object, zero name length and twelve bytes of box: the voxel count.
        let at = layer_count_at(&bytes) + 2 + 3 + 12;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_layer_count_of_zero_or_too_many_is_an_error() {
        let mut bytes = encode(&sample());
        let at = layer_count_at(&bytes);
        bytes[at] = 0;
        assert!(decode(&bytes).is_err());
        bytes[at] = MAX_LAYERS as u8 + 1;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn an_object_count_of_zero_or_too_many_is_an_error() {
        let mut bytes = encode(&sample());
        bytes[10] = 0;
        assert!(decode(&bytes).is_err());
        bytes[10] = MAX_OBJECTS as u8 + 1;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_truncated_file_is_an_error() {
        let bytes = encode(&sample());
        assert!(decode(&bytes[..6]).is_err());
        assert!(decode(&bytes[..12]).is_err());
    }

    #[test]
    fn a_file_without_a_palette_loads_with_the_default_one() {
        let m = sample();
        let bytes = encode(&m);
        let trimmed = &bytes[..bytes.len() - 768];
        let back = decode(trimmed).unwrap();
        assert_eq!(back.get(10, 20, 5), 42);
        assert_eq!(*back.palette(), Palette::default());
    }
}
