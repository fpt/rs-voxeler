//! The grid itself, and the stack of layers it is made of.

use crate::palette::Palette;

/// The largest edge a model may have.
///
/// 256 is where the coordinate stops fitting in the `u8` the `.vox` voxel
/// record uses, so it is the real interoperability ceiling rather than an
/// arbitrary one. The editor's own default is 64 — see `voxeler`'s `--size`.
pub const MAX_DIM: u16 = 256;

/// The most layers one model may hold.
///
/// A layer is a grid of its own, so this is the multiplier on the model's
/// memory: sixteen 64³ layers is 4 MiB, which is still nothing, and sixteen
/// rows is a panel you can read at a glance rather than one that has to scroll.
/// The number of layers a model *needs* is the number of parts you want to hide
/// independently, and that has never been thirty.
pub const MAX_LAYERS: usize = 16;

/// One layer: a full grid, a name, and whether it is being shown.
///
/// A grid of its own rather than a tag on each cell, so that layers genuinely
/// stack — hiding the armour reveals the body underneath instead of leaving a
/// hole where the armour was. The cost is a byte per cell per layer, which
/// [`MAX_LAYERS`] keeps bounded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layer {
    pub name: String,
    pub visible: bool,
    voxels: Vec<u8>,
}

impl Layer {
    fn new(name: impl Into<String>, cells: usize) -> Self {
        Self {
            name: name.into(),
            visible: true,
            voxels: vec![0; cells],
        }
    }

    /// This layer's own index at a cell, ignoring every other layer.
    pub fn at(&self, i: usize) -> u8 {
        self.voxels[i]
    }

    /// How many cells this layer alone fills.
    pub fn filled_count(&self) -> usize {
        self.voxels.iter().filter(|v| **v != 0).count()
    }

    pub fn is_empty(&self) -> bool {
        self.voxels.iter().all(|v| *v == 0)
    }
}

/// A stack of dense grids of palette indices, plus the palette they index.
///
/// Each layer's `voxels` is x-major: index `x + y * sx + z * sx * sy`. That
/// ordering means a run along X — the direction face extraction and the
/// rasterizer both scan — is contiguous.
///
/// # The composite is the model everything else sees
///
/// [`VoxelModel::get`] returns the *composited* index: the topmost visible
/// layer holding something at that cell. That is what the raycaster picks
/// against, what face extraction meshes, and what an export writes, so none of
/// them had to learn what a layer is. Compositing on every read would make each
/// of those O(layers) in their hottest loop, so the result is cached in
/// `composite` and repaired one cell at a time as the model is written to —
/// writes happen at the speed of a hand, reads at the speed of a frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VoxelModel {
    size: [u16; 3],
    layers: Vec<Layer>,
    composite: Vec<u8>,
    /// The layer [`VoxelModel::set`] writes to.
    ///
    /// On the model rather than in the editor because it is a property of the
    /// document — reopening a file should put you back on the layer you left —
    /// and because it is what lets every existing caller of `set` keep working
    /// without learning about layers.
    active: usize,
    palette: Palette,
}

impl VoxelModel {
    /// An empty model with one layer. Panics on a zero or oversized edge: every
    /// caller either hard-codes the size or has already validated it against
    /// [`MAX_DIM`], so a `Result` here would be a `.unwrap()` at every call
    /// site.
    pub fn new(sx: u16, sy: u16, sz: u16) -> Self {
        assert!(
            sx > 0 && sy > 0 && sz > 0,
            "a model needs a positive size, got {sx}x{sy}x{sz}"
        );
        assert!(
            sx <= MAX_DIM && sy <= MAX_DIM && sz <= MAX_DIM,
            "{sx}x{sy}x{sz} exceeds the {MAX_DIM} limit"
        );
        let cells = sx as usize * sy as usize * sz as usize;
        Self {
            size: [sx, sy, sz],
            layers: vec![Layer::new("LAYER 1", cells)],
            composite: vec![0; cells],
            active: 0,
            palette: Palette::default(),
        }
    }

