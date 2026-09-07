# rs-voxeler — Developer Guide

## Overview

A voxel model editor and the software renderer under it. This is the 3D
successor to `rs-kessel`, but it is **not** an extension of it: Kessel's VM is a
deterministic integer machine, and carrying that constraint into a projection
matrix would buy nothing. Floats live freely in this workspace. When a Luax
bridge eventually appears, the integer boundary sits at *that* edge — not
inside the renderer.

The project order is asset → renderer → one game → only the API that game
needed. Steps 1–3 exist; steps 4–7 (scene graph, Luax bridge, first game,
then collision/chunks/animation) do not.

## Architecture

```text
voxeler  (the window, an MCP server over stdio or SSE, and `attach`)
   │
   ├── voxel-render   faces → transform → clip → raster → z-buffer → framebuffer
   │
   └── voxel-core     the layer stack, the palette, undo/redo, the file formats
```

The dependency only points downward. `voxel-core` knows nothing about drawing —
it does not even have a vector type, and the raycaster takes `[f32; 3]` arrays
so it stays that way. `voxel-render` knows nothing about editing. Everything
about *what a click means* is in `voxeler/src/editor.rs`, which is why that file
has tests and `app.rs` has almost none.

### A scene is a range; a layer is a box

`VoxelModel::size` says where voxels *may* go — what the work plane spans, what
the camera frames, what a ray is clipped to. **Nothing of that size is
allocated.** Each layer carries a `Bounds` (origin + size) and allocates only
that, which is what lets a 64×64×5 ground, a 16×32×16 tree and a 16³ character
share a 64³ scene for 32 769 cells instead of 1 048 576.

Three rules make the box invisible in ordinary use:

- **It grows to fit.** `set_in` enlarges the box before writing, so a new layer
  starts empty and you never size one before drawing in it. A declared box is a
  starting size, not a wall.
- **Writing air outside it does not grow it.** An erase that missed has nothing
  to record, and it is the one way a box could grow without gaining anything.
- **It shrinks only when asked** (`trim_layer`). Erasing and redrawing in one
  spot would otherwise reallocate the layer on every stroke.

The **composite cache is gone**, and had to be: it was scene-sized, which is
precisely the allocation this design exists to avoid. `get` now walks the layers
top-down — sixteen bounds tests against one load. That would be a bad trade if
anything still swept the scene volume, so nothing does: `extract` and
`iter_filled` walk the *layers*, which is a far larger saving than the lookup
gives back. Anything tempted to loop `for z for y for x` over `model.size()` is
now the thing to be suspicious of.

Where two layers overlap, the cell belongs to whoever is on top (`owner_at`) and
only that layer emits it — otherwise a shared cell meshes twice and the lower
copy z-fights the upper one.

### A layer is a grid of its own

Layers composite top down: `VoxelModel::get` returns the topmost *visible*
layer's index at a cell. A grid per layer rather than an owner tag per cell,
because that is the difference between hiding the armour and seeing the body
underneath, and hiding the armour and seeing a hole. The cost is a byte per cell
per layer, which `MAX_LAYERS` (16) bounds at 4 MiB even at 64³.

Compositing on every read would make face extraction and the raycaster
O(layers) in their hottest loops, so the result is cached in `composite` and
repaired **one cell at a time** by `set_in` — writes happen at the speed of a
hand, reads at the speed of a frame. Every structural change ends in a full
`recomposite` instead, because there is no structural change whose effect is
cheaper to work out than to recompute.

The consequence worth keeping in mind is that `get`, `iter_filled`,
`filled_count` and `occupied_bounds` are all *composited*, which is why the
raycaster, the mesh extractor and the `.vox` exporter needed no changes at all
when layers arrived. The per-layer views are `get_in`, `set_in` and
`iter_filled_in`, and `format::native` is the one place that must use them: a
save writes each layer's own grid, or hiding a layer and saving would silently
delete it.

`VoxelModel::active` — which layer `set` writes to — lives on the model rather
than in the editor. It is a property of the document (reopening a file should
put you back where you left off), it belongs in the file, and it is what let
every existing caller of `set` keep working without learning what a layer is.

### The editor writes to the active layer, and only to it

Three rules, all in `Editor::continue_stroke` and its neighbours:

- **The air/solid test asks the active layer, not the composite.** Testing what
  is on screen would refuse to build under a voxel a higher layer is showing —
  which is exactly what a lower layer is for.
