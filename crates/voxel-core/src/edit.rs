//! Undo/redo, recorded as the cells that changed.
//!
//! A batch stores the *before* and *after* index of each cell it touched, not a
//! snapshot of the grid. A drag across fifty voxels then costs 250 bytes rather
//! than 256 KiB, which is what makes an unbounded history affordable — and
//! undo/redo become the same loop run in opposite directions.

use crate::model::VoxelModel;

/// One cell's change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edit {
    pub pos: [u16; 3],
    pub before: u8,
    pub after: u8,
}

/// The cells one user action changed, under a name the HUD can show.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EditBatch {
    pub label: &'static str,
    pub edits: Vec<Edit>,
}

/// A stack of applied batches and a stack of undone ones.
#[derive(Default, Debug)]
pub struct History {
    undo: Vec<EditBatch>,
    redo: Vec<EditBatch>,
}

/// Collects the cells one action touches, so the action itself does not have to
/// know about the history at all.
///
/// A stroke is separate from [`History`] because a drag is one undo step but
/// many events: the editor opens a stroke on mouse-down, writes through it as
/// the pointer moves, and hands the whole thing to the history on mouse-up. A
/// closure-scoped transaction cannot span three event callbacks.
#[derive(Debug)]
pub struct Stroke {
    label: &'static str,
    edits: Vec<Edit>,
}

impl Stroke {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            edits: Vec::new(),
        }
    }

    /// Write a cell, recording it if it actually changed.
    ///
    /// A no-op write is dropped rather than recorded, which is what stops a
    /// drag that re-paints the same voxel forty times from producing forty
    /// entries — and stops a click that changed nothing from consuming an undo.
    pub fn set(&mut self, model: &mut VoxelModel, x: i32, y: i32, z: i32, value: u8) {
        if !model.contains(x, y, z) || model.get(x, y, z) == value {
            return;
        }
        let before = model.set(x, y, z, value);
        self.edits.push(Edit {
            pos: [x as u16, y as u16, z as u16],
            before,
            after: value,
        });
    }

    /// Whether anything has actually changed yet.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// How many cells have changed. The number a fill reports back: a span can
    /// reach a thousand cells and change none of them, and only this tells the
    /// two apart.
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn into_batch(self) -> EditBatch {
        EditBatch {
            label: self.label,
            edits: self.edits,
        }
    }
}

impl History {
    /// Run `f` against the model and push whatever it changed as one undo step.
    /// The one-shot form of [`Stroke`], for an edit that is over by the time it
    /// returns. Returns whether anything changed.
    pub fn edit(
        &mut self,
        model: &mut VoxelModel,
        label: &'static str,
        f: impl FnOnce(&mut VoxelModel, &mut Stroke),
    ) -> bool {
        let mut stroke = Stroke::new(label);
        f(model, &mut stroke);
        self.push(stroke)
    }

    /// Record an already-applied stroke. Returns whether it held anything.
    ///
    /// An empty stroke is discarded — pushing it would leave the user with an
    /// undo that visibly does nothing, which reads as a broken undo rather than
    /// an empty one.
    pub fn push(&mut self, stroke: Stroke) -> bool {
        if stroke.is_empty() {
            return false;
        }
        self.undo.push(stroke.into_batch());
        // A new edit forks the timeline; anything undone past this point is
        // unreachable and keeping it would let redo resurrect a state the user
        // has already edited away from.
        self.redo.clear();
        true
    }

    /// Revert the last batch. Returns its label.
    pub fn undo(&mut self, model: &mut VoxelModel) -> Option<&'static str> {
        let batch = self.undo.pop()?;
        for e in batch.edits.iter().rev() {
            let [x, y, z] = e.pos;
            model.set(x as i32, y as i32, z as i32, e.before);
        }
        let label = batch.label;
        self.redo.push(batch);
        Some(label)
    }

    /// Re-apply the last undone batch. Returns its label.
    pub fn redo(&mut self, model: &mut VoxelModel) -> Option<&'static str> {
        let batch = self.redo.pop()?;
        for e in &batch.edits {
            let [x, y, z] = e.pos;
            model.set(x as i32, y as i32, z as i32, e.after);
        }
        let label = batch.label;
        self.undo.push(batch);
        Some(label)
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// Forget everything. Used when the model is replaced wholesale — a load or
    /// a resize — where the recorded coordinates no longer describe this grid.
    pub fn reset(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_then_redo_returns_the_same_grid() {
        let mut m = VoxelModel::new(4, 4, 4);
        let mut h = History::default();

        h.edit(&mut m, "build", |m, tx| {
            tx.set(m, 1, 1, 1, 5);
            tx.set(m, 2, 1, 1, 6);
        });
        let after = m.clone();

        assert_eq!(h.undo(&mut m), Some("build"));
        assert_eq!(m.filled_count(), 0);
        assert_eq!(h.redo(&mut m), Some("build"));
        assert_eq!(m, after);
    }

    /// Undo has to unwind in reverse: two writes to one cell in a single batch
    /// only restore correctly if the *first* one's `before` is applied last.
    #[test]
    fn a_cell_written_twice_in_one_batch_still_unwinds() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 3);
        let mut h = History::default();

        h.edit(&mut m, "paint", |m, tx| {
            tx.set(m, 0, 0, 0, 7);
            tx.set(m, 0, 0, 0, 9);
        });
        assert_eq!(m.get(0, 0, 0), 9);
        h.undo(&mut m);
        assert_eq!(m.get(0, 0, 0), 3);
    }

    #[test]
    fn a_write_that_changes_nothing_is_not_an_undo_step() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 4);
        let mut h = History::default();

        assert!(!h.edit(&mut m, "paint", |m, tx| tx.set(m, 0, 0, 0, 4)));
        assert!(!h.edit(&mut m, "build", |m, tx| tx.set(m, 99, 0, 0, 1)));
        assert_eq!(h.undo_depth(), 0);
    }

    /// The shape a drag uses: open a stroke, write to it across several
    /// events, push it once. It must land as a single undo step.
    #[test]
    fn a_stroke_spanning_several_calls_is_one_undo_step() {
        let mut m = VoxelModel::new(4, 4, 4);
        let mut h = History::default();

        let mut stroke = Stroke::new("build");
        for x in 0..3 {
            stroke.set(&mut m, x, 0, 0, 1);
        }
        assert!(h.push(stroke));
        assert_eq!(h.undo_depth(), 1);
        assert_eq!(m.filled_count(), 3);

        h.undo(&mut m);
        assert_eq!(m.filled_count(), 0);
    }

    #[test]
    fn editing_after_an_undo_drops_the_redo_stack() {
        let mut m = VoxelModel::new(4, 4, 4);
        let mut h = History::default();

        h.edit(&mut m, "a", |m, tx| tx.set(m, 0, 0, 0, 1));
        h.undo(&mut m);
        assert_eq!(h.redo_depth(), 1);
        h.edit(&mut m, "b", |m, tx| tx.set(m, 1, 0, 0, 1));
        assert_eq!(h.redo_depth(), 0);
    }
}