    pub fn size(&self) -> [u16; 3] {
        self.size
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn palette_mut(&mut self) -> &mut Palette {
        &mut self.palette
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Whether a *signed* coordinate names a cell. Signed because every caller
    /// arrives from arithmetic that can go negative — a neighbour lookup at
    /// x = 0, a face offset — and doing the check on unsigned types means each
    /// of those has to guard the cast first.
    pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
        x >= 0
            && y >= 0
            && z >= 0
            && x < self.size[0] as i32
            && y < self.size[1] as i32
            && z < self.size[2] as i32
    }

    fn index(&self, x: i32, y: i32, z: i32) -> usize {
        x as usize + y as usize * self.size[0] as usize + z as usize * self.size[0] as usize * self.size[1] as usize
    }

    fn cells(&self) -> usize {
        self.size[0] as usize * self.size[1] as usize * self.size[2] as usize
    }

    /// The visible palette index at a cell, or 0 (air) outside the grid.
    ///
    /// Out-of-bounds reading as air is what lets face extraction ask about a
    /// neighbour without a bounds test of its own: the outside of the model is
    /// air, so its boundary faces are generated by the same rule as every
    /// interior one.
    pub fn get(&self, x: i32, y: i32, z: i32) -> u8 {
        if self.contains(x, y, z) {
            self.composite[self.index(x, y, z)]
        } else {
            0
        }
    }

    /// Write a cell of the *active* layer and report the index that layer had
    /// there. Out of bounds is a no-op returning 0, so a tool that runs off the
    /// edge of the volume simply does nothing.
    ///
    /// The value reported is the layer's, not the composite's: a build on an
    /// empty layer over an existing voxel really is a change, and reporting the
    /// voxel that happens to show through would make undo restore it onto the
    /// wrong layer.
    pub fn set(&mut self, x: i32, y: i32, z: i32, value: u8) -> u8 {
        let layer = self.active;
        self.set_in(layer, x, y, z, value)
    }

    pub fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.get(x, y, z) != 0
    }

    /// Empty every layer, keeping the stack itself.
    pub fn clear(&mut self) {
        for layer in &mut self.layers {
            layer.voxels.fill(0);
        }
        self.composite.fill(0);
    }

    /// How many cells are not air, as seen.
    pub fn filled_count(&self) -> usize {
        self.composite.iter().filter(|v| **v != 0).count()
    }