- **A region is grown on the composite and written to the active layer.** You
  point at what you can see, so that is what a fill selects; what it changes is
  then narrowed by the rule above. With one layer, or while working on the layer
  you are looking at, the two sets are identical.
- **A click that changed nothing says why.** `Drag::owner` carries the layer
  that owned the voxel the stroke started on, and `end_stroke` reports it when
  the stroke was empty. A tool acting only on the active layer is a rule; a tool
  that ignores you with no explanation is a bug report.

### Structural changes store the stack whole

`History` holds a `Change`, which is either cells or layers. A cell edit names
the layer it landed on (`Edit::layer`), and removing or reordering a layer
renumbers the ones around it — so every edit already on the stack would start
pointing at the wrong grid. `History::restructure` therefore snapshots the whole
layer stack either side of the change. Undo puts the exact numbering back, which
is what lets the older cell edits keep meaning what they meant.

It costs a copy of the model per structural step. Structural steps happen a
handful of times in a session, and the alternative — stable layer ids, and a
resurrection path for a deleted one — is a great deal of machinery for the same
guarantee.

Visibility is deliberately *not* in the history. It is a thing you toggle
constantly while working, and undo would spend its first few presses turning
layers back on instead of undoing the edit you wanted back. It still dirties the
document, because which layers you had hidden is part of the model.

### Two transports, one dispatcher

`voxeler mcp` is **stdio** and headless; `voxeler FILE --mcp` is **SSE** from a
window that is already open. They answer different questions — the first is
started by the agent, so "is it running?" never comes up; the second is for when
you were editing and want an agent to join you — and they differ in exactly one
place, `mcp::ToolHost`:

- `Context` (SSE) queues the call for the winit event loop and waits.
- `Direct` (stdio) owns the editor and runs it under a mutex.

Everything else — the methods, the errors, the notification-gets-no-reply rule —
is `mcp::dispatch`, written once. A mutex is sound in the stdio server precisely
because there is no event loop there to be caught mid-frame; adding one to the
windowed server would put a lock around every field the window layer touches, for
no gain.

### `voxeler attach` sends the model, not the picture

`kessel attach` streams framebuffers, because its console renders a 320² indexed
screen and 57 KiB a frame over loopback is nothing. This renders up to 1.4
million pixels — 5 MiB a frame, hopeless. So the *model* crosses the wire and
the client renders it, which also puts the camera where it belongs: orbiting is
the viewer's business, and a view needing a round trip per mouse move would be
unusable.

The bytes are `format::native::encode` — the same sparse `.vxm` a save writes, so
a model is a few kilobytes and carries its layers, palette and names with no
second encoding to keep in agreement. A revision counter makes "nothing changed"
a single byte, so there is nothing a diff would buy.

Three properties hold it together:

- **Client-driven.** The server never pushes. With nobody attached it does no
  work at all, which is what makes attaching something you can do halfway
  through a build without having changed what the session would have done.
- **A viewer, not a second editor.** One document, one history, both in the
  server. A viewer that could also edit would need a rule for two simultaneous
  writers, and the honest ones are all worse than "the picture is live and the
  keyboard is the agent's". `Editor::viewing` is what `app.rs` refuses input on
  and what the HUD says `VIEWING` for.
- **`Editor::show`, not `Editor::open`.** A viewer takes a new model several
  times a second, and `open` re-frames the camera — which would wrench the view
  out of the watcher's hands on every update.

Liveness is decided by **connecting**, never by a pid: a pid can be reused and a
killed server leaves its session file behind. That means every discovery leaves
a connection that says nothing, so `protocol::read_hello` returns `Ok(None)` for
a peer that hangs up before speaking. Treating that as an error made the server
log a failure every time anyone ran `voxeler attach`.

### The file tools are confined to a root

`mcp::Root` resolves `open`/`save` paths inside one directory and refuses to
leave it. The check is **lexical** — `..` components and absolute paths are
rejected before anything touches the filesystem — because a check made by
canonicalising the result has already followed whatever symlink was there. Every
`..` is refused rather than only the escaping ones: `a/../b` is harmless and
`../b` is not, and telling them apart after the fact is exactly the reasoning
that goes wrong.

An MCP server is driven by a model reading content nobody vetted. Under `--mcp`
the root is empty and the file tools reach nothing at all: you opened that
document yourself, and an agent there has no business opening another.

### The agent and the user share one editor, through a queue

