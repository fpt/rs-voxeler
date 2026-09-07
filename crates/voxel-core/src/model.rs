//! The scene, and the layers placed in it.

use crate::palette::Palette;

/// The largest edge a scene or a layer may have.
///
/// 256 is where the coordinate stops fitting in the `u8` the `.vox` voxel
/// record uses, so it is the real interoperability ceiling rather than an
/// arbitrary one. The editor's own default is 32 — see `voxeler`'s `--size`.
pub const MAX_DIM: u16 = 256;

/// The most layers one scene may hold.
///
/// Sixteen rows is a panel you can read at a glance rather than one that has to
/// scroll, and the number of layers a scene *needs* is the number of parts you
/// want to hide independently. Since layers are sized to their contents, this is
/// no longer much of a statement about memory.
pub const MAX_LAYERS: usize = 16;

/// How many objects a scene may hold, the root included.
///
/// Larger than [`MAX_LAYERS`] because an object costs a name and a parent index
/// rather than a grid: a robot with a body, two arms, two legs and a head is
/// six objects sharing far fewer layers, and the tree is the cheap half.
pub const MAX_OBJECTS: usize = 64;

/// What a thing *is*, where a layer says how pixels combine.
///
/// Layers were doing both jobs and they are not the same job. A layer has a
/// grid, a box, a stack position and a visibility — that is compositing, and it
/// works. But it was also the only way to say "this is the left arm", and for
/// that it is the wrong shape: a flat list of sixteen with no way to say one
/// thing is *part of* another.
///
/// An object owns a name, a visibility and a parent. Layers name the object
/// they belong to, and every scene has a root at index 0 that cannot be removed
/// or reparented — so "no object" and "the whole scene" are the same answer
/// rather than two that can disagree.
///
/// # There is no transform here
///
/// A translation is applied to the layers' `Bounds::origin` the moment it is
/// asked for, not stored and composed on every read. Layer boxes stay in scene
/// coordinates, so `get`, the raycaster and the extractor need to know nothing
/// about objects at all — and the move itself costs a `u16` per axis per layer
/// rather than a re-voxelisation. A stored transform would put a matrix in the
/// middle of the hottest read in the codebase to buy a thing nobody asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub name: String,
    pub visible: bool,
    /// The object this is part of. `None` only for the root.
    pub parent: Option<usize>,
}

impl Object {
    pub fn new(name: impl Into<String>, parent: Option<usize>) -> Self {
        Self {
            name: name.into(),
            visible: true,
            parent,
        }
    }
}

/// Where a layer sits in the scene, and how big it is.
///
/// A zero on any axis means empty, and an empty box allocates nothing — which is
/// what lets a new layer cost nothing until something is drawn on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Bounds {
    pub origin: [u16; 3],
    pub size: [u16; 3],
}

impl Bounds {
    pub fn new(origin: [u16; 3], size: [u16; 3]) -> Self {
        Self { origin, size }
    }

    pub fn is_empty(self) -> bool {
        self.size.contains(&0)
    }

    pub fn cells(self) -> usize {
        if self.is_empty() {
            return 0;
        }
        self.size.iter().map(|d| *d as usize).product()
    }

    /// One past the last cell on each axis.
    pub fn end(self) -> [i32; 3] {
        [
            self.origin[0] as i32 + self.size[0] as i32,
            self.origin[1] as i32 + self.size[1] as i32,
            self.origin[2] as i32 + self.size[2] as i32,
        ]
    }

    pub fn contains(self, x: i32, y: i32, z: i32) -> bool {
        let end = self.end();
        let p = [x, y, z];
        (0..3).all(|a| p[a] >= self.origin[a] as i32 && p[a] < end[a])
    }

    /// The index of a *scene* coordinate in this box's own array.
    fn index(self, x: i32, y: i32, z: i32) -> usize {
        let (lx, ly, lz) = (
            (x - self.origin[0] as i32) as usize,
            (y - self.origin[1] as i32) as usize,
            (z - self.origin[2] as i32) as usize,
        );
        lx + ly * self.size[0] as usize + lz * self.size[0] as usize * self.size[1] as usize
    }

    /// The smallest box holding both this one and the cell — the whole of what
    /// "the box grows to fit" means.
    fn grown_to(self, x: i32, y: i32, z: i32) -> Bounds {
        if self.is_empty() {
            return Bounds::new([x as u16, y as u16, z as u16], [1, 1, 1]);
        }
        let end = self.end();
        let mut origin = self.origin;
        let mut size = [0u16; 3];
        let p = [x, y, z];
        for a in 0..3 {
            let lo = (self.origin[a] as i32).min(p[a]);
            let hi = end[a].max(p[a] + 1);
            origin[a] = lo as u16;
            size[a] = (hi - lo) as u16;
        }
        Bounds { origin, size }
    }
}

/// The edge of a chunk, in voxels.
///
/// The unit a rebuild happens in. 16 because a dense one is 4 KiB — cheap to
/// walk and kind to a cache — where 8 multiplies the bookkeeping and 32 makes
/// one voxel's worth of work touch thirty-two thousand cells.
pub const CHUNK: u16 = 16;

/// A whole stack at one moment, and the scene it was sized for.
///
/// What an undo step holds when the change was structural rather than a list of
/// cells. See [`VoxelModel::layer_snapshot`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub size: [u16; 3],
    pub layers: Vec<Layer>,
    pub objects: Vec<Object>,
    pub active: usize,
}

/// One layer: a grid of its own, placed somewhere in the scene.
///
/// A grid of its own rather than a tag on each cell, so that layers genuinely
/// stack — hiding the armour reveals the body underneath instead of leaving a
/// hole where the armour was.
///
/// # Sized to its contents, not to the scene
///
/// A ground plane is 64×64×5 and a character is 16³; making both of them a
/// 64³ grid because they share a scene wastes almost all of it. The scene's
/// `size` is a *range* — where things may be placed — and each layer allocates
/// only its own box. The box grows to fit whatever is written to it and shrinks
/// only when [`VoxelModel::trim_layer`] is asked, so erasing and redrawing in
/// one spot does not churn the allocation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layer {
    pub name: String,
    pub visible: bool,
    /// Which [`Object`] this layer is part of. 0 is the root, so a layer that
    /// was never assigned still has a real answer.
    pub object: usize,
    /// `visible`, and every object above it visible too.
    ///
    /// Cached rather than walked, for the reason every other tally here is:
    /// compositing asks this once per layer per lookup, and walking to the root
    /// each time would put the depth of the tree inside `get`. The one place it
    /// moves is [`VoxelModel::refresh_shown`].
    shown: bool,
    bounds: Bounds,
    voxels: Vec<u8>,
    /// How many of this layer's own cells are not air, kept in step with
    /// `voxels` for the reason [`VoxelModel::filled`] is: the layer panel reads
    /// it on every redraw, and walking the layer for it made the panel cost the
    /// whole model.
    filled: usize,
    /// How many filled cells lie in each plane of each axis.
    ///
    /// The trick that makes a tight bounding box cheap. A *count* can be
    /// maintained by adding and subtracting one; a *box* cannot, because
    /// erasing the cell that was furthest out has to find the next furthest,
    /// and nothing short of a walk knows where that is.
    ///
    /// Counting per plane sidesteps it. A write touches three counters, and the
    /// box is the first and last non-zero plane on each axis — a scan of a few
    /// hundred numbers instead of sixteen million cells. Exact, not an
    /// approximation, and it shrinks the moment the last cell of a plane goes.
    planes: [Vec<u32>; 3],
}

impl Layer {
    fn new(name: impl Into<String>, bounds: Bounds) -> Self {
        Self {
            name: name.into(),
            visible: true,
            voxels: vec![0; bounds.cells()],
            planes: [
                vec![0; bounds.size[0] as usize],
                vec![0; bounds.size[1] as usize],
                vec![0; bounds.size[2] as usize],
            ],
            bounds,
            filled: 0,
            object: 0,
            shown: true,
        }
    }

    /// Note one cell changing, in coordinates local to this layer's box.
    ///
    /// The single place `filled` and `planes` move, so the two cannot drift
    /// apart by one of them being updated and the other forgotten.
    fn note(&mut self, local: [usize; 3], before: u8, after: u8) {
        match (before, after) {
            (0, 0) => {}
            (0, _) => {
                self.filled += 1;
                for (axis, at) in self.planes.iter_mut().zip(local) {
                    axis[at] += 1;
                }
            }
            (_, 0) => {
                self.filled -= 1;
                for (axis, at) in self.planes.iter_mut().zip(local) {
                    axis[at] -= 1;
                }
            }
            // A recolour moves neither: the cell was filled and still is.
            _ => {}
        }
    }

