//! `.vox` — MagicaVoxel import and export.
//!
//! # The chunk tree
//!
//! ```text
//! "VOX " u32 version
//! "MAIN" u32 content_len u32 children_len
//!   "SIZE" 12 0   x y z            (u32 each)
//!   "XYZI" .. 0   n, n*{x y z i}   (u8 each)
//!   "RGBA" 1024 0 256*{r g b a}
//!   ...  anything else
//! ```
//!
//! Every chunk carries both its own length and its children's, so the reader
//! can skip a chunk it has never heard of. That is the whole reason a file from
//! a newer MagicaVoxel than the published spec still loads: version 200 adds
//! scene-graph and material chunks, and none of them stand between us and the
//! geometry.
//!
//! # Two conventions to reconcile
//!
//! **Palette indices are off by one.** The `RGBA` chunk's entry `i` is palette
//! index `i + 1`, because index 0 is air and never has a colour. Getting this
//! wrong is a model that loads with every colour one slot out — subtle enough
//! to ship.
//!
//! **MagicaVoxel is Z-up, we are Y-up.** The conversion is a rotation, not a
//! swap: `(x, y, z)vox -> (x, z, sy-1-y)`. Simply exchanging Y and Z is a
//! reflection, which loads every imported model mirrored — right hand becomes
//! left hand, and text on a model reads backwards.

use crate::palette::{Palette, Rgb8};
use crate::{Result, VoxelError, VoxelModel};

use super::Reader;

/// The version we write. 150 is the published specification; MagicaVoxel reads
/// it, and writing a number we have not implemented the extras of would be a
/// lie about the file's contents.
const WRITE_VERSION: u32 = 150;

pub fn import(bytes: &[u8]) -> Result<VoxelModel> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != b"VOX " {
        return Err(VoxelError::Format("not a MagicaVoxel .vox file".into()));
    }
    let _version = r.u32()?;

    let (id, content, _children_len) = read_chunk_header(&mut r)?;
    if &id != b"MAIN" {
        return Err(VoxelError::Format(format!(
            "expected a MAIN chunk, found {}",
            String::from_utf8_lossy(&id)
        )));
    }
    // MAIN's own content is empty in every version; the models are its children,
    // which is simply the rest of the file.
    r.take(content as usize)?;

    let mut size: Option<[u32; 3]> = None;
    let mut voxels: Option<Vec<[u8; 4]>> = None;
    let mut palette: Option<Palette> = None;

    while r.remaining() >= 12 {
        let (id, content, children) = read_chunk_header(&mut r)?;
        let body = r.take(content as usize)?;
        match &id {
            // Only the first model is taken. A `.vox` may hold several (an
            // animation, or a scene), and this crate's unit is one model — the
            // plan keeps world assembly out of the model editor on purpose.
            b"SIZE" if size.is_none() => {
                let mut b = Reader::new(body);
                size = Some([b.u32()?, b.u32()?, b.u32()?]);
            }
            b"XYZI" if voxels.is_none() => {
                let mut b = Reader::new(body);
                let n = b.u32()? as usize;
                if b.remaining() < n * 4 {
                    return Err(VoxelError::Format(format!(
                        "XYZI claims {n} voxels but holds {} bytes",
                        b.remaining()
                    )));
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push([b.u8()?, b.u8()?, b.u8()?, b.u8()?]);
                }
                voxels = Some(v);
            }
            b"RGBA" => {
                let mut b = Reader::new(body);
                let mut colors = [Rgb8::default(); 256];
                // Entry i of the chunk is palette index i + 1, so the fill
                // starts at 1 and the file's last entry has no index at all.
                for slot in colors.iter_mut().skip(1) {
                    let (red, green, blue) = (b.u8()?, b.u8()?, b.u8()?);
                    let _alpha = b.u8()?;
                    *slot = Rgb8::new(red, green, blue);
                }
                palette = Some(Palette::from_colors(colors));
            }
            // Unknown, or a second model's chunks: skip by length. `children`
            // is a byte count too, so one `take` covers the whole subtree.
            _ => {
                r.take(children as usize)?;
            }
        }
    }

    let size = size.ok_or_else(|| VoxelError::Format("no SIZE chunk".into()))?;
    for d in size {
        if d == 0 || d > crate::MAX_DIM as u32 {
            return Err(VoxelError::Format(format!(
                "SIZE {}x{}x{} is out of range 1..={}",
                size[0],
                size[1],
                size[2],
                crate::MAX_DIM
            )));
        }
    }

    // Z-up to Y-up: our Y is its Z, and our Z is its Y reversed.
    let mut model = VoxelModel::new(size[0] as u16, size[2] as u16, size[1] as u16);
    if let Some(p) = palette {
        model.set_palette(p);
    }
    for [x, y, z, index] in voxels.unwrap_or_default() {
        if index == 0 {
            continue; // air has no business in an XYZI list, but files vary
        }
        let mz = size[1] as i32 - 1 - y as i32;
        model.set(x as i32, z as i32, mz, index);
    }
    Ok(model)
}