`--mcp` serves the editor over MCP's **SSE** transport, on loopback, while the
window stays open. `kessel mcp` speaks stdio because a console an agent is
debugging needs no window; this is the opposite case, and the whole point of
driving a *model editor* from an agent is that a person can watch and object.
Stdio would own the terminal and give the agent a process of its own.

They do **not** share the editor through a lock. The HTTP threads put jobs on a
channel and the event loop runs them in `user_event`, against the `Editor` it
already owns. A `Mutex<Editor>` would work, but it would wrap every field the
window layer touches and leave open what a tool call does mid-frame. The queue
answers that: a tool call lands *between* two frames, and the editor stays the
single-threaded thing every other module assumes. Waking the loop needs an
`EventLoopProxy`, because `ControlFlow::Wait` would otherwise hold the call
until the next mouse move.

`crates/voxeler/src/mcp/` is `wire.rs` (JSON-RPC types), `http.rs` (enough
HTTP/1.1 for SSE), `tools.rs` (the tools, and the only file that touches the
model) and `mod.rs` (the listener and the bridge). The HTTP is hand-rolled for
the reason the PNG writer and the 5×7 font are: the alternative is an async
runtime inside a program that is one blocking event loop. `serde_json` is *not*
hand-rolled, because a JSON parser is the one piece here worth buying.

Three rules the MCP surface depends on:

- **Loopback only, and no `save`.** The listener binds `127.0.0.1`; the tools
  can edit the document but cannot write it to disk, cannot delete a layer, and
  cannot move the camera. An agent that could overwrite the user's file would
  make "watch it happen" pointless.
- **A whole tool call is one undo step.** The user shares this history, and a
  box an agent filled must cost them one `ctrl+Z` rather than five hundred.
  `Editor::apply_batch` is the entry point for an edit that did not come from a
  click — no ray, no span, no mirror, just cells.
- **Every edit reports four exclusive outcomes that sum to `targeted`.** Added,
  removed, repainted, unchanged. An agent cannot see the screen, so a tool that
  says "ok" has told it nothing; a tool whose numbers do not add up has told it
  something false.
- **`screenshot` is the exception, and the reason `Content::Image` exists.**
  Counts cannot tell an agent the arm is on backwards. It frames the contents,
  drops the work-plane grid (a grid is for aiming a mouse, and there is no
  mouse), and puts the camera back — under SSE that camera is the one the user
  is looking at, and moving it would be reaching through the screen. The base64
  is hand-rolled next to the PNG writer, for the same reason.

### A region can be read from one layer or from the composite

`Reach::layer` is `None` for the composite and `Some(n)` for one layer's own
grid. A click passes `None` — you select what you can see. A *coordinate* passes
`Some(active)`, and the difference is not cosmetic: growing a region on the
composite and writing it to the active layer copies another layer's shape onto
this one. Driving the real binary produced exactly that — a fill seeded on the
slab reported "100 added", left the slab untouched, and put a hundred-cell ghost
on the layer above. `mcp::tools`' `fill` now reads the active layer and refuses
a seed another layer owns, naming the layer to select instead.

### Dense in memory, sparse on disk

A 64³ grid is 256 KiB, small enough that the simplest representation wins:
editing wants `set` to be a store and face extraction wants a neighbour lookup
to be a load, and both are O(1) on a dense array. The *file* is sparse, because
a model is mostly air and the on-disk shape is the one that has to stay small.

### Y is up, and `.vox` is not

The grid is right-handed with +Y up. MagicaVoxel is Z-up, so `format::vox`
converts — and the conversion is a **rotation**, `(x, y, z) → (x, z, sy-1-y)`,
not a swap. Exchanging Y and Z has determinant −1, so it loads every model
mirrored; a right hand becomes a left hand and text on a model reads backwards.
That is the single easiest bug to ship here, and `import_does_not_mirror` is the
test that stops it.

The other `.vox` trap is that the `RGBA` chunk's entry *i* is palette index
*i + 1*, because index 0 is air. Off by one there is a model whose every colour
is one slot out — subtle enough to reach a release.

### Faces, not rays

The renderer turns voxels into quads and rasterizes them. That costs work
proportional to the model's *surface*, where ray casting costs work proportional
to the *screen*; for one 64³ model in a window the surface is far smaller, and
it is also the shape a GPU would want later. Only faces touching air are
emitted, so a solid 64³ block draws 24 576 quads rather than 1.5 million.