    /// Rebuild both tallies from the voxels. For the paths that replace the
    /// whole array rather than writing cells through [`note`](Self::note).
    fn retally(&mut self) {
        self.filled = 0;
        for a in 0..3 {
            self.planes[a] = vec![0; self.bounds.size[a] as usize];
        }
        let b = self.bounds;
        let (sx, sy) = (b.size[0] as usize, b.size[1] as usize);
        for (i, v) in self.voxels.iter().enumerate() {
            if *v == 0 {
                continue;
            }
            let local = [i % sx.max(1), i / sx.max(1) % sy.max(1), i / (sx * sy).max(1)];
            self.filled += 1;
            for (axis, at) in self.planes.iter_mut().zip(local) {
                axis[at] += 1;
            }
        }
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Whether this layer reaches the screen: its own flag, and every object
    /// above it. Compositing asks this, never `visible` — hiding an object has
    /// to hide the layers inside it.
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// Move the whole box, carrying its contents. Nothing is re-voxelised and
    /// no tally moves: the same cells are filled, at new scene coordinates.
    fn translate(&mut self, delta: [i32; 3]) {
        if self.bounds.is_empty() {
            return;
        }
        for (o, d) in self.bounds.origin.iter_mut().zip(delta) {
            *o = (*o as i32 + d) as u16;
        }
    }

    /// This layer's own index at a *scene* coordinate, 0 outside its box.
    pub fn at(&self, x: i32, y: i32, z: i32) -> u8 {
        if self.bounds.contains(x, y, z) {
            self.voxels[self.bounds.index(x, y, z)]
        } else {
            0
        }
    }

    /// How many cells this layer alone fills. O(1).
    pub fn filled_count(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    /// Every filled cell, in *scene* coordinates.
    pub fn iter_filled(&self) -> impl Iterator<Item = ([u16; 3], u8)> + '_ {
        let b = self.bounds;
        let (sx, sy) = (b.size[0] as usize, b.size[1] as usize);
        self.voxels
            .iter()
            .enumerate()
            .filter(|(_, v)| **v != 0)
            .map(move |(i, v)| {
                let (lx, ly, lz) = (i % sx.max(1), i / sx.max(1) % sy.max(1), i / (sx * sy).max(1));
                (
                    [
                        b.origin[0] + lx as u16,
                        b.origin[1] + ly as u16,
                        b.origin[2] + lz as u16,
                    ],
                    *v,
                )
            })
    }

    /// Re-place this layer's contents in a new box. Anything outside it is
    /// dropped, which only [`VoxelModel::trim_layer`] and a scene resize can
    /// cause — growing never loses a cell.
    fn reshape(&mut self, next: Bounds) {
        if next == self.bounds {
            return;
        }
        if self.grow_into(next) {
            return;
        }
        let mut voxels = vec![0u8; next.cells()];
        let mut filled = 0usize;
        let mut planes = [
            vec![0u32; next.size[0] as usize],
            vec![0u32; next.size[1] as usize],
            vec![0u32; next.size[2] as usize],
        ];
        // Tallied in the copy rather than by a second walk over the result. A
        // growing box reshapes on every write that falls outside it, so this
        // loop runs far more often than its once-per-call shape suggests, and a
        // second pass here cost three seconds on a full 256³ fill.
        for ([x, y, z], v) in self.iter_filled() {
            let (x, y, z) = (x as i32, y as i32, z as i32);
            // Reshaping can drop cells outside the new box — a trim never does,
            // a scene resize can — so the tallies count what actually landed.
            if !next.contains(x, y, z) {
                continue;
            }
            voxels[next.index(x, y, z)] = v;
            filled += 1;
            let local = [
                (x - next.origin[0] as i32) as usize,
                (y - next.origin[1] as i32) as usize,
                (z - next.origin[2] as i32) as usize,
            ];
            for (axis, at) in planes.iter_mut().zip(local) {
                axis[at] += 1;
            }
        }
        self.bounds = next;
        self.voxels = voxels;
        self.filled = filled;
        self.planes = planes;
    }

    /// The fast half of [`reshape`](Self::reshape): a box that only *grew*.
    ///
    /// Growing is what nearly every reshape is — `set_in` calls one on every
    /// write that lands outside the box — and it is also the one case that needs
    /// no arithmetic per cell. Nothing moves relative to anything else, so a row
    /// of x is contiguous in both the old array and the new one and copies whole,
    /// and the tallies do not change at all: the same cells are filled, only at
    /// new local indices, so `filled` stands and each axis' plane counts are the
    /// old ones shifted by however far that axis' origin moved.
    ///
    /// The general path walks every cell of the box computing a division and two
    /// remainders for each. Filling a 256³ scene grows the box about 768 times
    /// and so ran that walk over 2.1 billion cells — five seconds, and the whole
    /// of why a bulk fill was slow.
    ///
    /// Returns whether it applied. An empty old box has nothing to carry and a
    /// box that lost ground on any axis can drop cells, and both belong to the
    /// general path.
    fn grow_into(&mut self, next: Bounds) -> bool {
        let cur = self.bounds;
        if cur.is_empty() {
            return false;
        }
        let (end, next_end) = (cur.end(), next.end());
        if (0..3).any(|a| next.origin[a] > cur.origin[a] || next_end[a] < end[a]) {
            return false;
        }
        // Only now can this not go negative: the box holds the old one, so each
        // origin moved down or stayed.
        let shift: [usize; 3] =
            std::array::from_fn(|a| (cur.origin[a] - next.origin[a]) as usize);

        let mut voxels = vec![0u8; next.cells()];
        let (sx, sy) = (cur.size[0] as usize, cur.size[1] as usize);
        let (nx, ny) = (next.size[0] as usize, next.size[1] as usize);
        for z in 0..cur.size[2] as usize {
            for y in 0..sy {
                let from = (z * sy + y) * sx;
                let to = (z + shift[2]) * nx * ny + (y + shift[1]) * nx + shift[0];
                voxels[to..to + sx].copy_from_slice(&self.voxels[from..from + sx]);
            }
        }

        let planes = std::array::from_fn(|a| {
            let mut p = vec![0u32; next.size[a] as usize];
            p[shift[a]..shift[a] + self.planes[a].len()].copy_from_slice(&self.planes[a]);
            p
        });

        self.bounds = next;
        self.voxels = voxels;
        self.planes = planes;
        true
    }

    /// The smallest box holding every filled cell, or an empty one.
    ///
    /// A scan of the plane tallies rather than of the cells — a few hundred
    /// numbers against however many million the layer holds.
    pub fn occupied(&self) -> Bounds {
        if self.filled == 0 {
            return Bounds::default();
        }
        let mut origin = [0u16; 3];
        let mut size = [0u16; 3];
        for a in 0..3 {
            let first = self.planes[a].iter().position(|c| *c > 0);
            let last = self.planes[a].iter().rposition(|c| *c > 0);
            // `filled > 0` guarantees both, on every axis.
            let (Some(first), Some(last)) = (first, last) else {
                return Bounds::default();
            };
            origin[a] = self.bounds.origin[a] + first as u16;
            size[a] = (last - first + 1) as u16;
        }
        Bounds::new(origin, size)
    }
}

/// A scene: a range on each axis, and the layers placed within it.
///
/// # The scene's size is a range, not an allocation
///
/// `size` says where voxels *may* go — it is what the work plane spans, what the
/// camera frames and what a ray is clipped to. Nothing of that size is ever
/// allocated. The memory is the layers, and each of those is only as big as its
/// own contents.
///
/// # Reading composites, writing does not
///
/// [`VoxelModel::get`] returns the topmost visible layer holding something at a
/// cell, which is what the raycaster picks against and what an export writes.
/// There is no cached composite: one would be scene-sized, which is precisely
/// the allocation this design exists to avoid. It costs a bounds test per layer
/// instead of a single load — sixteen comparisons against one — and the callers
/// that used to sweep the whole scene volume now walk the layers instead, which
/// is a far larger saving than the lookup gives back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VoxelModel {
    size: [u16; 3],
    layers: Vec<Layer>,
    /// The object tree, as a flat arena with parent indices. Index 0 is the
    /// root and always exists.
    ///
    /// An arena rather than nested `Vec<Object>` children, because every
    /// structural change here is already snapshot-based: a flat list clones,
    /// compares and serialises without a recursive walk, and the tree shape is
    /// one `parent` field rather than a second structure to keep in agreement.
    objects: Vec<Object>,
    /// The layer [`VoxelModel::set`] writes to.
    ///
    /// On the model rather than in the editor because it is a property of the
    /// document — reopening a file should put you back on the layer you left —
    /// and because it is what lets every caller of `set` ignore layers.
    active: usize,
    palette: Palette,
    /// Which chunks have changed since a consumer last looked.
    ///
    /// Face extraction is the expensive thing a change makes necessary — at
    /// 256³ a full rebuild is ~300 ms — and almost every change is one voxel.
    /// Marking the chunk it fell in lets that rebuild be proportional to the
    /// edit rather than to the model.
    ///
    /// A bitset over the scene's chunk grid rather than a set of coordinates: a
    /// fill writes sixteen million cells, and sixteen million hash inserts
    /// would cost more than the rebuild they were meant to save. At 256³ the
    /// whole thing is 4 096 bits.
    dirty: Vec<bool>,
    /// How many cells are visible, kept in step with the layers.
    ///
    /// Counted rather than recounted because the answer is asked for far more
    /// often than it changes: every tool call reports it and the editor's status
    /// line reads it on every redraw, while the walk behind it is
    /// O(filled × layers). At 256³ that made a **single voxel edit cost 43 ms** —
    /// the write is O(1) and the report that followed it was the whole model.
    ///
    /// Maintained in [`set_in`](Self::set_in), which is the only path that
    /// changes one cell, and recomputed wholesale by the handful of structural
    /// operations that change many at once.
    filled: usize,
}

impl VoxelModel {
    /// An empty scene with one empty layer. Panics on a zero or oversized edge:
    /// every caller either hard-codes the size or has already validated it
    /// against [`MAX_DIM`], so a `Result` here would be a `.unwrap()` at every
    /// call site.
    pub fn new(sx: u16, sy: u16, sz: u16) -> Self {
        assert!(
            sx > 0 && sy > 0 && sz > 0,
            "a scene needs a positive size, got {sx}x{sy}x{sz}"
        );
        assert!(
            sx <= MAX_DIM && sy <= MAX_DIM && sz <= MAX_DIM,
            "{sx}x{sy}x{sz} exceeds the {MAX_DIM} limit"
        );
        Self {
            size: [sx, sy, sz],
            layers: vec![Layer::new("LAYER 1", Bounds::default())],
            objects: vec![Object::new("SCENE", None)],
            active: 0,
            palette: Palette::default(),
            dirty: vec![true; chunk_count([sx, sy, sz])],
            filled: 0,
        }
    }

