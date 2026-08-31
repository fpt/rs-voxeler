//! `.vxm` — the editor's own format.
//!
//! ```text
//! "VXM1"                       magic
//! u16 u16 u16                  size x, y, z
//! u32                          voxel count
//! count * { u8 u8 u8 u8 }      x, y, z, palette index
//! 256 * { u8 u8 u8 }           palette, RGB
//! ```
//!
//! Sparse on disk even though the grid is dense in memory: a model is mostly
//! air, and four bytes per *solid* voxel keeps a typical 64³ character under a
//! few tens of KiB where the dense form is a flat 256.
//!
//! Coordinates are one byte because [`MAX_DIM`](crate::MAX_DIM) is 256, so the
//! largest coordinate is 255. That is the same ceiling `.vox` has, which is not
//! a coincidence — it is where interoperability stops either way.

use crate::palette::{Palette, Rgb8};
use crate::{Result, VoxelError, VoxelModel};

use super::Reader;

const MAGIC: &[u8; 4] = b"VXM1";

pub fn encode(model: &VoxelModel) -> Vec<u8> {
    let filled: Vec<_> = model.iter_filled().collect();
    let mut out = Vec::with_capacity(16 + filled.len() * 4 + 768);
    out.extend_from_slice(MAGIC);
    for d in model.size() {
        out.extend_from_slice(&d.to_le_bytes());
    }
    out.extend_from_slice(&(filled.len() as u32).to_le_bytes());
    for ([x, y, z], index) in filled {
        out.extend_from_slice(&[x as u8, y as u8, z as u8, index]);
    }
    for c in model.palette().colors() {
        out.extend_from_slice(&[c.r, c.g, c.b]);
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<VoxelModel> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != MAGIC {
        return Err(VoxelError::Format(
            "not a .vxm file (bad magic); a MagicaVoxel file needs a .vox extension".into(),
        ));
    }
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

    let count = r.u32()? as usize;
    // Check the count against what is actually left before allocating from it:
    // a corrupt header claiming four billion voxels should be an error, not an
    // out-of-memory abort.
    if r.remaining() < count * 4 {
        return Err(VoxelError::Format(format!(
            "header claims {count} voxels but only {} bytes follow",
            r.remaining()
        )));
    }

    let mut model = VoxelModel::new(size[0], size[1], size[2]);
    for _ in 0..count {
        let (x, y, z, index) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
        // Out-of-bounds records are dropped rather than rejected: they can only
        // come from a file whose grid was shrunk by hand, and losing the stray
        // voxels beats refusing to open the model.
        model.set(x as i32, y as i32, z as i32, index);
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

    #[test]
    fn round_trips_geometry_and_palette() {
        let m = sample();
        assert_eq!(decode(&encode(&m)).unwrap(), m);
    }

    #[test]
    fn the_maximum_coordinate_survives_the_byte_it_is_stored_in() {
        let mut m = VoxelModel::new(256, 256, 256);
        m.set(255, 255, 255, 7);
        let back = decode(&encode(&m)).unwrap();
        assert_eq!(back.get(255, 255, 255), 7);
        assert_eq!(back.filled_count(), 1);
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
        let mut bytes = encode(&sample());
        bytes[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_truncated_file_is_an_error() {
        let bytes = encode(&sample());
        assert!(decode(&bytes[..6]).is_err());
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