Coplanar neighbours are **not** merged. Greedy meshing is the obvious next win,
but it changes the quads' extents, so it has to be built on an extractor already
known to be right.

### The rules the rasterizer depends on

- **Clip before the divide.** A vertex behind the camera has a negative `w`, and
  dividing by it mirrors the vertex across the screen — an unclipped triangle
  draws as a wild wedge rather than as its visible part. `clip_near` runs in
  homogeneous space against `z + w ≥ 0`.
- **NDC z is linear in screen space.** That is what makes the barycentric depth
  interpolation exact rather than merely close, and it is why the depth buffer
  stores ndc z and not view-space distance.
- **Winding is derived, not tabulated.** For a face on axis `a`, the other two
  axes in cyclic order satisfy `e_b × e_c = e_a`, so `base, +b, +b+c, +c` is
  counter-clockwise about `+a`; a negative face swaps them. Six hand-written
  corner lists would be six chances to get one backwards, and a backwards one is
  invisible until back-face culling eats half the model.
- **Gizmos are lifted, not depth-biased.** A highlight is nudged 0.01 voxel
  along the face normal. A depth bias has to be tuned against the near/far ratio
  and a value that works at arm's length fails when you zoom in.

### Everything is centred on the origin

The volume spans ±size/2 on every axis, and the **work plane** — the grid you
see, `Editor::ground_y` — runs through the middle of it rather than along the
bottom. A model is an object, not a scene: there is no reason for its ground to
be at the floor of the box, and a centred plane means the model grows either way
and the `Y` mirror reflects about a plane you can actually see. A new model's
seed voxel sits *on* that plane at the centre, so the first thing you see and
the surface you build on are the same thing.

The grid draws a line every `GRID_STEP` (4) voxels, picked out every
`GRID_COARSE_STEP` (16), plus the volume's own edge whether or not the step
divides the size. A line per cell is 65 lines each way at 64³, which at any
framing that fits the model is a grey haze rather than a grid.

### You can always build

Building places a voxel *against a face*, which on an empty grid means there is
no face and therefore nothing to do — the editor opened on empty space and no
click did anything. Two things fix that, and both are needed:

- A new model is **seeded** with one voxel at the centre (`new_model`), so the
  first click has something obvious to aim at.
- When the ray misses the model entirely, building falls back to the **work
  plane** (`Editor::ground_target`). Without it, erasing your last voxel would
  put the model back into the unrecoverable state.

The ground target is reported as a *face of a cell adjacent to the plane*, so it
is shaped exactly like a hit on a real voxel: the highlight, the placement and
the drag plane all fall out of the existing code with no special case. The plane
has two sides and both are used — from above the new voxel lands on it, from
below it hangs under it, the same rule as placing against a face you can see.
`face_corners` takes `i32` for the same family of reasons: a cell adjacent to
the plane can sit outside the volume, and an unsigned parameter would wrap
`y = -1` to the far end of the world.

Only building falls back. Erase, paint and pick act on a voxel, and bare plane
is not one; pick additionally refuses index 0, or it would leave the build tool
painting air.

### Picking is a grid walk, not a hit test

`voxeler` (the Python one this succeeds) hit-tested last frame's projected
polygons. This casts a ray into the grid instead — Amanatides–Woo traversal, so
the first solid cell reached *is* the nearest hit. It is exact at any
resolution, costs nothing per pixel, and hands back the face that was hit, which
is where a new voxel goes.

### A drag is one undo step, pinned to one plane

`Stroke` collects edits across events; `History::push` commits the lot as one
batch on mouse-up. That is why `Stroke` is separate from `History` at all — a
closure-scoped transaction cannot span three event callbacks.

`Drag::plane` is what stops a build drag from climbing its own work: each voxel
placed becomes a new surface for the ray to hit, so without the constraint,
dragging across a floor grows a staircase toward the camera. Build pins the
stroke to the axis and coordinate of its first placement; erase and paint do
not, because they do not grow the surface they are aimed at.

### What a tool does and how far it reaches are separate

`Tool` says add, remove or recolour; `Span` (`voxel-core/src/region.rs`) says
one cell, a run, a face, or a connected part. They are independent, so the four
tools and four spans are twelve useful operations from one flood fill rather
than twelve tools each carrying their own copy of it — "fill this face" and
"recolour this face" are the *same* set of cells reached with different intent.
Erase gets the three reaches for free, which is the sign the factoring is the
right one.