    /// The scene's range on each axis.
    pub fn size(&self) -> [u16; 3] {
        self.size
    }

    // -- what has changed -------------------------------------------------

    /// The scene's size in chunks.
    pub fn chunk_dims(&self) -> [usize; 3] {
        chunk_dims(self.size)
    }

    /// Whether a chunk needs rebuilding.
    pub fn chunk_is_dirty(&self, index: usize) -> bool {
        self.dirty.get(index).copied().unwrap_or(false)
    }

    pub fn dirty_chunk_count(&self) -> usize {
        self.dirty.iter().filter(|d| **d).count()
    }

    /// Say every chunk is clean. The consumer that rebuilt them calls this —
    /// the model does not know when anyone has caught up.
    pub fn clear_dirty(&mut self) {
        self.dirty.fill(false);
    }

    /// Mark everything, for a change too broad to attribute to cells.
    pub fn dirty_all(&mut self) {
        self.dirty.clear();
        self.dirty.resize(chunk_count(self.size), true);
        self.dirty.fill(true);
    }

    /// Mark the chunk a cell falls in — and, when the cell sits against a
    /// chunk's face, the chunk on the other side.
    ///
    /// A face is emitted for a cell towards its neighbour, so a cell on a
    /// boundary is part of the answer for the chunk next door as well. Only a
    /// boundary cell needs that: fourteen of every sixteen along an axis are
    /// interior and cost one mark.
    fn mark(&mut self, x: i32, y: i32, z: i32) {
        let dims = self.chunk_dims();
        let c = [
            x as usize / CHUNK as usize,
            y as usize / CHUNK as usize,
            z as usize / CHUNK as usize,
        ];
        self.mark_chunk(c, dims);
        let local = [
            x as u16 % CHUNK,
            y as u16 % CHUNK,
            z as u16 % CHUNK,
        ];
        for a in 0..3 {
            if local[a] == 0 && c[a] > 0 {
                let mut n = c;
                n[a] -= 1;
                self.mark_chunk(n, dims);
            } else if local[a] == CHUNK - 1 && c[a] + 1 < dims[a] {
                let mut n = c;
                n[a] += 1;
                self.mark_chunk(n, dims);
            }
        }
    }

    fn mark_chunk(&mut self, c: [usize; 3], dims: [usize; 3]) {
        let i = c[0] + c[1] * dims[0] + c[2] * dims[0] * dims[1];
        if let Some(d) = self.dirty.get_mut(i) {
            *d = true;
        }
    }

    /// How many voxel cells are actually allocated, across every layer. What
    /// the whole per-layer-box arrangement exists to keep small.
    pub fn allocated_cells(&self) -> usize {
        self.layers.iter().map(|l| l.bounds.cells()).sum()
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

    /// Whether a *signed* coordinate is inside the scene's range. Signed
    /// because every caller arrives from arithmetic that can go negative — a
    /// neighbour lookup at x = 0, a face offset — and checking on unsigned
    /// types means each of those has to guard the cast first.
    pub fn contains(&self, x: i32, y: i32, z: i32) -> bool {
        x >= 0
            && y >= 0
            && z >= 0
            && x < self.size[0] as i32
            && y < self.size[1] as i32
            && z < self.size[2] as i32
    }

    /// The visible palette index at a cell, or 0 (air) where nothing is.
    ///
    /// Out-of-scene reading as air is what lets face extraction ask about a
    /// neighbour without a bounds test of its own: the outside is air, so
    /// boundary faces come out of the same rule as interior ones.
    pub fn get(&self, x: i32, y: i32, z: i32) -> u8 {
        self.layers
            .iter()
            .rev()
            .filter(|l| l.shown)
            .map(|l| l.at(x, y, z))
            .find(|v| *v != 0)
            .unwrap_or(0)
    }

    /// Write a cell of the *active* layer and report the index that layer had
    /// there.
    ///
    /// The value reported is the layer's, not the composite's: a build on an
    /// empty layer over a voxel that shows through from below really is a
    /// change, and reporting the voxel that happens to show would make undo
    /// restore it onto the wrong layer.
    pub fn set(&mut self, x: i32, y: i32, z: i32, value: u8) -> u8 {
        let layer = self.active;
        self.set_in(layer, x, y, z, value)
    }

    pub fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.get(x, y, z) != 0
    }

    /// Empty every layer, keeping the stack and each layer's box.
    pub fn clear(&mut self) {
        for layer in &mut self.layers {
            layer.voxels.fill(0);
            layer.retally();
        }
        self.filled = 0;
        self.dirty_all();
    }

    /// How many cells are not air, as seen. O(1).
    pub fn filled_count(&self) -> usize {
        self.filled
    }

    /// Count from scratch. The definition [`filled_count`](Self::filled_count)
    /// is kept in step with, and what the tests check it against.
    pub fn recount(&self) -> usize {
        self.iter_filled().count()
    }

    fn refresh_count(&mut self) {
        self.filled = self.recount();
    }

    /// Every visible cell as `(scene x, y, z, index)`.
    ///
    /// Walks the layers rather than the scene: a 64×64×5 ground in a 64³ scene
    /// is twenty thousand cells against a quarter of a million. Overlaps are
    /// resolved by asking who owns each cell, so a cell covered by a higher
    /// layer is emitted once, by that layer.
    pub fn iter_filled(&self) -> impl Iterator<Item = ([u16; 3], u8)> + '_ {
        self.layers
            .iter()
            .enumerate()
            .filter(|(_, l)| l.shown)
            .flat_map(move |(n, layer)| {
                layer.iter_filled().filter_map(move |(p, v)| {
                    let (x, y, z) = (p[0] as i32, p[1] as i32, p[2] as i32);
                    (self.contains(x, y, z) && self.owner_at(x, y, z) == Some(n))
                        .then_some((p, v))
                })
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

    pub fn layer_bounds(&self, layer: usize) -> Bounds {
        self.layers.get(layer).map_or(Bounds::default(), |l| l.bounds)
    }

    /// One layer's own index at a scene cell, whatever is above or below it.
    pub fn get_in(&self, layer: usize, x: i32, y: i32, z: i32) -> u8 {
        self.layers.get(layer).map_or(0, |l| l.at(x, y, z))
    }

    /// Every cell one layer alone fills, in scene coordinates. What the file
    /// writer walks: a save records each layer's own grid, not the composite.
    pub fn iter_filled_in(&self, layer: usize) -> impl Iterator<Item = ([u16; 3], u8)> + '_ {
        self.layers.get(layer).into_iter().flat_map(|l| l.iter_filled())
    }

    /// Write one layer's cell, returning what that layer held.
    ///
    /// The box **grows** to hold the cell. Writing air outside it does not — an
    /// erase that missed has nothing to record, and enlarging a layer to store
    /// a zero would be the one way a box could grow without gaining anything.
    pub fn set_in(&mut self, layer: usize, x: i32, y: i32, z: i32, value: u8) -> u8 {
        if !self.contains(x, y, z) || layer >= self.layers.len() {
            return 0;
        }
        let l = &mut self.layers[layer];
        if !l.bounds.contains(x, y, z) {
            if value == 0 {
                return 0;
            }
            let grown = l.bounds.grown_to(x, y, z);
            l.reshape(grown);
        }
        // What the cell looks like from outside, either side of the write. A
        // layer under a covering one, or a hidden layer, changes nothing
        // visible — which is why this asks `get` rather than the layer.
        let seen_before = self.get(x, y, z) != 0;
        let l = &mut self.layers[layer];
        let i = l.bounds.index(x, y, z);
        let local = [
            (x - l.bounds.origin[0] as i32) as usize,
            (y - l.bounds.origin[1] as i32) as usize,
            (z - l.bounds.origin[2] as i32) as usize,
        ];
        let before = std::mem::replace(&mut l.voxels[i], value);
        l.note(local, before, value);
        // A box is a high-water mark so that erasing and redrawing in one spot
        // does not reallocate the layer every stroke. An *empty* layer has
        // nothing to churn, and holding its old extent is pure waste: seeding a
        // model at the centre of a 256³ scene and then building near the floor
        // kept a box 26 times the size of the work.
        if l.filled == 0 && !l.bounds.is_empty() {
            l.reshape(Bounds::default());
        }
        let seen_after = self.get(x, y, z) != 0;
        match (seen_before, seen_after) {
            (false, true) => self.filled += 1,
            (true, false) => self.filled -= 1,
            _ => {}
        }
        // Even a recolour changes the faces' colour, so the chunk is dirty
        // whenever the value moved at all.
        if before != value {
            self.mark(x, y, z);
        }
        before
    }

    /// Shrink a layer's box to the cells it actually holds. Returns whether the
    /// box changed.
    pub fn trim_layer(&mut self, layer: usize) -> bool {
        let Some(l) = self.layers.get_mut(layer) else {
            return false;
        };
        let want = l.occupied();
        if want == l.bounds {
            return false;
        }
        l.reshape(want);
        true
    }

    /// Move a layer's box without moving its contents in the scene — a
    /// declaration of where it is expected to live. Contents outside the new
    /// box are dropped, so it is refused unless they all fit.
    pub fn set_layer_bounds(&mut self, layer: usize, bounds: Bounds) -> bool {
        let Some(l) = self.layers.get_mut(layer) else {
            return false;
        };
        let occupied = l.occupied();
        if !occupied.is_empty() {
            let end = occupied.end();
            let new_end = bounds.end();
            let fits = (0..3).all(|a| {
                occupied.origin[a] >= bounds.origin[a] && end[a] <= new_end[a]
            });
            if !fits {
                return false;
            }
        }
        l.reshape(bounds);
        true
    }

    /// Which layer supplies the voxel visible at a cell, or `None` for air.
    ///
    /// The editor uses it to explain a click that did nothing: a tool writes to
    /// the active layer, so pointing at a voxel another layer owns is a no-op,
    /// and "layer 3 holds that voxel" is the difference between a rule and a
    /// bug.
    pub fn owner_at(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        self.layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.shown && l.at(x, y, z) != 0)
            .map(|(n, _)| n)
    }

