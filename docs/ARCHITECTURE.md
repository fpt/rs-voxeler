# Architecture

## Read-only model inspection

MCP checking and focused previews share a scoped occupied-cell collector. It
composites layers in stack order within the selected scope, filters effective
visibility (including ancestors), and includes descendants for object scopes.
The source-cell budget is 2,097,152; reports retain only bounded samples or the
largest components. A region limits the inspected cells, not a scene allocation.
Colour symmetry compares palette slots; connectivity ignores colour seams.
Neither asymmetric cells nor separate components are inherently defects.

Focused previews render a temporary editor with a shared camera frame for all
views. They ignore the live slice and cannot disturb visibility, history or
selection. Saved comparisons trim cloned models and compare native encodings,
including hidden geometry, hierarchy and palette without reopening a document.
Process/session and document identities let callers detect replacement before
continuing an old edit sequence; successful replacement clears transient cell
references. Polygon extrusion includes edge centres in either winding, and
validates all geometry before the existing atomic batch write path runs.

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

This is a tool for making models, not a game engine. Floats live freely here;
there is no VM and no entity system, and the project's investment goes into the
modelling surface instead — layers, objects, instances, selections, and an
agent-facing API precise enough to name a part rather than a coordinate.

A few notes below compare a decision with `rs-kessel`, which shares this
project's MCP and `attach` patterns. Those compare *mechanism* only.

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

### An object is what a thing is; a layer is how pixels combine

A layer does compositing — a grid, a box, a stack order, a visibility — and does
it well. It was also the only way to say "this is the left arm", and for that a
flat list of sixteen with no way to say one thing is *part of* another is the
wrong shape.

```text
Scene
 ├── robot
 │    ├── body      (layers: base, paint)
 │    └── left_arm  (layers: skin)
 └── sword          (layers: blade)
```

An `Object` is a name, a visibility and a parent, held in a **flat arena** with
parent indices rather than as nested children. Every structural change here is
already snapshot-based, and a flat list clones, compares and serialises with no
recursive walk; the tree shape is one field rather than a second structure to
keep in agreement with the first. Index 0 is a root that cannot be removed or
reparented, so "no object" and "the whole scene" are one answer.

**There is no transform on an object.** `move_object` applies its translation to
the layers' `Bounds::origin` at the moment it is asked. Layer boxes stay in scene
coordinates, so `get`, the raycaster and the extractor need to know nothing about
objects at all, and moving a part costs three `u16` per layer rather than a
re-voxelisation — the tree walk is the expensive half. A stored transform would
put a matrix inside the hottest read in the codebase to buy a generality nobody
asked for; when rotation arrives it should bake, the way `rotate_selection`
already does.

Three more rules, each with a failure mode on the other side:

- **A move is all or nothing.** The whole subtree is checked against the scene
  before any of it moves — half a robot moved and half left standing is worse
  than a move that did not happen — and the refusal names the layer and the axis,
  because an agent cannot see the scene edge.
- **Removing an object removes a label, never the work.** Its children and its
  layers move up to its parent. That also changes what is on screen, since a
  layer inside a hidden object is not any more, so it recounts. The drift test
  caught that and nothing else would have.
- **A cycle is refused in both directions.** `reparent_object` refuses a parent
  that is the object itself or anything under it; `set_objects` refuses a file
  whose chain loops. A file is not a caller.

`Layer::shown` is the layer's own flag **and** every object above it, cached and
recomputed by `refresh_shown` — the single place it moves, for the reason
`Layer::note` is the single place a tally moves. Compositing asks `shown`, never
`visible`, because walking to the root per lookup would put the depth of the tree
inside `get`. The layer's own flag is untouched, so showing an object again
restores exactly what was shown before.

### A move slides boxes; a rotation has to bake

`move_object` is a translation and therefore free: it slides each layer's
`Bounds::origin` and re-voxelises nothing, so moving a finished robot costs three
`u16` per layer. A rotation cannot be that. A quarter turn rewrites the grid, and
there is deliberately no per-object transform to hide it in — a stored one would
put a matrix inside `get`.

So `rotate_object` bakes, and borrows `Editor::rotate_selection`'s conventions
rather than inventing a second set: quarter turns, positive being the right-hand
rule about the positive axis (the convention face winding already uses), and a
pivot about the **low corner** rather than the centre. Centring is not
invertible — a quarter turn swaps two extents, and where those differ in parity
the centre falls between cells and rounds the same way every time, so a turn and
its inverse do not come back.

The one thing it has that a selection does not is **more than one grid**, and
that decides the pivot: the union of the whole subtree's occupied cells,
computed once and applied to every layer. A pivot per layer would turn each part
about its own middle, and the arm would leave the body. The occupied cells
rather than the boxes, because a box is a high-water mark and would drift the
turn by however much slack it happened to hold.