A region is **bounded by colour**: plane and volume grow over cells holding the
same palette index as the one under the cursor. Build starts on air and floods
air; erase and paint start on a voxel and stop where the colour changes. On a
single-colour model that is identical to "every connected voxel", so the rule
costs nothing there and is what makes a multi-colour model editable.

Two asymmetries in `region.rs` are deliberate and easy to "fix" wrongly:

- **A plane region tests different neighbours for build than for erase.** Build
  grows over air and needs the cell *behind* each one to be solid, or a fill
  spreads across the whole empty layer instead of across the face clicked.
  Erase and paint grow over the material and need the cell *in front* to be
  clear, or the region reaches buried voxels that share the layer. The material
  is on opposite sides of the region in the two cases, so one test cannot serve
  both.
- **The slice is applied twice, two different ways.** A hidden layer is excluded
  from a region (`joins` asks `model.get`), *and* reads as air to the neighbour
  tests (`on_face` asks `visible`). Only the first, and a build floods into
  layers that are not on screen; only the second, and the top of a
  cross-section is not a face a region can grow along, so clicking it fills one
  cell.

`region::cells` returns *candidates*, not writes. Whether a cell is actually
touched is the tool's rule — build fills air and never repaints, erase and
paint act on material and never create it — applied in `Editor::continue_stroke`.
With a single cell that rule holds by construction; a brush covers cells the ray
never touched, and a reflection lands wherever the model happens to be, so it
has to be stated.

### Mirroring reflects the edit, not the model

`Editor::mirror` is three independent planes, each through the middle of its
axis; two give four copies of a stroke and three give eight. What is reflected
is the set of cells the edit *wrote*, so nothing is touched that the stroke did
not reach. Turning mirroring on does not symmetrise what is already there, and a
deliberately lopsided model stays lopsided while you work on it symmetrically —
which is the whole point, and the reason this is not implemented as a
"symmetrise" command over the grid.

For the same reason a mirrored fill reflects the region it found rather than
re-running the flood from the reflected seed: re-seeding would let an asymmetric
neighbourhood on the far side produce an edit of a completely different size
from the one that was asked for.

## Key patterns

- **Out of bounds reads as air.** `VoxelModel::get` returns 0 outside the grid,
  and takes `i32` rather than `u16`, because every caller arrives from
  arithmetic that can go negative. Face extraction leans on this: the outside of
  the model is air, so boundary faces come out of the same rule as interior
  ones, with no special case.
- **A no-op write is not an undo step.** `Stroke::set` drops a write that
  changes nothing, so a drag re-painting one voxel forty times produces one
  entry and a click that changed nothing does not consume an undo. That is also
  what makes a cell lying *on* a mirror plane cost an iteration rather than a
  duplicate entry, at any number of active planes.
- **A fill is a click; a brush is a drag.** A region span applies once and
  `continue_stroke` returns early afterwards. Re-flooding as the pointer moves
  would re-seed several times a frame and turn one intended fill into a
  wandering pile of them.
- **A brush is sized by radius, so it is always odd-edged.** An even edge has to
  round its centre to one side, and the side it rounded to shows up as a
  half-voxel drift every time the brush is resized mid-model. `Brush::covers`
  measures a ball to `(r + ½)²` rather than `r²`: at radius 1 the latter gives a
  plus sign, which is not what anyone drawing with a ball brush expects.
- **A region is not previewed.** Outlining a flood fill means running it on
  every pointer move and drawing a box per cell for the answer. The span's name
  in the tool row is the signal beforehand; the cell count `end_stroke` puts in
  the status line is the confirmation after. A brush *is* previewed, because its
  extent is a box already known.
- **A slice hides layers from the mesh *and* the pick.** Treating hidden layers
  as air in the extractor is what puts a lid on the cross-section; re-casting
  against a sliced copy is what stops a click reaching a voxel that is not on
  screen. Both, or the slice is a lie in one direction.
- **The layer list is drawn top of the stack first.** A layer that covers
  another is above it on screen and later in the array, and only one of those
  counts downwards. `hud::layer_hit` does that flip and is tested row by row,
  for the same reason `palette_hit` is.
- **The HUD's layout and its hit test live together.** `hud::palette_hit` is the
  exact inverse of the swatch layout, tested swatch by swatch, for the reason
  `kessel`'s `window_to_console` is: a click landing one swatch off reads as a
  broken editor rather than as an off-by-one. `over_panel` is bounded in *both*
  axes for the same reason — testing the column alone made the whole right-hand
  strip of the window swallow clicks silently.
