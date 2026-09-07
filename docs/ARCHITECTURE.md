# Architecture

How rs-voxeler is put together, and why each load-bearing piece is the shape it
is. Decisions are recorded with the reasoning that produced them and, where one
was settled by running it, the numbers that settled it.

`CLAUDE.md` at the repository root is the exhaustive rule-by-rule companion to
this document. This one is the map; that one is the legend.

## The three crates

```text
voxeler  (the window, an MCP server over stdio or SSE, and `attach`)
   │
   ├── voxel-render   faces → transform → clip → raster → z-buffer → framebuffer
   │
   └── voxel-core     the layer stack, the palette, undo/redo, the file formats
```

**The dependency only points downward, and that is enforced by what the lower
crates are missing.** `voxel-core` has no vector type at all — the raycaster
takes `[f32; 3]` arrays specifically so a `Vec3` cannot creep in and quietly
make the model depend on the renderer. `voxel-render` knows nothing about
editing: it is handed a model and a camera and produces pixels.

Everything about *what a click means* lives in `voxeler/src/editor.rs`. That is
why `editor.rs` carries the bulk of the tests and `app.rs` — winit events in, a
blitted framebuffer out — has almost none. The split is deliberate: the part
worth testing is the part with rules in it, and the part with a window in it
cannot be tested without one.

This is the 3D successor to `rs-kessel` but **not** an extension of it. Kessel's
VM is a deterministic integer machine; carrying that constraint into a
projection matrix would buy nothing, so floats live freely here. When a Luax
bridge eventually appears, the integer boundary sits at *that* edge.

## The data model

### A scene is a range; a layer is a box

`VoxelModel::size` says where voxels *may* go — what the work plane spans, what
the camera frames, what a ray is clipped to. **Nothing of that size is
allocated.** Each layer carries a `Bounds` (origin + size) and allocates only
that.

That is the difference between a 64×64×5 ground, a 16×32×16 tree and a 16³
character costing 32 769 cells in a shared 64³ scene, or costing 1 048 576.

Three rules keep the box invisible in ordinary use:

- **It grows to fit.** `set_in` enlarges the box before writing, so a new layer
  starts empty and nothing has to be sized before it is drawn in. A declared box
  is a starting size, not a wall.
- **Writing air outside it does not grow it.** An erase that missed has nothing
  to record, and it is the one way a box could grow without gaining anything.
- **It shrinks only when asked** (`trim_layer`) — *except* when the layer becomes
  empty, which hands the box back at once. The high-water mark exists so erasing
  and redrawing in one spot does not reallocate every stroke; an empty layer has
  nothing to churn, and holding its old extent is pure waste. Measured: a new
  model seeds one voxel at the scene's centre, so erasing the seed and building
  near the floor used to keep a box spanning both — 26× the size of the work,
  487 MB against 35 MB at 256³.

The **composite cache is gone**, and had to be: it was scene-sized, which is
precisely the allocation this design exists to avoid. `get` walks the layers
top-down instead — sixteen bounds tests against one load. That would be a bad
trade if anything still swept the scene volume, so nothing does: `extract` and
`iter_filled` walk the *layers*. Anything tempted to loop `for z for y for x`
over `model.size()` is now the thing to be suspicious of.

Where two layers overlap, the cell belongs to whoever is on top (`owner_at`).

### A layer is a grid of its own

Layers composite top down: `get` returns the topmost *visible* layer's index at
a cell. A grid per layer rather than an owner tag per cell, because that is the
difference between hiding the armour and seeing the body underneath, and hiding
the armour and seeing a hole. `MAX_LAYERS` (16) bounds the cost.

The consequence to keep in mind is that `get`, `iter_filled`, `filled_count` and
`occupied_bounds` are all *composited*, which is why the raycaster, the mesh
extractor and the `.vox` exporter needed no changes when layers arrived. The
per-layer views are `get_in`, `set_in` and `iter_filled_in`, and
`format::native` is the one place that **must** use them — a save writes each
layer's own grid, or hiding a layer and saving would silently delete it.