    /// Add an empty layer directly above `at`, and return where it landed.
    /// `None` when the stack is already [`MAX_LAYERS`] deep.
    pub fn add_layer(&mut self, at: usize, name: impl Into<String>) -> Option<usize> {
        self.add_layer_with(at, name, Bounds::default())
    }

    /// The same, with a box declared up front. It is a starting size, not a
    /// wall: writing outside it grows it like any other.
    pub fn add_layer_with(
        &mut self,
        at: usize,
        name: impl Into<String>,
        bounds: Bounds,
    ) -> Option<usize> {
        if self.layers.len() >= MAX_LAYERS {
            return None;
        }
        let i = (at + 1).min(self.layers.len());
        let mut layer = Layer::new(name, self.clamp(bounds));
        // Beside a layer means inside the same object: adding a second layer to
        // an arm should give that arm two layers, not put one of them at the
        // root for the user to file afterwards.
        layer.object = self.layers.get(at).map_or(0, |l| l.object);
        layer.shown = self.object_shown(layer.object);
        self.layers.insert(i, layer);
        if self.active >= i {
            self.active += 1;
        }
        Some(i)
    }

    /// A box cut down to the scene's range. A layer outside the scene could
    /// never be drawn or clicked, so there is nothing to be gained by keeping
    /// one.
    fn clamp(&self, b: Bounds) -> Bounds {
        if b.is_empty() {
            return Bounds::default();
        }
        let end = b.end();
        let mut origin = [0u16; 3];
        let mut size = [0u16; 3];
        for a in 0..3 {
            let lo = (b.origin[a] as i32).clamp(0, self.size[a] as i32);
            let hi = end[a].clamp(lo, self.size[a] as i32);
            origin[a] = lo as u16;
            size[a] = (hi - lo) as u16;
        }
        Bounds { origin, size }
    }

    /// Remove a layer. Refused when it is the last one: a scene with no layer
    /// has nowhere to put a voxel, and every caller would need the empty case.
    pub fn remove_layer(&mut self, i: usize) -> bool {
        if self.layers.len() <= 1 || i >= self.layers.len() {
            return false;
        }
        self.layers.remove(i);
        self.active = self.active.min(self.layers.len() - 1);
        self.refresh_count();
        self.dirty_all();
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
        self.refresh_count();
        self.dirty_all();
        Some(j)
    }

    /// Fold a layer into the one below it, and remove it.
    ///
    /// The upper layer wins every cell it holds, which is the same rule the
    /// composite follows — a merge has to look like what you were already
    /// seeing, or it is a surprise rather than a flatten. The lower box grows
    /// to hold whatever arrives.
    pub fn merge_down(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.layers.len() {
            return false;
        }
        let upper: Vec<_> = self.layers[i].iter_filled().collect();
        for ([x, y, z], v) in upper {
            self.set_in(i - 1, x as i32, y as i32, z as i32, v);
        }
        // A merge into a hidden layer would make voxels vanish on the spot.
        // Showing the result is the only reading of "merge" that is not a
        // deletion in disguise.
        self.layers[i - 1].visible = true;
        self.layers.remove(i);
        self.active = self.active.min(self.layers.len() - 1);
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
        true
    }

    pub fn set_layer_visible(&mut self, i: usize, visible: bool) {
        if let Some(l) = self.layers.get_mut(i) {
            l.visible = visible;
            self.refresh_shown();
            self.refresh_count();
            self.dirty_all();
        }
    }

    pub fn rename_layer(&mut self, i: usize, name: impl Into<String>) {
        if let Some(l) = self.layers.get_mut(i) {
            l.name = name.into();
        }
    }

    // -- objects ----------------------------------------------------------

    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// An object and everything under it, parents before children.
    ///
    /// The order matters to a caller drawing a tree; it costs nothing here
    /// because the arena is already in creation order and a child is always
    /// added after its parent.
    pub fn subtree(&self, root: usize) -> Vec<usize> {
        let mut out = Vec::new();
        if root >= self.objects.len() {
            return out;
        }
        out.push(root);
        for i in 0..self.objects.len() {
            let mut at = self.objects[i].parent;
            let mut depth = 0;
            while let Some(p) = at {
                if p == root {
                    out.push(i);
                    break;
                }
                // Bounded rather than trusting: `reparent_object` refuses a
                // cycle and `decode` breaks one, but a walk that can loop
                // forever should not depend on both being right.
                depth += 1;
                if depth > self.objects.len() {
                    break;
                }
                at = self.objects[p].parent;
            }
        }
        out
    }

    /// The layers belonging to an object, not counting its children's.
    pub fn object_layers(&self, object: usize) -> Vec<usize> {
        (0..self.layers.len())
            .filter(|n| self.layers[*n].object == object)
            .collect()
    }

    /// Whether an object and every object above it is visible.
    fn object_shown(&self, object: usize) -> bool {
        let mut at = Some(object);
        let mut depth = 0;
        while let Some(i) = at {
            let Some(o) = self.objects.get(i) else {
                return true;
            };
            if !o.visible {
                return false;
            }
            depth += 1;
            if depth > self.objects.len() {
                return true;
            }
            at = o.parent;
        }
        true
    }

    /// Recompute every layer's `shown` from its own flag and its object chain.
    ///
    /// The single place that cache moves, for the reason [`Layer::note`] is the
    /// single place a tally moves: two paths updating it is two chances for one
    /// of them to be forgotten.
    fn refresh_shown(&mut self) {
        let lit: Vec<bool> = (0..self.objects.len())
            .map(|o| self.object_shown(o))
            .collect();
        for l in &mut self.layers {
            l.shown = l.visible && lit.get(l.object).copied().unwrap_or(true);
        }
    }

    /// Replace the whole tree at once, for a loader.
    ///
    /// Validated rather than trusted: a file is not a caller. The root must be
    /// index 0 with no parent, every parent must exist, and no chain may loop —
    /// a cycle read off a disk would make the visibility walk and every tree
    /// draw run forever, so it is refused here rather than survived everywhere
    /// else.
    pub fn set_objects(&mut self, objects: Vec<Object>) -> bool {
        if objects.is_empty() || objects.len() > MAX_OBJECTS || objects[0].parent.is_some() {
            return false;
        }
        for (i, o) in objects.iter().enumerate() {
            if o.parent.is_some_and(|p| p >= objects.len() || p == i) {
                return false;
            }
        }
        for i in 0..objects.len() {
            let mut at = objects[i].parent;
            let mut depth = 0;
            while let Some(p) = at {
                depth += 1;
                if depth > objects.len() {
                    return false;
                }
                at = objects[p].parent;
            }
        }
        // A layer pointing past the end of the new tree belongs at the root:
        // dropping it would lose work over a bookkeeping detail.
        for l in &mut self.layers {
            if l.object >= objects.len() {
                l.object = 0;
            }
        }
        self.objects = objects;
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
        true
    }

    /// Add an object under `parent`. `None` past [`MAX_OBJECTS`] or when the
    /// parent does not exist.
    pub fn add_object(&mut self, parent: usize, name: impl Into<String>) -> Option<usize> {
        if self.objects.len() >= MAX_OBJECTS || parent >= self.objects.len() {
            return None;
        }
        self.objects.push(Object::new(name, Some(parent)));
        let i = self.objects.len() - 1;
        self.refresh_shown();
        Some(i)
    }

    /// Remove an object. Its children and its layers move up to its parent
    /// rather than going with it: an object is a label, and removing a label
    /// must not remove the work. Refused for the root.
    pub fn remove_object(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.objects.len() {
            return false;
        }
        let parent = self.objects[i].parent.unwrap_or(0);
        for o in &mut self.objects {
            if o.parent == Some(i) {
                o.parent = Some(parent);
            }
        }
        for l in &mut self.layers {
            if l.object == i {
                l.object = parent;
            }
        }
        self.objects.remove(i);
        // Removing renumbers everything above it, the same hazard a removed
        // layer is — and the reason a snapshot has to carry the whole tree.
        for o in &mut self.objects {
            match o.parent {
                Some(p) if p > i => o.parent = Some(p - 1),
                _ => {}
            }
        }
        for l in &mut self.layers {
            if l.object > i {
                l.object -= 1;
            }
        }
        // A layer that was inside a hidden object and has just moved up to a
        // visible parent is on screen now, so this changes the composite even
        // though no cell was written.
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
        true
    }

    pub fn rename_object(&mut self, i: usize, name: impl Into<String>) {
        if let Some(o) = self.objects.get_mut(i) {
            o.name = name.into();
        }
    }

    pub fn set_object_visible(&mut self, i: usize, visible: bool) {
        if let Some(o) = self.objects.get_mut(i) {
            o.visible = visible;
            self.refresh_shown();
            self.refresh_count();
            self.dirty_all();
        }
    }

    /// Make `i` part of `parent`. Refused for the root, and refused when
    /// `parent` is `i` or anything under it — an object cannot be part of
    /// itself at any depth, and a cycle would make the visibility walk and
    /// every tree draw loop forever.
    pub fn reparent_object(&mut self, i: usize, parent: usize) -> bool {
        if i == 0 || i >= self.objects.len() || parent >= self.objects.len() {
            return false;
        }
        if self.subtree(i).contains(&parent) {
            return false;
        }
        self.objects[i].parent = Some(parent);
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
        true
    }

    /// Put a layer in an object.
    pub fn set_layer_object(&mut self, layer: usize, object: usize) -> bool {
        if layer >= self.layers.len() || object >= self.objects.len() {
            return false;
        }
        self.layers[layer].object = object;
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
        true
    }