pub fn export(model: &VoxelModel) -> Result<Vec<u8>> {
    let [sx, sy, sz] = model.size();
    let filled: Vec<_> = model.iter_filled().collect();
    if filled.len() > u32::MAX as usize {
        return Err(VoxelError::Format(
            "too many voxels for one XYZI chunk".into(),
        ));
    }

    let mut size_body = Vec::with_capacity(12);
    // Back to Z-up, reversing the import's rotation.
    for d in [sx as u32, sz as u32, sy as u32] {
        size_body.extend_from_slice(&d.to_le_bytes());
    }

    let mut xyzi_body = Vec::with_capacity(4 + filled.len() * 4);
    xyzi_body.extend_from_slice(&(filled.len() as u32).to_le_bytes());
    for ([x, y, z], index) in filled {
        let vy = sz as i32 - 1 - z as i32;
        xyzi_body.extend_from_slice(&[x as u8, vy as u8, y as u8, index]);
    }

    let mut rgba_body = Vec::with_capacity(1024);
    for i in 1..=255usize {
        let c = model.palette().get(i as u8);
        rgba_body.extend_from_slice(&[c.r, c.g, c.b, 255]);
    }
    // The 256th entry has no palette index. It is still written, because the
    // chunk's length is fixed at 1024 and a short one is a malformed file.
    rgba_body.extend_from_slice(&[0, 0, 0, 255]);

    let mut children = Vec::new();
    write_chunk(&mut children, b"SIZE", &size_body);
    write_chunk(&mut children, b"XYZI", &xyzi_body);
    write_chunk(&mut children, b"RGBA", &rgba_body);

    let mut out = Vec::with_capacity(children.len() + 32);
    out.extend_from_slice(b"VOX ");
    out.extend_from_slice(&WRITE_VERSION.to_le_bytes());
    out.extend_from_slice(b"MAIN");
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(children.len() as u32).to_le_bytes());
    out.extend_from_slice(&children);
    Ok(out)
}

fn read_chunk_header(r: &mut Reader) -> Result<([u8; 4], u32, u32)> {
    let id = r.take(4)?;
    let id = [id[0], id[1], id[2], id[3]];
    Ok((id, r.u32()?, r.u32()?))
}

fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.vox` has no place to put our layers, so an export writes what is on
    /// screen: the composite, as one model. Losing them is the honest outcome —
    /// the alternative is a scene graph we have not implemented pretending to
    /// be one we have.
    #[test]
    fn an_export_flattens_the_layer_stack() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        m.set(2, 1, 1, 3);
        let cover = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(cover);
        m.set(1, 1, 1, 8);
        let hidden = m.add_layer(cover, "hidden").unwrap();
        m.set_active_layer(hidden);
        m.set(3, 3, 3, 9);
        m.set_layer_visible(hidden, false);

        let back = import(&export(&m).unwrap()).unwrap();
        assert_eq!(back.layer_count(), 1, "a .vox comes back as one layer");
        assert_eq!(
            back.get(1, 1, 1),
            8,
            "the covering layer won, as it was drawn"
        );
        assert_eq!(back.get(2, 1, 1), 3);
        assert_eq!(back.get(3, 3, 3), 0, "a hidden layer is not exported");
        assert_eq!(back.filled_count(), 2);
    }

    #[test]
    fn round_trips_through_the_axis_change() {
        let mut m = VoxelModel::new(4, 6, 8);
        m.set(0, 0, 0, 1);
        m.set(3, 5, 7, 2);
        m.set(1, 2, 3, 99);
        m.palette_mut().set(99, Rgb8::new(9, 8, 7));

        let back = import(&export(&m).unwrap()).unwrap();
        assert_eq!(back.size(), m.size());
        assert_eq!(back.get(1, 2, 3), 99);
        assert_eq!(back.get(0, 0, 0), 1);
        assert_eq!(back.get(3, 5, 7), 2);
        assert_eq!(back.palette().get(99), Rgb8::new(9, 8, 7));
    }

    /// The axis change has to be a rotation. A reflection also round-trips
    /// through export-then-import, so the test asserts on the *file* bytes:
    /// a voxel high in our Y must be high in the file's Z.
    #[test]
    fn our_y_is_the_files_z() {
        let mut m = VoxelModel::new(2, 8, 2);
        m.set(0, 7, 0, 5);
        let bytes = export(&m).unwrap();
        let xyzi = find_chunk(&bytes, b"XYZI").unwrap();
        assert_eq!(&xyzi[4..8], &[0, 1, 7, 5], "x, y, z, index in file order");
    }

    /// Right-handedness: a model with distinct arms must not come back mirrored.
    /// Import composed with export is the identity either way, so this checks
    /// the import path against a file built by hand as MagicaVoxel would.
    #[test]
    fn import_does_not_mirror() {
        // A 2x1x2 file, Z-up: one voxel at file (1, 0, 0).
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&[1, 0, 0, 3]);
        let mut children = Vec::new();
        let mut size_body = Vec::new();
        for d in [2u32, 2, 1] {
            size_body.extend_from_slice(&d.to_le_bytes());
        }
        write_chunk(&mut children, b"SIZE", &size_body);
        write_chunk(&mut children, b"XYZI", &body);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VOX ");
        bytes.extend_from_slice(&150u32.to_le_bytes());
        bytes.extend_from_slice(b"MAIN");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(children.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&children);

        let m = import(&bytes).unwrap();
        // File Y spans 0..2 and becomes our Z reversed, so file y=0 is our z=1.
        assert_eq!(m.size(), [2, 1, 2]);
        assert_eq!(m.get(1, 0, 1), 3);
    }

    /// The off-by-one is the easy mistake, so pin it from both directions: the
    /// first RGBA entry we write must be palette index 1.
    #[test]
    fn the_rgba_chunk_starts_at_palette_index_one() {
        let mut m = VoxelModel::new(1, 1, 1);
        m.palette_mut().set(1, Rgb8::new(11, 22, 33));
        let bytes = export(&m).unwrap();
        let rgba = find_chunk(&bytes, b"RGBA").unwrap();
        assert_eq!(&rgba[..4], &[11, 22, 33, 255]);
        assert_eq!(rgba.len(), 1024);

        let back = import(&bytes).unwrap();
        assert_eq!(back.palette().get(1), Rgb8::new(11, 22, 33));
    }

    /// A version-200 file's extra chunks must be skipped, not choked on.
    #[test]
    fn unknown_chunks_are_skipped() {
        let mut m = VoxelModel::new(2, 2, 2);
        m.set(1, 1, 1, 4);
        let mut bytes = export(&m).unwrap();
        bytes[4..8].copy_from_slice(&200u32.to_le_bytes());
        // Append a chunk with a child subtree, the shape nTRN/nGRP have.
        let mut extra = Vec::new();
        write_chunk(&mut extra, b"nSHP", b"whatever");
        let tail_start = bytes.len();
        bytes.extend_from_slice(b"nTRN");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"body");
        bytes.extend_from_slice(&extra);
        // MAIN's children length has to cover what we appended.
        let added = (bytes.len() - tail_start) as u32;
        let main_children = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        bytes[16..20].copy_from_slice(&(main_children + added).to_le_bytes());

        assert_eq!(import(&bytes).unwrap().get(1, 1, 1), 4);
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_panic() {
        let mut m = VoxelModel::new(2, 2, 2);
        m.set(1, 1, 1, 4);
        let bytes = export(&m).unwrap();
        for cut in [4, 9, 20, 30, bytes.len() - 1] {
            assert!(import(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn a_file_that_is_not_vox_is_rejected() {
        assert!(import(b"VXM1\x00\x00\x00\x00\x00\x00").is_err());
    }

    /// Find a top-level chunk's body, for the byte-level assertions above.
    fn find_chunk<'a>(bytes: &'a [u8], id: &[u8; 4]) -> Option<&'a [u8]> {
        let mut i = 20; // "VOX " + version + the MAIN header
        while i + 12 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().ok()?) as usize;
            if &bytes[i..i + 4] == id {
                return Some(&bytes[i + 12..i + 12 + len]);
            }
            let children = u32::from_le_bytes(bytes[i + 8..i + 12].try_into().ok()?) as usize;
            i += 12 + len + children;
        }
        None
    }
}