- **The default volume is 32³, and `--size` only describes a model that does not
  exist yet.** A loaded file keeps whatever size it was saved at. 64³ is
  supported and one flag away, but at a framing that fits it a voxel is a few
  pixels across, which is not a workspace.
- **Opening frames the volume; `F` frames the contents.** A new model is one
  voxel, and framing that fills the window with a single cube and shows nothing
  of the space around it. Seeing your workspace is the right thing on open;
  zooming to your work is a thing you ask for.
- **A missing file is not an error.** `voxeler robot.vxm` in an empty directory
  starts a model. Refusing would mean the tool could only open what some other
  tool had already made.
- **The file stores each layer, not the composite.** `.vxm` is `VXM3`: a scene
  range, a layer count, the active layer, then per layer its flags, name, origin,
  size and own sparse voxel list. Coordinates are relative to the layer's origin,
  so one byte covers a layer anywhere in the scene. `VXM2` (layers, no boxes) and
  `VXM1` (no layers) still load, and are **trimmed** on the way in — an old file
  gains the smaller shape by being opened. A file already on disk is not free to
  rewrite itself.
- **An export is a flatten.** `.vox` has nowhere to put a stack, so `ctrl+E`
  writes the composite as one model and the status line says how many layers
  went into it. Doing otherwise means the nTRN/nGRP/nSHP scene graph a
  multi-model `.vox` requires, which is the part of that spec most likely to be
  got subtly wrong.
- **Merging down shows the result.** The upper layer wins each shared cell — the
  same rule the composite follows, so a merge looks like what was already on
  screen — and the layer merged *into* is forced visible, or a merge into a
  hidden layer is a deletion in disguise.
- **`ctrl+N` clears every layer.** Leaving the hidden ones full would make the
  next save carry work the user believes they threw away.
- **A rename is modal.** While `Editor::rename` is `Some`, `app.rs` routes the
  whole keyboard into it — typing "BODY" would otherwise fire build, erase and
  pick on the way through. The characters come from `event.text`, not from key
  codes: a name is what the user's layout produces, and reconstructing that
  would be a keyboard-layout table this editor has no business owning.
- **Saves go through a temp file and a rename.** A crash or a full disk midway
  through leaves the previous save intact rather than a truncated file where the
  model used to be.
- **An export is not a save.** `ctrl+E` writes a `.vox` beside the working file
  and leaves the document dirty, because it is.

## Build & Run

```bash
cd crates && cargo build --release
cd crates && cargo test
cd crates && cargo clippy --all-targets

./crates/target/release/voxeler models/robot.vxm
./crates/target/release/voxeler models/robot.vxm --thumbnail shot.png
```

```bash
./crates/target/release/voxeler models/robot.vxm --mcp   # sse on 127.0.0.1:8730
./crates/target/release/voxeler mcp models/             # stdio, headless
./crates/target/release/voxeler attach models/          # a window onto that
```

`--thumbnail` renders one framed view and exits without opening a window. It is
how a model gets an icon, and how the renderer gets checked on a machine with no
display — which is also the fastest way to see whether a rendering change did
what you meant.

## Project Structure

```text
rs-voxeler/
├── crates/voxel-core/     the model, host- and render-free
│   ├── model.rs           the scene range, and the boxed layers in it
│   ├── palette.rs         256 colours; 0 is air
│   ├── edit.rs            Stroke + History
│   ├── raycast.rs         Amanatides–Woo grid traversal
│   ├── region.rs          how far one edit reaches: brush, run, flood
│   └── format/            .vxm (ours) and .vox (MagicaVoxel)
├── crates/voxel-render/   the software rasterizer, editor-free
│   ├── math.rs            Vec3/Vec4/Mat4
│   ├── camera.rs          orbit camera + pick ray
│   ├── mesh.rs            visible-face extraction
│   ├── raster.rs          clip, project, fill, depth-test
│   ├── overlay.rs         2D primitives + an embedded 5×7 font
│   └── png.rs             a minimal PNG writer (stored deflate)
├── crates/voxeler/        the editor
│   ├── editor.rs          state and every operation on it — the tested part
│   ├── mcp/               MCP: dispatch, wire, http+sse, stdio, tools, session
│   ├── attach/            `voxeler attach`: protocol, listener, viewer
│   ├── view.rs            one frame: backdrop, grid, model, gizmos
│   ├── hud.rs             palette strip, status line, help card
│   └── app.rs             winit events in, a blitted framebuffer out
├── models/                sample models
└── docs/
```