### Tallies are maintained, not walked

`VoxelModel::filled_count`, `Layer::filled_count` and `Layer::occupied` are all
O(1)-ish because the answers are asked for far more often than they change:
every MCP tool call reports a count, the status line reads one per redraw, the
layer panel reads a per-layer one per row per redraw, and every screenshot
frames — which asks for the occupied box.

Measured before fixing, at 256³: a single voxel edit cost 43 ms and a screenshot
after one cost 348 ms. Both are under 15 ms now.

A **count** can be maintained by adding and subtracting one. A **box** cannot:
erasing the cell that was furthest out has to find the next furthest, and
nothing short of a walk knows where that is. So `Layer::planes` counts filled
cells per plane of each axis — a write touches three counters, and the tight box
is the first and last non-zero plane on each axis, a scan of a few hundred
numbers rather than of sixteen million cells. Exact, not approximate, and it
shrinks the moment the last cell of a plane goes.

Three rules keep the tallies honest:

- **`Layer::note` is the only place a single cell moves a tally**, so `filled`
  and `planes` cannot drift apart by one being updated and the other forgotten.
- **Paths that replace the whole array tally as they build it**, never by a
  second walk over the result. `reshape` is called on every write outside a
  growing box, so a second pass there cost three seconds on a full 256³ fill.
- **`the_maintained_count_never_drifts_from_a_fresh_walk`** runs every operation
  that can change what is visible and checks every cheap answer against the
  expensive one — including erasing the furthest cell on each side, so the box
  has to shrink from both ends.

### Growing a box is a copy, never a recount

The same reasoning one level down. A box that only *grew* holds the same cells
in the same order, so a row of x is contiguous in both arrays and copies whole
with `copy_from_slice`; `filled` stands unchanged; and each axis' plane counts
are the old ones shifted by however far that origin moved.

The general path — which a trim or a scene resize needs, because those can drop
cells — walks every cell computing a division and two remainders. Filling a 256³
scene grows the box about 768 times, so that walk ran over 2.1 billion cells.

```text
bulk fill of a whole 256³ scene
  before   5027 ms
  after     319 ms          box declared up front: 134 ms
```

The policy deliberately did **not** change. Growing the box geometrically, as
`Vec` does, would buy the remaining 2.4× by making every box up to 1.5× too big
on each axis — and a tight box is the guarantee this design exists to make. The
16× was available without giving that up, so it was taken and the rest was not.

## The edit model

### What a tool does and how far it reaches are separate

`Tool` says add, remove or recolour; `Span` says one cell, a run, a face, or a
connected part. They are independent, so four tools and four spans are twelve
useful operations out of one flood fill rather than twelve tools each carrying
their own copy of it. Erase gets the three reaches for free, which is the sign
the factoring is right.

A region is **bounded by colour**: plane and volume grow over cells holding the
same palette index as the one under the cursor. On a single-colour model that is
identical to "every connected voxel", so the rule costs nothing there and is
what makes a multi-colour model editable.

`region::cells` returns *candidates*, not writes. Whether a cell is actually
touched is the tool's rule, applied in `Editor::continue_stroke`.

### A drag is one undo step, aimed at the model it started on

`Stroke` collects edits across events and `History::push` commits them as one
batch on mouse-up — which is why `Stroke` is separate from `History` at all: a
closure-scoped transaction cannot span three event callbacks.

Two separate rules stop a stroke from re-targeting onto its own work, and they
are both needed because they fail differently:

- **`Drag::before` + `raycast::cast_masked`** make a stroke's own placements
  invisible to its own aim. Without it, placing a voxel puts a new side face
  under the pointer and the next event builds again — **one click added two
  voxels**, which shipped.
- **`app::is_click`'s dead zone** stops a click becoming a drag at all. Near the
  horizon one pixel of the work plane really is a whole cell away, so that second
  placement is geometrically correct and still not what anyone meant.