    /// Move an object and everything under it, in whole voxels.
    ///
    /// This is where `Bounds` does the work: a layer's box already carries an
    /// origin in scene space, so moving a part is a change to three `u16`s per
    /// layer. Nothing is re-voxelised and no tally moves — the same cells are
    /// filled, at new coordinates — so the cost is the tree walk rather than
    /// the contents.
    ///
    /// **All or nothing.** A move that pushed one arm out of the scene and
    /// left the rest would be worse than one that did not happen, so the whole
    /// subtree is checked before any of it moves. Reports how many layers moved.
    pub fn move_object(&mut self, i: usize, delta: [i32; 3]) -> Result<usize, String> {
        if i >= self.objects.len() {
            return Err(format!("there is no object {i}"));
        }
        let subtree = self.subtree(i);
        let moving: Vec<usize> = (0..self.layers.len())
            .filter(|n| subtree.contains(&self.layers[*n].object))
            .filter(|n| !self.layers[*n].bounds.is_empty())
            .collect();

        for &n in &moving {
            let b = self.layers[n].bounds;
            let end = b.end();
            for a in 0..3 {
                let lo = b.origin[a] as i32 + delta[a];
                let hi = end[a] + delta[a];
                if lo < 0 || hi > self.size[a] as i32 {
                    return Err(format!(
                        "that move would put layer {n} outside the scene on \
                         {} — it spans {}..{} and the scene is 0..{}",
                        ["x", "y", "z"][a],
                        lo,
                        hi,
                        self.size[a]
                    ));
                }
            }
        }

        if delta != [0, 0, 0] {
            for &n in &moving {
                self.layers[n].translate(delta);
            }
            // Two layers that overlapped may not any more, so what is visible
            // can change even though no cell was written.
            self.refresh_count();
            self.dirty_all();
        }
        Ok(moving.len())
    }

    /// The whole stack, for a history entry to hold onto.
    ///
    /// A structural change cannot be recorded as a list of changed cells the
    /// way an edit can: removing a layer renumbers the ones above it, so every
    /// cell edit already in the history would start pointing at the wrong grid.
    /// Storing the stack whole sidesteps that — undo puts the exact numbering
    /// back, and the older entries line up again.
    ///
    /// The scene's own size goes with it. Most structural changes leave that
    /// alone, but [`subdivide`](Self::subdivide) does not, and layers restored
    /// at their old coordinates into a scene of the new size would be a stack
    /// whose boxes all sit outside it.
    pub fn layer_snapshot(&self) -> Snapshot {
        Snapshot {
            size: self.size,
            layers: self.layers.clone(),
            objects: self.objects.clone(),
            active: self.active,
        }
    }

    /// Put a snapshot back, scene size and all.
    pub fn restore_layers(&mut self, snapshot: Snapshot) {
        if snapshot.layers.is_empty() {
            return;
        }
        self.size = snapshot.size;
        self.active = snapshot.active.min(snapshot.layers.len() - 1);
        self.layers = snapshot.layers;
        // Objects renumber when one is removed, exactly the way layers do, so a
        // snapshot that restored the stack without the tree would put every
        // layer in the wrong part of it.
        if !snapshot.objects.is_empty() {
            self.objects = snapshot.objects;
        }
        self.refresh_shown();
        self.refresh_count();
        self.dirty_all();
    }

    /// Scale the whole scene up, so every voxel becomes `factor`³ of them.
    ///
    /// The way to take a shape you are happy with and carve detail into it: the
    /// silhouette is unchanged and there is simply more room in it. Each layer's
    /// box scales with its contents, so a stack costs `factor`³ of what it did
    /// and no more — a scene mostly made of empty range does not start paying
    /// for it now.
    ///
    /// Deliberately a plain replication, not a smoothing. A subdivide that
    /// rounded corners would be a different model rather than a finer one, and
    /// you could not carve against it and get back what you drew.
    ///
    /// Refused when the result would pass [`MAX_DIM`] on any axis, or for a
    /// factor below 2 — a factor of 1 is a copy, and there is no point spending
    /// an undo step on it.
    pub fn subdivide(&mut self, factor: u16) -> Result<(), String> {
        if factor < 2 {
            return Err(format!("a subdivide needs a factor of 2 or more, got {factor}"));
        }
        let f = factor as u32;
        for (a, d) in self.size.iter().enumerate() {
            if *d as u32 * f > MAX_DIM as u32 {
                return Err(format!(
                    "{}x{}x{} by {factor} is {} on {}, past the {MAX_DIM} limit",
                    self.size[0],
                    self.size[1],
                    self.size[2],
                    *d as u32 * f,
                    ["x", "y", "z"][a]
                ));
            }
        }

        let fi = factor as i32;
        for layer in &mut self.layers {
            let b = layer.bounds;
            if b.is_empty() {
                continue;
            }
            let scaled = Bounds::new(
                [b.origin[0] * factor, b.origin[1] * factor, b.origin[2] * factor],
                [b.size[0] * factor, b.size[1] * factor, b.size[2] * factor],
            );
            let mut voxels = vec![0u8; scaled.cells()];
            for ([x, y, z], v) in layer.iter_filled() {
                let (bx, by, bz) = (x as i32 * fi, y as i32 * fi, z as i32 * fi);
                for dz in 0..fi {
                    for dy in 0..fi {
                        for dx in 0..fi {
                            voxels[scaled.index(bx + dx, by + dy, bz + dz)] = v;
                        }
                    }
                }
            }
            layer.bounds = scaled;
            layer.voxels = voxels;
            layer.retally();
        }
        self.size = [
            self.size[0] * factor,
            self.size[1] * factor,
            self.size[2] * factor,
        ];
        self.refresh_count();
        self.dirty_all();
        Ok(())
    }

    /// Resize the *scene*, keeping whatever still fits at the same coordinates.
    ///
    /// Anchored at the origin rather than centred: a scene's origin is the
    /// corner the renderer and any future scene graph place it by, so keeping
    /// *that* fixed is what makes growing a scene feel like adding room on the
    /// far side instead of shifting everything in it.
    pub fn resize(&mut self, sx: u16, sy: u16, sz: u16) {
        let next = VoxelModel::new(sx, sy, sz);
        self.size = next.size;
        for i in 0..self.layers.len() {
            let clamped = self.clamp(self.layers[i].bounds);
            self.layers[i].reshape(clamped);
        }
        self.refresh_count();
        self.dirty_all();
    }

    /// The inclusive bounding box of the visible cells, or `None` when empty.
    /// The editor uses it to frame the camera, and every screenshot frames.
    ///
    /// The union of the visible layers' own boxes, which is exactly right
    /// rather than merely close: a filled cell on *any* visible layer makes
    /// that scene cell non-air, whether or not another layer covers it, so
    /// nothing a hidden layer holds is counted and nothing a visible one holds
    /// is missed.
    pub fn occupied_bounds(&self) -> Option<([u16; 3], [u16; 3])> {
        let mut min = [u16::MAX; 3];
        let mut max = [0u16; 3];
        let mut any = false;
        for layer in self.layers.iter().filter(|l| l.shown) {
            let b = layer.occupied();
            if b.is_empty() {
                continue;
            }
            any = true;
            let end = b.end();
            for a in 0..3 {
                min[a] = min[a].min(b.origin[a]);
                max[a] = max[a].max(end[a] as u16 - 1);
            }
        }
        any.then_some((min, max))
    }
}

/// The scene's size in chunks, rounding up: a scene need not be a multiple of
/// [`CHUNK`], and the last chunk on an axis is simply short.
fn chunk_dims(size: [u16; 3]) -> [usize; 3] {
    [
        size[0].div_ceil(CHUNK) as usize,
        size[1].div_ceil(CHUNK) as usize,
        size[2].div_ceil(CHUNK) as usize,
    ]
}

