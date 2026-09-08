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
pub mod vox;

use std::path::Path;

use crate::{Result, VoxelModel, VoxelError};

/// Load by extension: `.vox` is MagicaVoxel's, anything else is ours.
pub fn load(path: &Path) -> Result<VoxelModel> {
    let bytes = std::fs::read(path)?;
    if has_extension(path, "vox") {
        vox::import(&bytes)
    } else {
        native::decode(&bytes)
    }
}

/// Save by extension, matching [`load`].
pub fn save(path: &Path, model: &VoxelModel) -> Result<()> {
    let bytes = if has_extension(path, "vox") {
        vox::export(model)?
    } else {
        native::encode(model)
    };
    // Write through a sibling temp file and rename, so a crash or a full disk
    // midway through leaves the previous save intact rather than a truncated
    // file where the user's model used to be.
    let tmp = path.with_extension("tmp-save");
    std::fs::write(&tmp, &bytes)?;
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
