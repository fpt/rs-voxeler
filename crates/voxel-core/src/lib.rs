//! `voxel-core` — what a voxel model *is*, with no opinion on how it is drawn.
//!
//! One model is a dense grid of palette indices plus the 256-entry palette they
//! index. Index 0 is air, so the palette really offers 255 colours; that is the
//! MagicaVoxel convention and adopting it here means `.vox` import/export is a
//! coordinate swap rather than a re-indexing pass.
//!
//! # Dense in memory, sparse on disk
//!
//! A 64³ grid is 256 KiB — small enough that the simplest possible
//! representation wins. Editing wants `set(x, y, z)` to be a store, and face
//! extraction wants "is my neighbour air?" to be a load; both are O(1) on a
//! dense array and neither is on a `Vec<Voxel>`. The *file* is sparse, because a
//! model is mostly air and the on-disk shape is the one that has to stay small.
//!
//! # Y is up
//!
//! The grid is right-handed with +Y up, matching what the renderer and the
//! editor camera assume. MagicaVoxel is Z-up, so [`format::vox`] swaps axes on
//! the way in and out — that swap lives at the format boundary and nowhere else.

pub mod edit;
pub mod format;
pub mod model;
pub mod palette;
pub mod raycast;
pub mod region;

pub use edit::{Edit, EditBatch, History, Stroke};
pub use model::{VoxelModel, MAX_DIM};
pub use palette::{Palette, Rgb8};
pub use raycast::{Face, RayHit};
pub use region::{Brush, BrushShape, Reach, Span};

/// Anything that can go wrong loading or saving a model.
#[derive(Debug, thiserror::Error)]
pub enum VoxelError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The bytes are not the format they claim to be, or claim something
    /// impossible (a 900³ model, a chunk that runs off the end of the file).
    #[error("{0}")]
    Format(String),
}

pub type Result<T> = std::result::Result<T, VoxelError>;