### Structural changes store the stack whole

`History` holds a `Change`: cells, a palette entry, or layers. A cell edit names
the layer it landed on, and removing or reordering a layer renumbers the ones
around it — so every edit already on the stack would start pointing at the wrong
grid. `History::restructure` therefore snapshots the whole layer stack either
side of the change. It costs a copy of the model per structural step, which
happens a handful of times a session; the alternative is stable layer ids and a
resurrection path for a deleted one, which is a great deal of machinery for the
same guarantee.

`Snapshot` carries the scene's **size** as well as the layers. It did not at
first, which was fine until `subdivide` existed: undoing one would have restored
layers at twice their coordinates into a scene half the size.

Visibility is deliberately *not* in the history — it is toggled constantly while
working, and undo would spend its first few presses turning layers back on.

### Mirroring reflects the edit, not the model

`Editor::mirror` is three independent planes; two give four copies of a stroke
and three give eight. What is reflected is the set of cells the edit *wrote*, so
nothing is touched that the stroke did not reach. Turning mirroring on does not
symmetrise what is already there, and a deliberately lopsided model stays
lopsided while you work on it symmetrically — which is the whole point, and the
reason this is not a "symmetrise" command over the grid.

### A selection is cells, on one layer, and not part of the document

Four decisions, each with a failure mode on the other side: **cells, not a box**
(a box carries the air inside it and would erase whatever it landed on);
**one layer, named at selection time** (a selection that followed the *current*
active layer would move a different set of voxels than the user was shown);
**never empty** (`None` rather than an empty `Selection`, so "is anything
selected" has one answer); and **dropped on undo** (it names coordinates, and an
undo changes what is at them).

`transform_selection` is the shape every transform has: read every colour
**before anything moves**, then emit the clears ahead of the writes in one
`apply_writes`. A transform whose result overlaps its source would otherwise
carry a voxel along or erase its own arrival.

**A rotation pivots about the low corner, not the centre.** Centring reads
better and is not invertible: a quarter turn swaps two extents, and where those
differ in parity the centre falls between cells and has to be rounded — the same
way each time, so it accumulates. `rotate(+1)` then `rotate(-1)` came back a
whole cell out, which is how this was found.

## The renderer

Faces, not rays. The renderer turns voxels into quads and rasterizes them, which
costs work proportional to the model's *surface* where ray casting costs work
proportional to the *screen*. Only faces touching air are emitted, so a solid
64³ block draws 24 576 quads rather than 1.5 million. Coplanar neighbours are
**not** merged: greedy meshing is the obvious next win, but it changes the quads'
extents, so it has to be built on an extractor already known to be right.

Four invariants the rasterizer depends on:

- **Clip before the divide.** A vertex behind the camera has a negative `w`, and
  dividing by it mirrors the vertex across the screen. `clip_near` runs in
  homogeneous space against `z + w ≥ 0`.
- **NDC z is linear in screen space**, which is what makes barycentric depth
  interpolation exact rather than merely close.
- **Winding is derived, not tabulated.** For a face on axis `a`, the other two
  axes in cyclic order satisfy `e_b × e_c = e_a`. Six hand-written corner lists
  would be six chances to get one backwards, and a backwards one is invisible
  until back-face culling eats half the model.
- **Gizmos are lifted, not depth-biased.** A highlight is nudged 0.01 voxel along
  the face normal; a depth bias has to be tuned against the near/far ratio and a
  value that works at arm's length fails when you zoom in.

### A rebuild is proportional to the edit, not to the model

A full extraction at 256³ costs about 300 ms and almost every change is one
voxel, so the mesh is kept **per chunk** (`CHUNK` = 16 — a dense one is 4 KiB,
where 8 multiplies the bookkeeping and 32 makes one voxel's work touch
thirty-two thousand cells) and only the chunks the model says changed are walked
again.

