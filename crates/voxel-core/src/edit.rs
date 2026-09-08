//! Undo/redo, recorded as the cells that changed.
//!
//! A batch stores the *before* and *after* index of each cell it touched, not a
//! snapshot of the grid. A drag across fifty voxels then costs 250 bytes rather
//! than 256 KiB, which is what makes an unbounded history affordable — and
//! undo/redo become the same loop run in opposite directions.
//!
//! # Except when the layers themselves change
//!
//! A cell edit names the layer it landed on, and removing or reordering a layer
//! renumbers the ones around it — so every edit already on the stack would
//! start pointing at the wrong grid. A structural change therefore stores the
//! layer stack whole, before and after ([`Change::Layers`]). Undoing one puts
//! the exact numbering back, which is what lets the older cell edits keep
//! meaning what they meant. It costs a copy of the model per structural step,
//! and structural steps happen a handful of times in a session.

use crate::model::{Snapshot, VoxelModel};
use crate::Rgb8;

/// One cell's change, on one layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edit {
    /// Which layer's grid this cell belongs to. A stroke only ever writes to
    /// the active layer, but the *history* outlives which layer that was.
    pub layer: u8,
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

/// One undoable step: cells, or the shape of the stack they live in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    Cells(EditBatch),
    Palette {
        index: u8,
        before: Rgb8,
        after: Rgb8,
    },
    /// Boxed, because this variant is the odd one out by two orders of
    /// magnitude: a `Snapshot` is the whole stack and the palette, and the undo
    /// stack is overwhelmingly `Cells`. Inline, every one-cell edit on it would
    /// carry the footprint of a structural change.
    Layers {
        label: &'static str,
        before: Box<Snapshot>,
        after: Box<Snapshot>,
    },
}

impl Change {
    pub fn label(&self) -> &'static str {
        match self {
            Change::Cells(b) => b.label,
            Change::Palette { .. } => "palette",
            Change::Layers { label, .. } => label,
        }
    }
}

/// A stack of applied changes and a stack of undone ones.
#[derive(Default, Debug)]
pub struct History {
    undo: Vec<Change>,
    redo: Vec<Change>,
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
    /// The comparison is against the *active layer*, not against what is on
    /// screen. Building on an empty layer over a voxel that shows through from
    /// below really is a change, and testing the composite would silently drop
    /// it as a no-op.
    pub fn set(&mut self, model: &mut VoxelModel, x: i32, y: i32, z: i32, value: u8) {
        self.set_in(model, model.active_layer(), x, y, z, value)
    }