All or nothing, like a move: every layer's new cells are computed and checked
against the scene before any of them is written.

An instance cannot be turned. `Instance` holds an offset and a mirror and has
nowhere to keep a rotation, so `rebuild_instances` would silently undo one on the
next edit. Refused, pointing at the source — whose rotation turns every copy,
which is the answer the caller wanted anyway.

### An instance is a reference, and it is baked

`Object::instance` names another object, an offset and a per-axis mirror. The
question the design turns on is where that reference is *resolved*, and the
answer is: not in `get`.

Resolving it there — a true virtual reference, no duplicated storage — would put
a stored transform inside the hottest read in the codebase, and would need a
second path in every maintained tally, because the cells would belong to no
layer. What it buys is a byte per cell, which is the same order as the composite
grid rule already accepted at 4 MiB for a 64³ scene.

So an instance owns **one ordinary layer**, flagged `Layer::generated`, holding
the source subtree's composite; `rebuild_instances` rewrites it when the source
moves. Compositing, meshing, the raycaster, `owner_at`, `occupied_bounds` and
every tally needed no changes, because a derived layer is a layer.

One layer rather than one per source layer, because a rebuild must never change
the layer *count*: inserting or removing one renumbers the rest, and every
`Edit::layer` already on the undo stack would point at the wrong grid.

Derived cells are therefore never in the history. Undo restores the source and
the rebuild runs again, which is what keeps the copies in step rather than
restoring stale ones. `Editor::refresh_instances` asks for it at the commit
points — stroke, batch, undo, structural change — never inside `set_in`, where a
fill would pay for a source walk per voxel.

Placing is all or nothing and rebuilding clips, and the asymmetry is the point:
a placement is a thing a caller chose and can be told to choose differently; a
rebuild is a consequence of an unrelated edit, and refusing there would leave
every instance stale with nothing to press. `instance_clipped` reports it.

A write to a copy is refused with the source's name rather than detaching into
one silently, and `select_layer` is the single gate that enforces it: a derived
layer never becomes active, so every active-layer tool is off it for free.

### The panel is the tree, and the hit test is not an inverse

The layer panel listed layers flat, so the object tree was invisible at the
window. It now draws objects with their layers indented under them, with a
visibility switch and a fold on each object row.

The interesting part is `hud::panel_rows`. The flat panel's `layer_hit` was a
hand-written inverse of the drawing code — tested row by row, and a standing
invitation to drift. A tree makes that inverse harder: rows are no longer
`layers - 1 - n`, and the switch moves with the indent. So there is no inverse
any more. The row list is built once, and the drawing and the hit test both
index into it; they cannot disagree about what row four is.

`Editor::collapsed` is view state — not saved, not undoable, dropped with the
selection and clipboard on document replacement. Held by object index, so
removing an object can move a fold onto its neighbour. That is accepted: a fold
is not data, one click fixes it, and the alternative is an identity on `Object`
carried in the file format for the sake of a triangle in a panel.

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

### A region's *limit* is what decides click or stroke

`Span` says how far one edit reaches; `Tool` says what it does. A third thing
was hiding inside `Span` and pretending to be part of it: whether the edit is
applied once, on a click, or repeatedly as the pointer moves.

The rule read "any span but `Voxel` applies once". That names the wrong
property. What makes a fill unstrokeable is that it is *unbounded* — re-seeding
an unlimited flood several times a frame turns one intended fill into a
wandering pile of them. A flood with a radius has no such problem: it covers a
patch the size you asked for, and dragging one is how a surface gets worked.

So the seam is boundedness, and the brush supplies it. `Reach::brush` used to be
consulted only by `Span::Voxel`; it now bounds every span that grows, checked
during the flood beside `Reach::within` rather than filtered off the result — a
region trimmed at the end would spread round a corner and come back, which is
not what a disc on a surface means.

Radius 0 means two different things, and they never collide: to `Span::Voxel`,
which does not grow, it is the seed cell alone; to a flood it is "no bound".

This is also the groundwork a surface brush needs. "Raise a patch of surface by
one" is `Span::Plane` + `Build` with a radius — the existing planar flood,
limited — rather than a second surface walk written beside the first.

### `Drag::plane` is load-bearing, and its old test did not show it

`a_build_drag_stays_on_the_plane_it_started_on` passes with the pin **removed**.
Its fixture is a bare floor, where `Drag::before`'s mask already stops a stroke
re-targeting onto its own work, so the test never exercised the pin.