`VoxelModel::dirty` is a **bitset** over the chunk grid, not a set of
coordinates: a fill writes sixteen million cells, and sixteen million hash
inserts would cost more than the rebuild they were meant to save. At 256³ the
whole thing is 4 096 bits.

`set_in` marks the chunk a cell fell in **and the neighbour across a shared face
when the cell sits against one**, because a face belongs to the cell that emits
it. Anything not attributable to cells — a slice moving, a layer hidden, a
subdivide — calls `dirty_all`.

A chunk no layer reaches is free, which is what keeps a sparse scene cheap — but
the test has to ask whether the layer is *empty* as well as where its box is. A
cleared layer keeps its size until someone trims it, and without that test a
cleared 256³ scene swept every cell it used to have to find nothing. It did,
briefly: **19 ms became 40 ms** before the check went in, and 6 ms after.

## The formats

**Dense in memory, sparse on disk.** Editing wants `set` to be a store and face
extraction wants a neighbour lookup to be a load, and both are O(1) on a dense
array. The *file* is sparse, because a model is mostly air.

`.vxm` is `VXM3`: a scene range, a layer count, the active layer, then per layer
its flags, name, origin, size and own sparse voxel list. Coordinates are relative
to the layer's origin, so one byte covers a layer anywhere in the scene. `VXM2`
(layers, no boxes) and `VXM1` (no layers) still load and are **trimmed** on the
way in — an old file gains the smaller shape by being opened, and a file already
on disk is not free to rewrite itself.

**Y is up, and `.vox` is not.** MagicaVoxel is Z-up, so `format::vox` converts —
and the conversion is a **rotation**, `(x, y, z) → (x, z, sy-1-y)`, not a swap.
Exchanging Y and Z has determinant −1, so it loads every model mirrored: a right
hand becomes a left hand and text reads backwards. That is the single easiest bug
to ship here, and `import_does_not_mirror` is the test that stops it. The other
trap is that the `RGBA` chunk's entry *i* is palette index *i + 1*, because index
0 is air.

An export is a **flatten** — `.vox` has nowhere to put a stack, and doing
otherwise means the nTRN/nGRP/nSHP scene graph, which is the part of that spec
most likely to be got subtly wrong.

## The agent surface

### Two transports, one dispatcher

`voxeler mcp` is **stdio** and headless; `voxeler FILE --mcp` is **SSE** from a
window that is already open. They answer different questions — the first is
started by the agent, so "is it running?" never comes up; the second is for when
you were editing and want an agent to join you — and they differ in exactly one
place, `mcp::ToolHost`:

- `Context` (SSE) queues the call for the winit event loop and waits.
- `Direct` (stdio) owns the editor and runs it under a mutex.

Everything else is `mcp::dispatch`, written once. The queue is what keeps the
editor the single-threaded thing every other module assumes: a tool call lands
*between* two frames. A mutex is sound in the stdio server precisely because
there is no event loop there to be caught mid-frame.

The HTTP is hand-rolled for the reason the PNG writer and the 5×7 font are: the
alternative is an async runtime inside a program that is one blocking event loop.
`serde_json` is *not* hand-rolled, because a JSON parser is the one piece here
worth buying.

Three rules the ~34 tools depend on:

- **A whole tool call is one undo step.** The user shares this history, and a box
  an agent filled must cost them one `ctrl+Z` rather than five hundred.
- **Every edit reports four exclusive outcomes that sum to `targeted`** — added,
  removed, repainted, unchanged. An agent cannot see the screen, so a tool that
  says "ok" has told it nothing, and a tool whose numbers do not add up has told
  it something false.
- **`screenshot` is the exception**, and the reason `Content::Image` exists.
  Counts cannot tell an agent the arm is on backwards.

### A region can be read from one layer or from the composite