    /// The same, on a layer the caller names. For an edit that spans the whole
    /// stack — clearing the model — where "the active layer" is not the answer.
    pub fn set_in(
        &mut self,
        model: &mut VoxelModel,
        layer: usize,
        x: i32,
        y: i32,
        z: i32,
        value: u8,
    ) {
        if !model.contains(x, y, z) || model.get_in(layer, x, y, z) == value {
            return;
        }
        let before = model.set_in(layer, x, y, z, value);
        self.edits.push(Edit {
            layer: layer as u8,
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
    /// Change one palette entry without copying the voxel grids.
    pub fn set_palette_color(&mut self, model: &mut VoxelModel, index: u8, after: Rgb8) -> bool {
        let before = model.palette().get(index);
        if before == after {
            return false;
        }
        model.palette_mut().set(index, after);
        self.undo.push(Change::Palette {
            index,
            before,
            after,
        });
        self.redo.clear();
        true
    }

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
        self.undo.push(Change::Cells(stroke.into_batch()));
        // A new edit forks the timeline; anything undone past this point is
        // unreachable and keeping it would let redo resurrect a state the user
        // has already edited away from.
        self.redo.clear();
        true
    }

    /// Record a change to the layer stack, snapshotting it either side of `f`.
    /// Returns whether `f` changed anything.
    pub fn restructure(
        &mut self,
        model: &mut VoxelModel,
        label: &'static str,
        f: impl FnOnce(&mut VoxelModel),
    ) -> bool {
        let before = snapshot(model);
        f(model);
        let after = snapshot(model);
        if before == after {
            return false;
        }
        self.undo.push(Change::Layers {
            label,
            before: Box::new(before),
            after: Box::new(after),
        });
        self.redo.clear();
        true
    }

    /// Revert the last change. Returns its label.
    pub fn undo(&mut self, model: &mut VoxelModel) -> Option<&'static str> {
        let change = self.undo.pop()?;
        match &change {
            Change::Cells(batch) => {
                for e in batch.edits.iter().rev() {
                    let [x, y, z] = e.pos;
                    model.set_in(e.layer as usize, x as i32, y as i32, z as i32, e.before);
                }
            }
            Change::Layers { before, .. } => restore(model, before),
            Change::Palette { index, before, .. } => model.palette_mut().set(*index, *before),
        }
        let label = change.label();
        self.redo.push(change);
        Some(label)
    }

    /// Re-apply the last undone change. Returns its label.
    pub fn redo(&mut self, model: &mut VoxelModel) -> Option<&'static str> {
        let change = self.redo.pop()?;
        match &change {
            Change::Cells(batch) => {
                for e in &batch.edits {
                    let [x, y, z] = e.pos;
                    model.set_in(e.layer as usize, x as i32, y as i32, z as i32, e.after);
                }
            }
            Change::Layers { after, .. } => restore(model, after),
            Change::Palette { index, after, .. } => model.palette_mut().set(*index, *after),
        }
        let label = change.label();
        self.undo.push(change);
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

fn snapshot(model: &VoxelModel) -> Snapshot {
    model.layer_snapshot()
}

fn restore(model: &mut VoxelModel, state: &Snapshot) {
    model.restore_layers(state.clone());
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

    /// An edit records which layer it landed on, so undo puts it back where it
    /// came from rather than onto whatever layer is active at the time.
    #[test]
    fn an_edit_undoes_onto_the_layer_it_was_made_on() {
        let mut m = VoxelModel::new(4, 4, 4);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        let mut h = History::default();
        h.edit(&mut m, "build", |m, tx| tx.set(m, 1, 1, 1, 5));

        m.set_active_layer(0);
        h.undo(&mut m);
        assert_eq!(
            m.get_in(1, 1, 1, 1),
            0,
            "undone on the layer it was made on"
        );
        h.redo(&mut m);
        assert_eq!(m.get_in(1, 1, 1, 1), 5);
        assert_eq!(m.get_in(0, 1, 1, 1), 0, "and never on the active one");
    }

    /// A build on an empty layer over a voxel showing through from below is a
    /// real change. Testing the composite would drop it as a no-op.
    #[test]
    fn a_write_hidden_by_a_lower_layer_is_still_an_edit() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 5);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        let mut h = History::default();

        assert!(h.edit(&mut m, "build", |m, tx| tx.set(m, 1, 1, 1, 5)));
        assert_eq!(m.get_in(top, 1, 1, 1), 5);
    }

    /// The reason a structural change stores the stack whole: it renumbers the
    /// layers, and every cell edit already recorded names one by number.
    #[test]
    fn undoing_past_a_removed_layer_puts_the_older_edits_back_in_place() {
        let mut m = VoxelModel::new(4, 4, 4);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        let mut h = History::default();

        h.edit(&mut m, "build", |m, tx| tx.set(m, 1, 1, 1, 5));
        assert!(h.restructure(&mut m, "delete layer", |m| {
            m.remove_layer(0);
        }));
        assert_eq!(m.layer_count(), 1);

        h.undo(&mut m); // the delete
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.get_in(1, 1, 1, 1), 5);
        h.undo(&mut m); // the build, on layer 1 again
        assert_eq!(m.get_in(1, 1, 1, 1), 0);

        h.redo(&mut m);
        h.redo(&mut m);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(
            m.get_in(0, 1, 1, 1),
            5,
            "and it came back on the merged stack"
        );
    }

    #[test]
    fn a_structural_change_that_changes_nothing_is_not_a_step() {
        let mut m = VoxelModel::new(4, 4, 4);
        let mut h = History::default();
        assert!(!h.restructure(&mut m, "delete layer", |m| {
            m.remove_layer(0); // refused: it is the last one
        }));
        assert_eq!(h.undo_depth(), 0);
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
