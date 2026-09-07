//! `.vxm` — the editor's own format.
//!
//! ```text
//! "VXM2"                       magic
//! u16 u16 u16                  size x, y, z
//! u8                           layer count, 1..=MAX_LAYERS
//! u8                           the layer that was being edited
//! layers * {
//!   u8                         flags; bit 0 is "visible"
//!   u8 + bytes                 name length, then UTF-8
//!   u32                        voxel count
//!   count * { u8 u8 u8 u8 }    x, y, z, palette index
//! }
//! 256 * { u8 u8 u8 }           palette, RGB
//! ```
//!
//! Sparse on disk even though the grid is dense in memory: a model is mostly
//! air, and four bytes per *solid* voxel keeps a typical 64³ character under a
//! few tens of KiB where the dense form is a flat 256 — per layer, which is
//! what makes a sixteen-layer model a file rather than a download.
//!
//! Each layer stores its own voxels rather than the composite, so hiding a
//! layer and saving does not throw away what was under it. A layer's flags and
//! name ride along with it, and so does the active layer: reopening a model
//! puts you back where you left off.
//!
//! Coordinates are one byte because [`MAX_DIM`](crate::MAX_DIM) is 256, so the
//! largest coordinate is 255. That is the same ceiling `.vox` has, which is not
//! a coincidence — it is where interoperability stops either way.
//!
//! # `VXM1`
//!
//! The original layerless format: the same header without the two layer bytes,
//! then one flat voxel list. Still read, as a single layer named after nothing
//! in particular — a format nobody else implements is one we are free to
//! extend, but a file already on disk is not free to rewrite itself.

use crate::model::MAX_LAYERS;
use crate::palette::{Palette, Rgb8};
use crate::{Result, VoxelError, VoxelModel};

use super::Reader;

const MAGIC: &[u8; 4] = b"VXM2";
const MAGIC_V1: &[u8; 4] = b"VXM1";

/// A name longer than this is truncated on the way out. The field is one byte
/// long, and a layer label nobody can read in the panel is not a name.
const MAX_NAME: usize = 64;

pub fn encode(model: &VoxelModel) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(MAGIC);
    for d in model.size() {
        out.extend_from_slice(&d.to_le_bytes());
    }
    out.push(model.layer_count() as u8);
    out.push(model.active_layer() as u8);

    for (n, layer) in model.layers().iter().enumerate() {
        out.push(u8::from(layer.visible));
        // Truncated on a character boundary, not a byte one: half a multi-byte
        // character would make the file's own name field invalid UTF-8.
        let name: String = layer
            .name
            .chars()
            .scan(0usize, |used, c| {
                *used += c.len_utf8();
                (*used <= MAX_NAME).then_some(c)
            })
            .collect();
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());

        let filled: Vec<_> = model.iter_filled_in(n).collect();
        out.extend_from_slice(&(filled.len() as u32).to_le_bytes());
        for ([x, y, z], index) in filled {
            out.extend_from_slice(&[x as u8, y as u8, z as u8, index]);
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
    let v1 = magic == MAGIC_V1;
    if magic != MAGIC && !v1 {
        return Err(VoxelError::Format(
            "not a .vxm file (bad magic); a MagicaVoxel file needs a .vox extension".into(),
        ));
    }
    let size = read_size(&mut r)?;
    let mut model = VoxelModel::new(size[0], size[1], size[2]);

    if v1 {
        read_voxels(&mut r, &mut model, 0)?;
    } else {
        let count = r.u8()? as usize;
        let active = r.u8()? as usize;
        if count == 0 || count > MAX_LAYERS {
            return Err(VoxelError::Format(format!(
                "a model needs 1..={MAX_LAYERS} layers, the file claims {count}"
            )));
        }
        for n in 0..count {
            // The first layer is the one `VoxelModel::new` already made; the
            // rest are added as they are read, so the stack ends up in file
            // order with no separate allocation pass.
            if n > 0 && model.add_layer(n - 1, "").is_none() {
                return Err(VoxelError::Format("too many layers".into()));
            }
            let flags = r.u8()?;
            let name_len = r.u8()? as usize;
            let name = String::from_utf8_lossy(r.take(name_len)?).into_owned();
            model.rename_layer(n, name);
            read_voxels(&mut r, &mut model, n)?;
            // Set after the voxels, so the one recomposite it costs happens
            // with the layer already filled.
            model.set_layer_visible(n, flags & 1 != 0);
        }
        model.set_active_layer(active);
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

fn read_voxels(r: &mut Reader, model: &mut VoxelModel, layer: usize) -> Result<()> {
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
        // Out-of-bounds records are dropped rather than rejected: they can only
        // come from a file whose grid was shrunk by hand, and losing the stray
        // voxels beats refusing to open the model.
        model.set_in(layer, x as i32, y as i32, z as i32, index);
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

    /// A count field that outruns the file must be an error, not an attempt to
    /// reserve four gigabytes.
    #[test]
    fn an_absurd_count_is_an_error_not_an_allocation() {
        let mut m = sample();
        m.rename_layer(0, ""); // so the count sits at a fixed offset
        let mut bytes = encode(&m);
        // Past the magic, the size, the two layer bytes, and the first layer's
        // flags and zero name length: the voxel count.
        bytes[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_layer_count_of_zero_or_too_many_is_an_error() {
        let mut bytes = encode(&sample());
        bytes[10] = 0;
        assert!(decode(&bytes).is_err());
        bytes[10] = MAX_LAYERS as u8 + 1;
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