`Reach::layer` is `None` for the composite and `Some(n)` for one layer's own
grid. A click passes `None` — you select what you can see. A *coordinate* passes
`Some(active)`, and the difference is not cosmetic: growing a region on the
composite and writing it to the active layer copies another layer's shape onto
this one. Driving the real binary produced exactly that — a fill reported "100
added", left the slab untouched, and put a hundred-cell ghost on the layer above.

### The file tools are confined to a set of directories

An MCP server is driven by a model reading content nobody vetted, so `mcp::Roots`
is a boundary rather than a convenience. Four rules, all load-bearing:

- **Absolute paths are accepted, and that is the point.** They were refused at
  first, on the reasoning that a relative path under a known root is unambiguous.
  It is not: a desktop MCP client spawns its servers with whatever working
  directory the *app* had. Reported from use.
- **`..` is refused lexically**, before anything touches the filesystem, and
  *every* `..` rather than only the escaping ones. `a/../b` is harmless and
  `../b` is not, and telling them apart after the fact is exactly the reasoning
  that goes wrong.
- **An absolute path is checked against its canonical form**, via
  `canonical_enough`, which canonicalises the deepest ancestor that exists and
  puts the rest back — `save_model` names a file that does not exist yet. A
  prefix test on the name alone would be satisfied by a symlink inside a root
  pointing anywhere at all.
- **A symlink anywhere below a root is refused outright**, on top of both. It
  survives the canonical test when it points inside, and it can be repointed
  outside between the check and the write.

Listeners bind `127.0.0.1`, never `0.0.0.0`. Under `--mcp` the roots are empty
and **all** the file tools are refused, `save_model` included: you opened that
document yourself, and an agent there has no business writing it. `voxeler mcp`
with no argument refuses to root at the filesystem root, which is what a desktop
client's working directory often is.

### `voxeler attach` sends the model, not the picture

`kessel attach` streams framebuffers, because its console renders a 320² indexed
screen and 57 KiB a frame over loopback is nothing. This renders up to 1.4
million pixels — 5 MiB a frame, hopeless. So the *model* crosses the wire and the
client renders it, which also puts the camera where it belongs.

The bytes are `format::native::encode` — the same sparse `.vxm` a save writes, so
a model is a few kilobytes and carries its layers, palette and names with no
second encoding to keep in agreement. A revision counter makes "nothing changed"
a single byte, so there is nothing a diff would buy.

**Client-driven**: the server never pushes, so with nobody attached it does no
work at all — which is what makes attaching something you can do halfway through
a build without having changed what the session would have done. It is a
**viewer, not a second editor**: one document, one history, both in the server.

Liveness is decided by **connecting**, never by a pid: a pid can be reused and a
killed server leaves its session file behind. That means every discovery leaves a
connection that says nothing, so `protocol::read_hello` returns `Ok(None)` for a
peer that hangs up before speaking — treating it as an error made the server log
a failure every time anyone ran `voxeler attach`.

## Decisions that measurement reversed

The pattern worth keeping: measure or reproduce first, and be willing to have the
answer contradict the plan.

| Believed | Measured | Outcome |
| --- | --- | --- |
| Sparse chunk storage would fix large scenes | Compact content already 1.0× the ideal; only thinly-scattered content in one layer is bad, a shape layers already avoid | Storage swap **declined**; the wins were elsewhere |
| A single-voxel edit at 256³ was a storage cost | 43 ms, all of it O(n) tallies | Maintained tallies; under 15 ms |
| A screenshot after an edit was a render cost | 348 ms, dominated by the framing walk | `occupied_bounds` from plane counts; under 15 ms |
| The extraction regression was in the mesh | A cleared layer keeps its box, so every chunk looked occupied | `is_empty` test in `touches_a_layer`; 40 ms → 6 ms |
| A bulk fill was slow because the box grows | True, but the cost was the per-cell division in the *copy*, not the number of copies | Copy-only growth path; 5027 ms → 319 ms, box policy unchanged |
| The plane tallies were the 3-second `reshape` regression | It was the second walk added to tally | Tally inside the existing copy loop |