fn chunk_count(size: [u16; 3]) -> usize {
    chunk_dims(size).iter().product()
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

    /// Face extraction leans on this: outside the scene must read as air rather
    /// than panic or wrap into the opposite edge.
    #[test]
    fn outside_the_scene_is_air() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.set(0, 0, 0, 1);
        assert_eq!(m.get(-1, 0, 0), 0);
        assert_eq!(m.get(4, 0, 0), 0);
        assert_eq!(m.set(-1, 0, 0, 5), 0);
        assert_eq!(m.filled_count(), 1);
    }

    /// A non-cubic scene is where an index-arithmetic slip shows up: with
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
    fn iter_filled_reports_scene_coordinates() {
        let mut m = VoxelModel::new(2, 3, 5);
        m.set(1, 2, 4, 3);
        assert_eq!(m.iter_filled().collect::<Vec<_>>(), vec![([1, 2, 4], 3)]);
    }

    #[test]
    fn bounds_are_inclusive_and_none_when_empty() {
        let mut m = VoxelModel::new(8, 8, 8);
        assert_eq!(m.occupied_bounds(), None);
        m.set(2, 3, 4, 1);
        m.set(5, 3, 1, 1);
        assert_eq!(m.occupied_bounds(), Some(([2, 3, 1], [5, 3, 4])));
    }

    // -- boxes -----------------------------------------------------------

    /// The whole point: a scene's size is a range, and a layer allocates only
    /// what it holds.
    #[test]
    fn an_empty_layer_allocates_nothing_and_grows_to_what_is_written() {
        let mut m = VoxelModel::new(64, 64, 64);
        assert_eq!(m.allocated_cells(), 0, "a 64-cubed scene, and nothing in it");

        m.set(20, 5, 10, 1);
        assert_eq!(m.layer_bounds(0), Bounds::new([20, 5, 10], [1, 1, 1]));
        assert_eq!(m.allocated_cells(), 1);

        m.set(23, 8, 13, 1);
        assert_eq!(m.layer_bounds(0), Bounds::new([20, 5, 10], [4, 4, 4]));
        assert_eq!(m.allocated_cells(), 64);
        // And growing never loses what was already there.
        assert_eq!(m.get(20, 5, 10), 1);
        assert_eq!(m.get(23, 8, 13), 1);
    }

    /// The example this was built for: three layers of very different shapes in
    /// one scene, costing their own sizes rather than three copies of it.
    #[test]
    fn layers_of_different_shapes_cost_only_themselves() {
        let mut m = VoxelModel::new(64, 64, 64);
        let ground = Bounds::new([0, 0, 0], [64, 5, 64]);
        let tree = Bounds::new([20, 5, 10], [16, 32, 16]);
        let character = Bounds::new([40, 5, 40], [16, 16, 16]);

        m.set_layer_bounds(0, ground);
        m.rename_layer(0, "GROUND");
        m.add_layer_with(0, "TREE", tree).unwrap();
        m.add_layer_with(1, "CHARACTER", character).unwrap();

        assert_eq!(m.layer_bounds(0), ground);
        assert_eq!(m.layer_bounds(1), tree);
        assert_eq!(m.layer_bounds(2), character);
        assert_eq!(
            m.allocated_cells(),
            64 * 5 * 64 + 16 * 32 * 16 + 16 * 16 * 16,
            "and not three 64-cubed grids"
        );
        assert!(m.allocated_cells() < 3 * 64 * 64 * 64 / 4);
    }

    /// A declared box is a starting size, not a wall.
    #[test]
    fn a_write_outside_a_declared_box_grows_it() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.add_layer_with(0, "TREE", Bounds::new([20, 5, 10], [4, 4, 4]))
            .unwrap();
        m.set_active_layer(1);
        m.set(19, 5, 10, 7);
        assert_eq!(m.layer_bounds(1), Bounds::new([19, 5, 10], [5, 4, 4]));
        assert_eq!(m.get(19, 5, 10), 7);
    }

    /// Erasing outside the box must not enlarge it: there is nothing to store,
    /// and it is the one way a box could grow without gaining anything.
    #[test]
    fn erasing_outside_a_box_does_not_grow_it() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.set(10, 10, 10, 1);
        let before = m.layer_bounds(0);
        assert_eq!(m.set(30, 30, 30, 0), 0);
        assert_eq!(m.layer_bounds(0), before);
    }

    /// The box keeps its high-water mark while you work; only a trim moves it
    /// back, so erasing and redrawing in one spot does not churn.
    #[test]
    fn a_box_shrinks_only_when_it_is_asked_to() {
        let mut m = VoxelModel::new(64, 64, 64);
        for x in 10..20 {
            m.set(x, 10, 10, 1);
        }
        assert_eq!(m.layer_bounds(0).size, [10, 1, 1]);

        for x in 12..20 {
            m.set(x, 10, 10, 0);
        }
        assert_eq!(m.layer_bounds(0).size, [10, 1, 1], "no shrink on erase");

        assert!(m.trim_layer(0));
        assert_eq!(m.layer_bounds(0), Bounds::new([10, 10, 10], [2, 1, 1]));
        assert_eq!(m.get(10, 10, 10), 1);
        assert_eq!(m.get(11, 10, 10), 1);
        assert!(!m.trim_layer(0), "and trimming twice changes nothing");
    }

    /// An empty layer gives its box back without being asked. The high-water
    /// mark exists so erasing and redrawing in one spot does not reallocate
    /// every stroke, and an empty layer has nothing to churn.
    #[test]
    fn an_emptied_layer_gives_its_box_back_at_once() {
        let mut m = VoxelModel::new(32, 32, 32);
        m.set(4, 4, 4, 1);
        m.set(20, 20, 20, 1);
        assert_eq!(m.layer_bounds(0).cells(), 17 * 17 * 17);

        m.set(4, 4, 4, 0);
        assert_eq!(
            m.layer_bounds(0).cells(),
            17 * 17 * 17,
            "one cell gone is not empty, and the mark stands"
        );
        m.set(20, 20, 20, 0);
        assert_eq!(m.layer_bounds(0), Bounds::default(), "the last one is");
        assert_eq!(m.allocated_cells(), 0);
        assert!(!m.trim_layer(0), "and there is nothing left to trim");

        m.set(30, 30, 30, 1);
        assert_eq!(m.layer_bounds(0), Bounds::new([30, 30, 30], [1, 1, 1]));
    }

    /// The everyday shape of it: a new model seeds one voxel at the centre, and
    /// building near the floor after erasing it should not carry the centre.
    #[test]
    fn erasing_a_seed_does_not_leave_its_box_behind() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.set(32, 32, 32, 1);
        m.set(32, 32, 32, 0);
        for z in 0..64 {
            for x in 0..64 {
                m.set(x, 0, z, 4);
            }
        }
        assert_eq!(m.filled_count(), 64 * 64);
        assert_eq!(
            m.allocated_cells(),
            64 * 64,
            "the floor, and not a column up to where the seed was"
        );
    }

    /// A box that would drop voxels is refused rather than silently losing
    /// them — the one destructive thing `set_layer_bounds` could do.
    #[test]
    fn a_box_that_would_cut_off_voxels_is_refused() {
        let mut m = VoxelModel::new(32, 32, 32);
        m.set(10, 10, 10, 1);
        assert!(!m.set_layer_bounds(0, Bounds::new([0, 0, 0], [4, 4, 4])));
        assert_eq!(m.get(10, 10, 10), 1);
        assert!(m.set_layer_bounds(0, Bounds::new([8, 8, 8], [8, 8, 8])));
        assert_eq!(m.get(10, 10, 10), 1);
    }

    #[test]
    fn a_box_is_cut_down_to_the_scenes_range() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.add_layer_with(0, "big", Bounds::new([8, 8, 8], [64, 64, 64]))
            .unwrap();
        assert_eq!(m.layer_bounds(1), Bounds::new([8, 8, 8], [8, 8, 8]));
    }

    // -- stacking --------------------------------------------------------

    #[test]
    fn a_new_model_has_one_layer_and_writes_land_on_it() {
        let mut m = VoxelModel::new(4, 4, 4);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.active_layer(), 0);
        m.set(1, 1, 1, 5);
        assert_eq!(m.get_in(0, 1, 1, 1), 5);
        assert_eq!(m.get(1, 1, 1), 5);
    }

    /// Layers still stack, and now they do it across different boxes.
    #[test]
    fn a_higher_layer_covers_a_lower_one_without_destroying_it() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.set(1, 1, 1, 3);
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

    /// Two layers whose boxes do not even touch: each is only asked about its
    /// own cells, and neither shadows the other.
    #[test]
    fn layers_that_do_not_overlap_both_show() {
        let mut m = VoxelModel::new(64, 16, 16);
        m.set(1, 1, 1, 3);
        let far = m.add_layer(0, "far").unwrap();
        m.set_active_layer(far);
        m.set(60, 1, 1, 9);

        assert_eq!(m.get(1, 1, 1), 3);
        assert_eq!(m.get(60, 1, 1), 9);
        assert_eq!(m.filled_count(), 2);
        assert_eq!(m.owner_at(1, 1, 1), Some(0));
        assert_eq!(m.owner_at(60, 1, 1), Some(far));
    }

    #[test]
    fn erasing_only_clears_the_layer_it_is_aimed_at() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);

        m.set(1, 1, 1, 0);
        assert_eq!(m.get(1, 1, 1), 3, "the body shows through again");
        assert_eq!(m.filled_count(), 1);
    }

    #[test]
    fn owner_at_names_the_layer_the_visible_voxel_came_from() {
        let mut m = VoxelModel::new(8, 8, 8);
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

    #[test]
    fn adding_a_layer_keeps_the_cursor_on_the_layer_it_was_on() {
        let mut m = VoxelModel::new(4, 4, 4);
        m.add_layer(0, "b");
        m.add_layer(1, "c");
        m.set_active_layer(2);
        m.add_layer(0, "inserted");
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

    #[test]
    fn moving_a_layer_changes_which_one_shows() {
        let mut m = VoxelModel::new(8, 8, 8);
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

    /// A merge has to look like what was on screen, and the lower box has to
    /// grow to hold what arrives from a box that was somewhere else.
    #[test]
    fn merging_down_keeps_what_was_visible_and_grows_the_box() {
        let mut m = VoxelModel::new(32, 32, 32);
        m.set(1, 1, 1, 3);
        m.set(2, 1, 1, 3);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 8);
        m.set(20, 1, 1, 9);

        assert!(m.merge_down(top));
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.get(1, 1, 1), 8, "the upper layer won the shared cell");
        assert_eq!(m.get(2, 1, 1), 3);
        assert_eq!(m.get(20, 1, 1), 9, "and the far one came along");
        assert!(m.layer_bounds(0).contains(20, 1, 1));
        assert!(!m.merge_down(0), "the bottom layer has nothing to merge into");
    }

    #[test]
    fn merging_into_a_hidden_layer_shows_the_result() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 3);
        m.set_layer_visible(0, false);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(2, 1, 1, 8);

        m.merge_down(top);
        assert_eq!(m.get(1, 1, 1), 3);
        assert_eq!(m.get(2, 1, 1), 8);
    }

    #[test]
    fn a_snapshot_restores_the_stack_the_boxes_and_the_cursor() {
        let mut m = VoxelModel::new(32, 32, 32);
        m.set(1, 1, 1, 3);
        let top = m.add_layer_with(0, "cover", Bounds::new([8, 8, 8], [4, 4, 4])).unwrap();
        m.set_active_layer(top);
        m.set(9, 9, 9, 8);
        let saved = m.layer_snapshot();

        m.remove_layer(0);
        assert_eq!(m.layer_count(), 1);
        assert_eq!(m.get(1, 1, 1), 0);

        m.restore_layers(saved);
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.active_layer(), 1);
        assert_eq!(m.get(1, 1, 1), 3);
        assert_eq!(m.get(9, 9, 9), 8);
        assert_eq!(m.layer_bounds(1), Bounds::new([8, 8, 8], [4, 4, 4]));
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

    // -- objects ----------------------------------------------------------

    /// A scene always has a root, and it is not removable or reparentable —
    /// otherwise "no object" and "the whole scene" become two answers that can
    /// disagree.
    #[test]
    fn the_root_object_is_always_there_and_cannot_be_taken_away() {
        let mut m = VoxelModel::new(16, 16, 16);
        assert_eq!(m.object_count(), 1);
        assert_eq!(m.objects()[0].parent, None);
        assert_eq!(m.layers()[0].object, 0, "a layer starts at the root");

        assert!(!m.remove_object(0));
        let arm = m.add_object(0, "ARM").unwrap();
        assert!(!m.reparent_object(0, arm), "the root has no parent to gain");
    }

    /// Hiding an object hides the layers inside it, at any depth, and showing
    /// it again does not resurrect a layer the user had hidden themselves.
    #[test]
    fn hiding_an_object_hides_what_is_under_it_without_forgetting_the_layers_own_flag() {
        let mut m = VoxelModel::new(16, 16, 16);
        let robot = m.add_object(0, "ROBOT").unwrap();
        let arm = m.add_object(robot, "ARM").unwrap();
        let hand = m.add_layer(0, "HAND").unwrap();
        m.set_layer_object(hand, arm);
        m.set_in(hand, 4, 4, 4, 7);
        assert_eq!(m.get(4, 4, 4), 7);
        assert_eq!(m.filled_count(), 1);

        // Two levels up, so this is the chain and not just the parent.
        m.set_object_visible(robot, false);
        assert_eq!(m.get(4, 4, 4), 0, "the layer is inside a hidden object");
        assert_eq!(m.filled_count(), 0);
        assert!(m.layers()[hand].visible, "but its own flag never moved");
        assert!(!m.layers()[hand].shown());

        // Hide the layer itself as well, then show the object again.
        m.set_layer_visible(hand, false);
        m.set_object_visible(robot, true);
        assert_eq!(m.get(4, 4, 4), 0, "the layer is still hidden on its own");
        m.set_layer_visible(hand, true);
        assert_eq!(m.get(4, 4, 4), 7);
    }

    /// The cached `shown` is the one thing here that can silently drift, so it
    /// is checked against a walk up the tree after every operation that can
    /// change it.
    #[test]
    fn the_shown_flags_never_drift_from_a_walk_up_the_tree() {
        let mut m = VoxelModel::new(16, 16, 16);
        let a = m.add_object(0, "A").unwrap();
        let b = m.add_object(a, "B").unwrap();
        let c = m.add_object(0, "C").unwrap();
        for n in 0..3 {
            let l = m.add_layer(n, format!("L{n}")).unwrap();
            m.set_layer_object(l, [a, b, c][n]);
            m.set_in(l, n as i32, 1, 1, 3);
        }

        let check = |m: &VoxelModel, note: &str| {
            for (n, l) in m.layers().iter().enumerate() {
                let mut walk = l.visible;
                let mut at = Some(l.object);
                while let Some(i) = at {
                    walk &= m.objects()[i].visible;
                    at = m.objects()[i].parent;
                }
                assert_eq!(l.shown(), walk, "layer {n} after {note}");
            }
            assert_eq!(m.filled_count(), m.recount(), "count after {note}");
        };

        check(&m, "setup");
        m.set_object_visible(b, false);
        check(&m, "hiding a leaf object");
        m.set_object_visible(a, false);
        check(&m, "hiding its parent too");
        m.reparent_object(b, c);
        check(&m, "reparenting out of a hidden object");
        m.set_layer_visible(0, false);
        check(&m, "hiding a layer");
        m.remove_object(a);
        check(&m, "removing an object");
        m.add_layer(0, "extra");
        check(&m, "adding a layer");
        m.restore_layers(m.layer_snapshot());
        check(&m, "restoring a snapshot");
    }

    /// Removing an object is removing a label. The work inside it moves up to
    /// the parent — a delete that took the geometry with it would be a very
    /// expensive way to rename something.
    #[test]
    fn removing_an_object_keeps_its_children_and_its_layers() {
        let mut m = VoxelModel::new(16, 16, 16);
        let robot = m.add_object(0, "ROBOT").unwrap();
        let arm = m.add_object(robot, "ARM").unwrap();
        let hand = m.add_object(arm, "HAND").unwrap();
        let l = m.add_layer(0, "SKIN").unwrap();
        m.set_layer_object(l, arm);
        m.set_in(l, 2, 2, 2, 5);

        assert!(m.remove_object(arm));
        assert_eq!(m.get(2, 2, 2), 5, "the voxels are untouched");
        // Removing `arm` renumbers everything above it, `hand` included.
        assert_eq!(m.objects()[hand - 1].name, "HAND");
        assert_eq!(m.objects()[hand - 1].parent, Some(robot), "the child moved up");
        assert_eq!(m.layers()[l].object, robot, "and so did the layer");
    }

    /// An object cannot be made part of itself at any depth. A cycle would make
    /// the visibility walk and every tree draw loop forever.
    #[test]
    fn an_object_cannot_be_reparented_into_its_own_subtree() {
        let mut m = VoxelModel::new(16, 16, 16);
        let a = m.add_object(0, "A").unwrap();
        let b = m.add_object(a, "B").unwrap();
        let c = m.add_object(b, "C").unwrap();

        assert!(!m.reparent_object(a, a), "not into itself");
        assert!(!m.reparent_object(a, b), "not into its child");
        assert!(!m.reparent_object(a, c), "nor its grandchild");
        assert!(m.reparent_object(c, 0), "but out is always fine");
        assert!(m.reparent_object(a, c), "and now that is no longer a cycle");
    }

    /// Moving an object moves its layers' boxes and nothing else: the same
    /// cells are filled, at new coordinates, with no re-voxelisation.
    #[test]
    fn moving_an_object_carries_its_subtree_and_costs_no_reallocation() {
        let mut m = VoxelModel::new(32, 32, 32);
        let robot = m.add_object(0, "ROBOT").unwrap();
        let arm = m.add_object(robot, "ARM").unwrap();
        let body = m.add_layer(m.layer_count() - 1, "BODY").unwrap();
        let hand = m.add_layer(m.layer_count() - 1, "HAND").unwrap();
        m.set_layer_object(body, robot);
        m.set_layer_object(hand, arm);
        m.set_in(body, 4, 4, 4, 1);
        m.set_in(hand, 6, 4, 4, 2);
        let allocated = m.allocated_cells();

        assert_eq!(m.move_object(robot, [3, 0, 1]), Ok(2), "both layers moved");
        assert_eq!(m.get(7, 4, 5), 1, "the body went with it");
        assert_eq!(m.get(9, 4, 5), 2, "and so did the child object's layer");
        assert_eq!(m.get(4, 4, 4), 0, "nothing was left behind");
        assert_eq!(m.allocated_cells(), allocated, "and nothing was reallocated");
        assert_eq!(m.filled_count(), 2);
    }

    /// All or nothing. A move that put one arm outside the scene and left the
    /// rest where it was would be worse than one that did not happen.
    #[test]
    fn a_move_that_would_leave_the_scene_moves_nothing() {
        let mut m = VoxelModel::new(16, 16, 16);
        let o = m.add_object(0, "PART").unwrap();
        let near = m.add_layer(m.layer_count() - 1, "NEAR").unwrap();
        let far = m.add_layer(m.layer_count() - 1, "FAR").unwrap();
        m.set_layer_object(near, o);
        m.set_layer_object(far, o);
        m.set_in(near, 1, 1, 1, 1);
        m.set_in(far, 15, 1, 1, 2);

        assert!(m.move_object(o, [3, 0, 0]).is_err(), "FAR would go past 16");
        assert_eq!(m.get(1, 1, 1), 1, "so NEAR did not move either");
        assert_eq!(m.get(15, 1, 1), 2);
        assert_eq!(m.move_object(o, [-1, 0, 0]), Ok(2), "the other way fits");
        assert_eq!(m.get(0, 1, 1), 1);
        assert_eq!(m.get(14, 1, 1), 2);
    }

    /// A growing box takes the copy-only path, which moves every cell's local
    /// index without recomputing any of them. Growing on the **low** side is
    /// where that goes wrong if the shift is missed or applied to the wrong
    /// axis, so the growth here is downward on every axis and upward on two,
    /// and each voxel is checked at the *scene* coordinate it was written to.
    #[test]
    fn growing_a_box_carries_every_cell_and_both_tallies() {
        let mut m = VoxelModel::new(64, 64, 64);
        // A deliberately non-cubic seed: with equal extents a swapped stride
        // still lands on a valid cell.
        let seed = [
            ([20, 30, 40], 1u8),
            ([22, 31, 40], 2),
            ([20, 33, 42], 3),
            ([23, 30, 43], 4),
        ];
        for ([x, y, z], v) in seed {
            m.set_in(0, x, y, z, v);
        }
        let before = m.layer_bounds(0);
        assert_eq!(m.layers()[0].filled_count(), 4);

        // Down on all three axes, and out past the far end on x and z.
        for ([x, y, z], v) in [([5, 6, 7], 9u8), ([50, 31, 55], 8)] {
            m.set_in(0, x, y, z, v);
        }
        let after = m.layer_bounds(0);
        assert!(
            after.origin[0] < before.origin[0]
                && after.origin[1] < before.origin[1]
                && after.origin[2] < before.origin[2],
            "the box grew downward on every axis: {before:?} -> {after:?}"
        );

        for ([x, y, z], v) in seed {
            assert_eq!(m.get_in(0, x, y, z), v, "cell {x},{y},{z} moved");
        }
        assert_eq!(m.get_in(0, 5, 6, 7), 9);
        assert_eq!(m.get_in(0, 50, 31, 55), 8);
        assert_eq!(m.layers()[0].filled_count(), 6);
        // `occupied` reads the plane tallies, so a shifted plane vector that
        // landed at the wrong offset shows up here and nowhere else.
        assert_eq!(m.layers()[0].occupied(), Bounds::new([5, 6, 7], [46, 28, 49]));
        assert_eq!(m.filled_count(), 6);
    }

    /// The count is maintained rather than walked, so the one thing that can go
    /// wrong is drift. Every operation that can change what is visible is run
    /// here, and the cheap answer is checked against the expensive one after
    /// each of them.
    #[test]
    fn the_maintained_count_never_drifts_from_a_fresh_walk() {
        let mut m = VoxelModel::new(16, 16, 16);
        let mut step = 0;
        let mut check = |m: &VoxelModel, what: &str| {
            step += 1;
            assert_eq!(
                m.filled_count(),
                m.recount(),
                "step {step} ({what}): the composite count drifted"
            );
            // And each layer's own count, which the layer panel reads.
            for (n, l) in m.layers().iter().enumerate() {
                assert_eq!(
                    l.filled_count(),
                    m.iter_filled_in(n).count(),
                    "step {step} ({what}): layer {n} drifted"
                );
                assert_eq!(l.is_empty(), l.filled_count() == 0, "step {step} ({what})");
                // The plane tallies, checked against the box they are supposed
                // to describe. A tally that drifts is a camera framing the
                // wrong thing, or a trim that cuts off voxels.
                let mut lo = [u16::MAX; 3];
                let mut hi = [0u16; 3];
                let mut any = false;
                for (p, _) in m.iter_filled_in(n) {
                    any = true;
                    for a in 0..3 {
                        lo[a] = lo[a].min(p[a]);
                        hi[a] = hi[a].max(p[a]);
                    }
                }
                let want = if any {
                    Bounds::new(lo, [hi[0] - lo[0] + 1, hi[1] - lo[1] + 1, hi[2] - lo[2] + 1])
                } else {
                    Bounds::default()
                };
                assert_eq!(
                    l.occupied(),
                    want,
                    "step {step} ({what}): layer {n}'s occupied box drifted"
                );
            }
            // And the scene's, which is what frames the camera.
            let mut lo = [u16::MAX; 3];
            let mut hi = [0u16; 3];
            let mut any = false;
            for (p, _) in m.iter_filled() {
                any = true;
                for a in 0..3 {
                    lo[a] = lo[a].min(p[a]);
                    hi[a] = hi[a].max(p[a]);
                }
            }
            assert_eq!(
                m.occupied_bounds(),
                any.then_some((lo, hi)),
                "step {step} ({what}): the scene's occupied box drifted"
            );
        };

        for x in 0..8 {
            m.set(x, 0, 0, 3);
        }
        check(&m, "writes");
        m.set(3, 0, 0, 0);
        check(&m, "an erase");
        m.set(3, 0, 0, 0);
        check(&m, "erasing air again");
        m.set(1, 0, 0, 9);
        check(&m, "a recolour, which changes no count");
        // The case a maintained *box* cannot do and plane tallies can: erasing
        // the cell that was furthest out has to find the next furthest.
        m.set(7, 0, 0, 0);
        check(&m, "erasing the furthest cell, so the box must shrink");
        m.set(0, 0, 0, 0);
        check(&m, "and the nearest, so it must shrink from the other end");

        // A second layer over the first: writing where something already shows
        // must not count twice, and hiding it must give the cell back.
        let top = m.add_layer(0, "cover").unwrap();
        check(&m, "adding a layer");
        m.set_active_layer(top);
        for x in 0..4 {
            m.set(x, 0, 0, 5);
        }
        check(&m, "writing over a covered cell");
        m.set(9, 0, 0, 5);
        check(&m, "writing where nothing was");

        m.set_layer_visible(top, false);
        check(&m, "hiding a layer");
        m.set(10, 0, 0, 7);
        check(&m, "writing to a hidden layer");
        m.set_layer_visible(top, true);
        check(&m, "showing it again");

        m.move_layer(top, false).unwrap();
        check(&m, "reordering");
        let saved = m.layer_snapshot();
        m.merge_down(1);
        check(&m, "merging down");
        m.restore_layers(saved);
        check(&m, "restoring a snapshot");
        m.remove_layer(1);
        check(&m, "removing a layer");

        m.subdivide(2).unwrap();
        check(&m, "subdividing");
        m.resize(4, 4, 4);
        check(&m, "shrinking the scene");
        m.clear();
        check(&m, "clearing");
        assert_eq!(m.filled_count(), 0);
    }

    /// Every voxel becomes a block, and the shape is otherwise untouched.
    #[test]
    fn subdividing_scales_the_scene_and_every_layer() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.rename_layer(0, "GROUND");
        m.set(2, 0, 3, 5);
        let top = m.add_layer(0, "TOWER").unwrap();
        m.set_active_layer(top);
        m.set(10, 4, 10, 9);
        m.set_layer_visible(0, false);

        assert_eq!(m.subdivide(2), Ok(()));
        assert_eq!(m.size(), [32, 32, 32]);
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.layers()[1].name, "TOWER");
        assert!(!m.layers()[0].visible, "visibility survives");
        assert_eq!(m.active_layer(), 1, "and so does the cursor");

        // One voxel became eight, at twice the coordinates.
        for dz in 0..2 {
            for dy in 0..2 {
                for dx in 0..2 {
                    assert_eq!(m.get_in(0, 4 + dx, dy, 6 + dz), 5);
                    assert_eq!(m.get(20 + dx, 8 + dy, 20 + dz), 9);
                }
            }
        }
        assert_eq!(m.get_in(0, 4, 0, 5), 0, "and nothing beside it");
        assert_eq!(m.layers()[1].filled_count(), 8);
    }

    /// A layer's box scales with its contents, so a scene mostly made of empty
    /// range does not start paying for it.
    #[test]
    fn subdividing_costs_the_cube_of_the_factor_and_no_more() {
        let mut m = VoxelModel::new(64, 64, 64);
        m.set(20, 5, 10, 1);
        m.set(23, 8, 13, 1);
        let before = m.allocated_cells();
        assert_eq!(before, 64);

        m.subdivide(2).unwrap();
        assert_eq!(m.allocated_cells(), before * 8, "and not the scene's cube");
        assert_eq!(m.layer_bounds(0), Bounds::new([40, 10, 20], [8, 8, 8]));
    }

    #[test]
    fn subdividing_past_the_dimension_limit_is_refused() {
        let mut m = VoxelModel::new(200, 8, 8);
        m.set(1, 1, 1, 1);
        let err = m.subdivide(2).unwrap_err();
        assert!(err.contains("256"), "{err}");
        assert!(err.contains(" x"), "names the axis: {err}");
        assert_eq!(m.size(), [200, 8, 8], "and nothing moved");
        assert_eq!(m.get(1, 1, 1), 1);

        assert!(m.subdivide(1).is_err(), "a factor of 1 is a copy");
        assert!(m.subdivide(0).is_err());
    }

    #[test]
    fn a_factor_of_three_triples_every_axis() {
        let mut m = VoxelModel::new(8, 8, 8);
        m.set(1, 1, 1, 7);
        m.subdivide(3).unwrap();
        assert_eq!(m.size(), [24, 24, 24]);
        assert_eq!(m.filled_count(), 27);
        assert_eq!(m.get(3, 3, 3), 7);
        assert_eq!(m.get(5, 5, 5), 7);
        assert_eq!(m.get(6, 3, 3), 0);
    }

    /// The snapshot has to carry the scene size, or undoing a subdivide leaves
    /// every layer's box outside the scene it is restored into.
    #[test]
    fn a_snapshot_puts_the_scene_size_back_too() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.set(2, 3, 4, 5);
        let saved = m.layer_snapshot();

        m.subdivide(2).unwrap();
        assert_eq!(m.size(), [32, 32, 32]);

        m.restore_layers(saved);
        assert_eq!(m.size(), [16, 16, 16]);
        assert_eq!(m.get(2, 3, 4), 5);
        assert_eq!(m.filled_count(), 1);
        let b = m.layer_bounds(0);
        let end = b.end();
        assert!((0..3).all(|a| end[a] <= 16), "{b:?} is outside the scene");
    }

    /// Shrinking the scene cuts every layer's box down to it, keeping whatever
    /// still fits at the same coordinates.
    #[test]
    fn resizing_the_scene_clamps_every_layer() {
        let mut m = VoxelModel::new(16, 16, 16);
        m.set(0, 0, 0, 1);
        let top = m.add_layer(0, "cover").unwrap();
        m.set_active_layer(top);
        m.set(1, 1, 1, 2);
        m.set(15, 15, 15, 5);
        m.set_layer_visible(0, false);

        m.resize(4, 4, 4);
        assert_eq!(m.size(), [4, 4, 4]);
        assert_eq!(m.layer_count(), 2);
        assert_eq!(m.layers()[1].name, "cover");
        assert!(!m.layers()[0].visible, "visibility survives too");
        assert_eq!(m.get_in(0, 0, 0, 0), 1);
        assert_eq!(m.get(1, 1, 1), 2);
        assert_eq!(m.filled_count(), 1, "the far corner no longer fits");
        for b in [m.layer_bounds(0), m.layer_bounds(1)] {
            let end = b.end();
            assert!((0..3).all(|a| end[a] <= 4), "{b:?} runs outside the scene");
        }
    }
}
