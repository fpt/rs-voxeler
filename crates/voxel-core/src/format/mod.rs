//! Reading and writing models.
//!
//! Two formats, on purpose. [`native`] is ours and is what the editor saves; it
//! is a handful of fields we control and can extend. [`vox`] is MagicaVoxel's,
//! and exists so models can come from — and go back to — the tool most people
//! already have.
//!
//! The `.vox` spec that is published is version 150, while MagicaVoxel now
//! writes 200 files with chunks that were never documented. That is exactly why
//! the internal representation is *not* `.vox`: the importer reads the chunks it
//! understands and skips the rest by length, so a newer file loads its geometry
//! instead of failing, and nothing downstream is shaped by a format we do not
//! control.

pub mod native;
pub mod obj;
pub mod surface;
pub mod threemf;
pub mod vox;
pub(crate) mod zip;

use std::path::Path;

use crate::{Result, VoxelError, VoxelModel};

/// Load by extension: `.vox` is MagicaVoxel's, anything else is ours.
pub fn load(path: &Path) -> Result<VoxelModel> {
    let bytes = std::fs::read(path)?;
    if has_extension(path, "vox") {
        vox::import(&bytes)
    } else {
        native::decode(&bytes)
    }
}

/// One voxel is one millimetre, which is what an export that names a unit
/// assumes unless told otherwise.
///
/// A 32³ character is then 32 mm — about right for a desk print, and easy
/// arithmetic to scale from.
pub const MM_PER_VOXEL: f32 = 1.0;

/// Whether a path names a format that can only be *written*.
///
/// The interchange formats are one-way on purpose. Reading OBJ or 3MF back
/// would mean voxelising an arbitrary mesh, which is a different program: the
/// document format is `.vxm`, and these are what a finished model leaves in.
pub fn is_export_only(path: &Path) -> bool {
    ["obj", "3mf", "stl"].iter().any(|e| has_extension(path, e))
}

/// Save by extension, matching [`load`] where the format can be read back.
///
/// `.obj` writes a `.mtl` beside itself, because OBJ has no colour of its own
/// and a model exported without one arrives grey.
pub fn save(path: &Path, model: &VoxelModel) -> Result<()> {
    if has_extension(path, "obj") {
        let mtl_path = path.with_extension("mtl");
        let mtl_name = mtl_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model.mtl".into());
        let out = obj::export(model, &mtl_name, MM_PER_VOXEL);
        write_atomically(&mtl_path, out.mtl.as_bytes())?;
        return write_atomically(path, out.obj.as_bytes());
    }
    let bytes = if has_extension(path, "3mf") {
        threemf::export(model, MM_PER_VOXEL)
    } else if has_extension(path, "vox") {
        vox::export(model)?
    } else {
        native::encode(model)
    };
    write_atomically(path, &bytes)
}

/// Write through a sibling temp file and rename, so a crash or a full disk
/// midway through leaves the previous file intact rather than a truncated one
/// where the user's model used to be.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-save");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// A little-endian cursor over a byte slice that reports running off the end
/// rather than panicking — every read here is of attacker-shaped input in the
/// ordinary sense that it came from a file we did not write.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(VoxelError::Format(format!(
                "truncated: wanted {n} bytes at offset {}, {} left",
                self.pos,
                self.remaining()
            )));
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Signed, because an instance's offset can point either way.
    pub(crate) fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}