## Testing notes

- Anything that has an inverse is tested *against* its inverse rather than
  against a hard-coded expectation: `camera::ray` is checked by reprojecting a
  ray back to the pixel it came from, and `hud::palette_hit` by walking every
  swatch the layout draws. Both pairs are written independently, and nothing
  else would catch them drifting apart.
- The `.vox` orientation tests assert on the **file bytes**, not on a round
  trip. Export-then-import is the identity whether the axis change is a rotation
  or a reflection, so a round-trip test cannot see a mirrored model at all.
- The near-plane clip test asserts the *background* survives above the horizon.
  It has been checked to fail when the clipper is disabled — a rendering test
  that passes either way is worse than none.
- Non-cubic models appear throughout the fixtures on purpose. With
  `sx == sy == sz` the index strides coincide and a wrong one still round-trips.

## Troubleshooting

- **The model is invisible but the grid is there.** Almost always winding or
  culling: check `every_face_winds_outward`. A model drawn inside-out looks like
  an empty scene from outside and like a solid from within.
- **Wireframes flicker along the edges they trace.** Something is drawn coplanar
  without a lift. Nudge along the normal; do not reach for a depth bias.
- **A `.vox` from MagicaVoxel opens mirrored.** The axis change lost its
  negation. Fix it in `format::vox` and nowhere else — that swap is meant to
  live at the format boundary only.
- **A fill changes one cell, or nothing.** The seed did not match what the
  region is made of. Check `Reach::matches` — build passes 0 and everything else
  passes the colour under the cursor — and then `on_face`, which is where a
  plane span drops cells that do not show the face that was clicked. On the work
  plane, `Reach::grounded` is what stops the answer being exactly one cell.
- **A fill reaches voxels that are not on screen.** The slice is not being
  passed down, or `joins` has been changed to ask `visible` instead of
  `model.get` — a hidden layer must be unreachable *and* read as air, and the
  two are different tests on purpose.
- **A click on a voxel does nothing, and the status line names a layer.** That
  is the rule, not a bug: tools write to the active layer. Select the layer the
  message names, or `V` to hide it and reach what is under it.
- **A layer is drawn that should be hidden, or vice versa.** `get` and
  `owner_at` both filter on `visible`; a path that reads `Layer::at` directly
  skips that filter.
- **A voxel written to a layer vanishes.** It landed outside the scene's range —
  `set_in` refuses those — or the box was set with `set_layer_bounds` to
  something that cut it off. `set_layer_bounds` refuses a box that would drop
  voxels; `trim_layer` and a scene `resize` are the only paths that may.
- **A saved model comes back missing a hidden layer.** `format::native::encode`
  is using `iter_filled` (composited) where it must use `iter_filled_in`.
- **A scene is slow, or allocates far more than its contents.** Something is
  sweeping `model.size()` instead of walking the layers, or a layer was never
  trimmed after a large erase — `allocated_cells()` is the number to look at.
- **`voxeler attach` says nothing is running when a server is.** The session
  file is keyed by *canonical* root, and discovery prefers one rooted at the
  current directory. Name the root explicitly, or check `VOXELER_SESSION_DIR`
  and the cache directory agree between the two processes.
- **The attached window opens and never updates.** The client polls; the wake
  proxy is what makes the event loop look. Without it `ControlFlow::Wait` sits
  there until the mouse moves.
- **An MCP tool call hangs, then times out.** The event loop is not draining.
  Either the window is gone, or the `EventLoopProxy` wake did not fire —
  `ControlFlow::Wait` means nothing runs until something wakes it.
- **An MCP client connects and then nothing works.** Check the `endpoint` event
  is absolute and that `POST /messages` is served as well as `/message`; both
  spellings are in the wild, and serving one silently breaks the other's clients.
- **A click does nothing.** Check `over_panel` first — a HUD rectangle that
  claims more than it draws eats clicks with no feedback at all. After that,
  check whether `target_at` is returning `None`: for everything but build, no
  voxel under the cursor means no target, by design.
- **The editor feels slow on a large display.** The renderer caps at
  `MAX_PIXELS` (1.4 M) and upscales; if that is being hit, the cost is the
  rasterizer, and greedy meshing is the lever, not the cap.