Measured on a floor with a step that was there *before* the stroke — geometry
the mask cannot hide, because it is not the stroke's work:

```text
pin present   levels the drag built on = {3}
pin removed   levels the drag built on = {1, 3}
```

at every camera pitch tried. So the pin does something the mask cannot: it keeps
a build drag on the plane it began on when other geometry passes under the
pointer. It stays, and
`a_build_drag_stays_on_its_plane_over_geometry_that_was_already_there` fails
without it.

The consequence for a surface brush is that the pin cannot simply be dropped to
let a patch follow curvature — it will need replacing with something
surface-aware, not deleting.

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

### Colour is a way of naming a part

"All the red" is a part in a way "all the cells in this box" is not.
`select_by_color` turns that into a selection the transforms already consume, and
`count_by_color` is the observation an agent was missing — it could see the
picture and count cells, and could not ask what the model was made of.

- **Index 0 is air and every palette operation refuses it.** `slot_arg` is
  deliberately a different argument helper from the drawing tools' `color`: they
  take 0 and erase with it, where "replace 0" means every empty cell and would
  fill the model.
- **`swap_colors` swaps the voxels, not the palette entries.** Both readings put
  the same picture on screen; only this one leaves an index meaning the colour it
  meant, so a brush set to it still paints that colour.
- **`compact_palette` touches both halves**, so it goes through the snapshot
  path — which is why `Snapshot` carries the palette. Recorded as cell edits
  alone, an undo would put the voxels back pointing at colours that had moved. It
  reports the mapping, because every index a caller was holding is stale.

That last one is also why `Change::Layers` boxes its snapshots: with the palette
in, the variant outweighs the others by two orders of magnitude, and the undo
stack is overwhelmingly `Cells`.

### Structural changes store the stack whole

`History` holds a `Change`: cells, a palette entry, or layers. A cell edit names
the layer it landed on, and removing or reordering a layer renumbers the ones
around it — so every edit already on the stack would start pointing at the wrong
grid. `History::restructure` therefore snapshots the whole layer stack either
side of the change, the **object tree** included: removing an object renumbers
objects exactly the way removing a layer renumbers layers, and a snapshot that
restored the stack without the tree would file every layer under the wrong part
of it. It costs a copy of the model per structural step, which
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

### Selecting by hand: a fifth tool, and two grains

`Tool::Select` is a tool rather than a modifier because a drag already means
"apply the current tool", and span and brush then compose with it as they do
with the other four. It never writes, so `app.rs` handles its click directly
rather than through a stroke — a choice, costing no undo — and `begin_stroke`
returns early on it as a floor under that.

`O` switches grain. Cells go through the same `region` walk the drawing tools
use. Objects set `Editor::selected_object`, an object index and deliberately not
a `Selection`: a selection is cells on one layer, an object spans several, and
gathering a part into one would take only the active layer's share and tear it
in half on the first move. The transforms it feeds are `move_object` and
`rotate_object`, which carry a subtree all-or-nothing.

The arrows move whatever is *selected* rather than whatever the mode says, and
flip is cells only — an object has no stored transform to hold a reflection, and
`create_instance`'s `mirror` is where that lives.

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

`.vxm` is `VXM4`: a scene range, the object table, a layer count, the active
layer, then per layer its flags, its object, name, origin, size and own sparse
voxel list. Coordinates are relative to the layer's origin, so one byte covers a
layer anywhere in the scene; an object's parent is stored as `parent + 1`, so the
root's "no parent" is a zero rather than a sentinel that could be read as object
0. `VXM3` (no objects), `VXM2` (layers, no boxes) and `VXM1` (no layers) all
still load — an older file arrives as a single root object holding every layer,
and the two oldest are additionally **trimmed** on the way in, so they gain the
smaller shape by being opened. A file already on disk is not free to rewrite
itself, and that rule has now held four times.

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

### Procedural branches carry their end radii

`put_tapered_line` (and `tapered_line` in `apply_edits`) samples a cone frustum
along an arbitrary segment. The radius varies linearly from `radius_from` to
`radius_to`; a zero radius is a pointed tip, equal radii are a cylinder. The end
planes follow the segment axis, so angled branches do not acquire upright tips.
Coincident endpoints and two zero radii are refused; the existing rounded
`put_line` still makes capsules and coincident-endpoint spheres.

The shape lives in the MCP modeling module, not the renderer or the model. It
expands only its clipped bounding box, under the shared candidate-cell budget,
then joins the same validate-first, one-undo-step pipeline as all batch shapes.
This was driven by a five-pronged accessory: adjusting its thickness and tip
direction had required over a thousand individual voxel operations. Two joined
segments can now keep a branch thick until close to its tip without a custom
voxel generator.

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