    /// Every visible cell as `(x, y, z, index)`, in storage order.
    pub fn iter_filled(&self) -> impl Iterator<Item = ([u16; 3], u8)> + '_ {
        let [sx, sy, _] = self.size;
        self.composite
            .iter()
            .enumerate()
            .filter(|(_, v)| **v != 0)
            .map(move |(i, v)| {
                let i = i as u32;
                let (sx, sy) = (sx as u32, sy as u32);
                ([(i % sx) as u16, (i / sx % sy) as u16, (i / (sx * sy)) as u16], *v)
            })
    }

    /// Every cell one layer alone fills, in storage order. What the file
    /// writer walks: a save has to record each layer's own grid, not the
    /// composite it adds up to.
    pub fn iter_filled_in(&self, layer: usize) -> impl Iterator<Item = ([u16; 3], u8)> + '_ {
        let [sx, sy, _] = self.size;
        self.layers
            .get(layer)
            .map(|l| l.voxels.as_slice())
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .filter(|(_, v)| **v != 0)
            .map(move |(i, v)| {
                let i = i as u32;
                let (sx, sy) = (sx as u32, sy as u32);
                ([(i % sx) as u16, (i / sx % sy) as u16, (i / (sx * sy)) as u16], *v)
            })
    }

    // -- layers ----------------------------------------------------------

    /// The stack, bottom first. The layer at the end is the one on top.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Which layer [`VoxelModel::set`] writes to.
    pub fn active_layer(&self) -> usize {
        self.active
    }

    /// Select the layer to write to. Out of range is ignored rather than
    /// clamped — a caller asking for a layer that is not there has a bug, and
    /// silently editing its neighbour would hide it behind lost work.
    pub fn set_active_layer(&mut self, i: usize) {
        if i < self.layers.len() {
            self.active = i;
        }
    }

    /// One layer's own index at a cell, whatever is above or below it.
    pub fn get_in(&self, layer: usize, x: i32, y: i32, z: i32) -> u8 {
        if !self.contains(x, y, z) {
            return 0;
        }
        match self.layers.get(layer) {
            Some(l) => l.voxels[self.index(x, y, z)],
            None => 0,
        }
    }

    /// Write one layer's cell, returning what that layer held. The composite is
    /// repaired for that cell alone, which is what keeps [`VoxelModel::get`] a
    /// single load.
    pub fn set_in(&mut self, layer: usize, x: i32, y: i32, z: i32, value: u8) -> u8 {
        if !self.contains(x, y, z) || layer >= self.layers.len() {
            return 0;
        }
        let i = self.index(x, y, z);
        let before = std::mem::replace(&mut self.layers[layer].voxels[i], value);
        self.composite[i] = self.composite_at(i);
        before
    }

    /// Which layer supplies the voxel visible at a cell, or `None` for air.
    ///
    /// The editor uses it to explain a click that did nothing: a tool writes to
    /// the active layer, so pointing at a voxel another layer owns is a no-op,
    /// and "layer 3 holds that voxel" is the difference between a rule and a
    /// bug.
    pub fn owner_at(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        if !self.contains(x, y, z) {
            return None;
        }
        let i = self.index(x, y, z);
        self.layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.visible && l.voxels[i] != 0)
            .map(|(n, _)| n)
    }

    /// The topmost visible layer's index at a cell, or 0.
    fn composite_at(&self, i: usize) -> u8 {
        self.layers
            .iter()
            .rev()
            .filter(|l| l.visible)
            .map(|l| l.voxels[i])
            .find(|v| *v != 0)
            .unwrap_or(0)
    }

    /// Rebuild the whole composite. Every structural change ends here, because
    /// there is no structural change whose effect on the composite is cheaper
    /// to work out than to recompute — and getting that arithmetic wrong is a
    /// model that draws layers it is not showing.
    fn recomposite(&mut self) {
        for i in 0..self.composite.len() {
            self.composite[i] = self.composite_at(i);
        }
    }

    /// Add an empty layer directly above `at`, and return where it landed.
    /// `None` when the stack is already [`MAX_LAYERS`] deep.
    pub fn add_layer(&mut self, at: usize, name: impl Into<String>) -> Option<usize> {
        if self.layers.len() >= MAX_LAYERS {
            return None;
        }
        let cells = self.cells();
        let i = (at + 1).min(self.layers.len());
        self.layers.insert(i, Layer::new(name, cells));
        if self.active >= i {
            self.active += 1;
        }
        Some(i)
    }

    /// Remove a layer. Refused when it is the last one: a model with no layer
    /// has nowhere to put a voxel, and every caller would need the empty case.
    pub fn remove_layer(&mut self, i: usize) -> bool {
        if self.layers.len() <= 1 || i >= self.layers.len() {
            return false;
        }
        self.layers.remove(i);
        self.active = self.active.min(self.layers.len() - 1);
        self.recomposite();
        true
    }

    /// Move a layer one step up or down the stack, changing what covers what.
    pub fn move_layer(&mut self, i: usize, up: bool) -> Option<usize> {
        let j = if up { i.checked_add(1)? } else { i.checked_sub(1)? };
        if i >= self.layers.len() || j >= self.layers.len() {
            return None;
        }
        self.layers.swap(i, j);
        if self.active == i {
            self.active = j;
        } else if self.active == j {
            self.active = i;
        }
        self.recomposite();
        Some(j)
    }

    /// Fold a layer into the one below it, and remove it.
    ///
    /// The upper layer wins every cell it holds, which is the same rule the
    /// composite follows — a merge has to look like what you were already
    /// seeing, or it is a surprise rather than a flatten.
    pub fn merge_down(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.layers.len() {
            return false;
        }
        let upper = self.layers[i].voxels.clone();
        let lower = &mut self.layers[i - 1];
        for (dst, src) in lower.voxels.iter_mut().zip(upper) {
            if src != 0 {
                *dst = src;
            }
        }
        // A merge into a hidden layer would make voxels vanish on the spot.
        // Showing the result is the only reading of "merge" that is not a
        // deletion in disguise.
        self.layers[i - 1].visible = true;
        self.layers.remove(i);
        self.active = self.active.min(self.layers.len() - 1);
        self.recomposite();
        true
    }

    pub fn set_layer_visible(&mut self, i: usize, visible: bool) {
        if let Some(l) = self.layers.get_mut(i) {
            l.visible = visible;
            self.recomposite();
        }
    }

    pub fn rename_layer(&mut self, i: usize, name: impl Into<String>) {
        if let Some(l) = self.layers.get_mut(i) {
            l.name = name.into();
        }
    }

    /// The whole stack, for a history entry to hold onto.
    ///
    /// A structural change cannot be recorded as a list of changed cells the
    /// way an edit can: removing a layer renumbers the ones above it, so every
    /// cell edit already in the history would start pointing at the wrong
    /// grid. Storing the stack whole sidesteps that — undo puts the exact
    /// numbering back, and the older entries line up again.
    pub fn layer_snapshot(&self) -> (Vec<Layer>, usize) {
        (self.layers.clone(), self.active)
    }

    /// Put a snapshot back. Ignores a stack that does not fit this volume,
    /// which can only come from a caller mixing two models up.
    pub fn restore_layers(&mut self, snapshot: (Vec<Layer>, usize)) {
        let (layers, active) = snapshot;
        if layers.is_empty() || layers.iter().any(|l| l.voxels.len() != self.composite.len()) {
            return;
        }
        self.active = active.min(layers.len() - 1);
        self.layers = layers;
        self.recomposite();
    }

    /// Resize in place, keeping whatever still fits at the same coordinates.
    ///
    /// Anchored at the origin rather than centred: a model's origin is the
    /// corner the renderer and any future scene graph place it by, so keeping
    /// *that* fixed is what makes growing a volume feel like adding room on the
    /// far side instead of shifting the model.
    pub fn resize(&mut self, sx: u16, sy: u16, sz: u16) {
        let mut next = VoxelModel::new(sx, sy, sz);
        next.palette = self.palette.clone();
        let cells = next.cells();
        next.layers = self
            .layers
            .iter()
            .map(|l| Layer {
                name: l.name.clone(),
                visible: l.visible,
                voxels: vec![0; cells],
            })
            .collect();
        next.active = self.active.min(next.layers.len() - 1);
        for (n, layer) in self.layers.iter().enumerate() {
            for z in 0..self.size[2] as i32 {
                for y in 0..self.size[1] as i32 {
                    for x in 0..self.size[0] as i32 {
                        let v = layer.voxels[self.index(x, y, z)];
                        if v != 0 {
                            next.set_in(n, x, y, z, v);
                        }
                    }
                }
            }
        }
        *self = next;
    }

    /// The inclusive bounding box of the visible cells, or `None` when empty.
    /// The editor uses it to frame the camera on a model it just loaded.
    pub fn occupied_bounds(&self) -> Option<([u16; 3], [u16; 3])> {
        let mut min = [u16::MAX; 3];
        let mut max = [0u16; 3];
        let mut any = false;
        for (p, _) in self.iter_filled() {
            any = true;
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
        any.then_some((min, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reports_the_previous_index() {
        let mut m = VoxelModel::new(4, 4, 4);
        assert_eq!(m.set(1, 2, 3, 7), 0);
        assert_eq!(m.set(1, 2, 3, 9), 7);
        assert_eq!(m.get(1, 2, 3), 9);
    }

    /// Face extraction leans on this: outside the grid must read as air rather
    /// than panic or wrap into the opposite edge.
    #[test]
    fn outside_the_grid_is_air() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 1);
        assert_eq!(m.get(-1, 0, 0), 0);
        assert_eq!(m.get(4, 0, 0), 0);
        assert_eq!(m.set(-1, 0, 0, 5), 0);
        assert_eq!(m.filled_count(), 1);
    }

    /// A non-cubic model is where an index-arithmetic slip shows up: with
    /// sx == sy == sz the strides coincide and a wrong one still round-trips.
    #[test]
    fn indexing_survives_unequal_edges() {
        let mut m = VoxelModel::new(2, 3, 5);
        for x in 0..2 {
            for y in 0..3 {
                for z in 0..5 {
                    m.set(x, y, z, (1 + x + y * 2 + z * 6) as u8);
                }
            }
        }
        for x in 0..2 {
            for y in 0..3 {
                for z in 0..5 {
                    assert_eq!(m.get(x, y, z), (1 + x + y * 2 + z * 6) as u8);
                }
            }
        }
        assert_eq!(m.filled_count(), 30);
    }

    #[test]
    fn iter_filled_reports_the_coordinates_it_was_stored_at() {
        let mut m = VoxelModel::new(2, 3, 5);
        m.set(1, 2, 4, 3);
        assert_eq!(m.iter_filled().collect::<Vec<_>>(), vec![([1, 2, 4], 3)]);
    }

    #[test]
    fn resize_keeps_what_still_fits_at_the_same_place() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 1);
        m.set(3, 3, 3, 2);
        m.resize(2, 2, 2);
        assert_eq!(m.get(0, 0, 0), 1);
        assert_eq!(m.filled_count(), 1);
    }

    #[test]
    fn bounds_are_inclusive_and_none_when_empty() {
        let mut m = VoxelModel::new(8, 8, 8);
        assert_eq!(m.occupied_bounds(), None);
        m.set(2, 3, 4, 1);
        m.set(5, 3, 1, 1);
        assert_eq!(m.occupied_bounds(), Some(([2, 3, 1], [5, 3, 4])));
    }

    /// A model with layers still has to look like a model to everything that
    /// was written before layers existed.
    #[test]
    fn a_new_model_has_one_layer_and_writes_land_on_it() {
        let mut m = VoxelModel::new(4, 4, 4);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.active_layer(), 0);
        m.set(1, 1, 1, 5);
        assert_eq!(m.get_in(0, 1, 1, 1), 5);
        assert_eq!(m.get(1, 1, 1), 5);
    }

    /// The whole point of a layer being a grid of its own: what is underneath
    /// is still there, and comes back when the cover is hidden.
    #[test]
    fn a_higher_layer_covers_a_lower_one_without_destroying_it() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3); // layer 0, the body
        let top = m.add_layer(0, "armour").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);

        assert_eq!(m.get(1, 1, 1), 8, "the top layer is what shows");
        assert_eq!(m.get_in(0, 1, 1, 1), 3, "the body is untouched");
        assert_eq!(m.filled_count(), 1, "one cell, whoever fills it");

        m.set_layer_visible(top, false);
        assert_eq!(m.get(1, 1, 1), 3, "hiding the cover reveals the body");
        m.set_layer_visible(0, false);
        assert_eq!(m.get(1, 1, 1), 0);
    }

    /// An erase on the active layer must not take a voxel another layer owns.
    #[test]
    fn erasing_only_clears_the_layer_it_is_aimed_at() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);

        m.set(1, 1, 1, 0); // erase, on the top layer
        assert_eq!(m.get(1, 1, 1), 3, "the body shows through again");
        assert_eq!(m.filled_count(), 1);
    }

    #[test]
    fn owner_at_names_the_layer_the_visible_voxel_came_from() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        m.set(2, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);

        assert_eq!(m.owner_at(1, 1, 1), Some(1));
        assert_eq!(m.owner_at(2, 1, 1), Some(0));
        assert_eq!(m.owner_at(3, 3, 3), None);
        m.set_layer_visible(top, false);
        assert_eq!(m.owner_at(1, 1, 1), Some(0), "a hidden layer owns nothing");
    }

    /// Inserting below the active layer must carry the cursor with it, or the
    /// next stroke lands on a layer the user did not choose.
    #[test]
    fn adding_a_layer_keeps_the_cursor_on_the_layer_it_was_on() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.add_layer(0, "b");
        m.add_layer(1, "c");
        m.set_active_layer(2);
        m.add_layer(0, "inserted"); // lands at index 1, under the active one
        assert_eq!(m.layer_count(), 4);
        assert_eq!(m.active_layer(), 3, "the cursor followed its layer up");
    }

    #[test]
    fn the_last_layer_cannot_be_removed() {
        let mut m = VoxelModel::new(4, 4, 4);
        assert!(!m.remove_layer(0));
        m.add_layer(0, "b");
        assert!(m.remove_layer(1));
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.active_layer(), 0);
    }

    /// Reordering changes what covers what, which is most of the reason to
    /// have an order at all.
    #[test]
    fn moving_a_layer_changes_which_one_shows() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);
        assert_eq!(m.get(1, 1, 1), 8);

        assert_eq!(m.move_layer(top, false), Some(0));
        assert_eq!(m.get(1, 1, 1), 3, "the body is on top now");
        assert_eq!(m.active_layer(), 0, "the cursor moved with the layer");
        assert_eq!(m.move_layer(0, false), None, "nothing below the bottom");
    }

    /// A merge has to look like what was on screen: the upper layer wins,
    /// exactly as it did in the composite.
    #[test]
    fn merging_down_keeps_what_was_visible() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        m.set(2, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);
        m.set(3, 1, 1, 9);

        assert!(m.merge_down(top));
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.get(1, 1, 1), 8, "the upper layer won the shared cell");
        assert_eq!(m.get(2, 1, 1), 3);
        assert_eq!(m.get(3, 1, 1), 9);
        assert!(!m.merge_down(0), "the bottom layer has nothing to merge into");
    }

    /// Merging into a layer nobody is looking at must not make the result
    /// disappear.
    #[test]
    fn merging_into_a_hidden_layer_shows_the_result() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        m.set_layer_visible(0, false);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(2, 1, 1, 8);

        m.merge_down(top);
        assert_eq!(m.get(1, 1, 1), 3);
        assert_eq!(m.get(2, 1, 1), 8);
    }

    /// A snapshot is how a structural change is undone, so it has to put the
    /// numbering back exactly — that is the thing older cell edits depend on.
    #[test]
    fn a_snapshot_restores_the_stack_and_the_cursor() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(1, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(2, 1, 1, 8);
        let saved = m.layer_snapshot();

        m.remove_layer(0);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.get(1, 1, 1), 0);

        m.restore_layers(saved);
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.active_layer(), 1);
        assert_eq!(m.get(1, 1, 1), 3);
        assert_eq!(m.get(2, 1, 1), 8);
    }

    #[test]
    fn the_stack_stops_at_the_layer_limit() {
        let mut m = VoxelModel::new(4, 4, 4);
        for _ in 1..MAX_LAYERS {
            assert!(m.add_layer(0, "x").is_some());
        }
        assert_eq!(m.layer_count(), MAX_LAYERS);
        assert!(m.add_layer(0, "one too many").is_none());
    }

    /// Every layer has to survive a resize, not just the one being edited.
    #[test]
    fn resize_carries_the_whole_stack() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 1);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 2);
        m.set(3, 3, 3, 5);
        m.set_layer_visible(0, false);

        m.resize(2, 2, 2);
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.layers()[1].name, "cover");
        assert!(!m.layers()[0].visible, "visibility survives too");
        assert_eq!(m.get_in(0, 0, 0, 0), 1);
        assert_eq!(m.get(1, 1, 1), 2);
        assert_eq!(m.filled_count(), 1, "the far corner no longer fits");
    }
}
