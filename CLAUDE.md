# rs-voxeler — Developer Guide

## Overview

A voxel model editor and the software renderer under it. It is a tool for
making models — by hand at the window, or by an agent over MCP — and the
renderer exists to show you what you are making.

It is **not** a game engine, and is not on its way to becoming one. Floats live
freely here; there is no VM, no entity system and no plan for one. What the
project invests in instead is the modelling surface: layers, objects,
instances, selections, and an agent-facing API precise enough that "make the
left arm longer" is an operation rather than a coordinate hunt.

Some design notes below compare a decision with `rs-kessel`, which shares this
project's MCP and `attach` patterns. Those are comparisons of *mechanism*, and
nothing more should be read into them.

## Architecture

Read-only MCP inspection lives in `mcp/tools/checking.rs` and `preview.rs`.
Scopes composite only their chosen layers, including object descendants; walk
layer storage, never the scene range. Bounded symmetry/component reports are
observations, not automatic repair instructions. Focused previews render a
temporary editor, never toggle the working model's visibility or slice.
Saved-file comparisons canonicalize copies and retain undo. Document replacement
changes `document_id` and clears selection/clipboard; ordinary edits do not.
`session_id` distinguishes process restarts. `put_prism` includes polygon
boundaries and uses the same prevalidated one-undo batch path as other shapes.

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
- **Growing is a copy, never a recount.** A box that only grew holds the same
  cells in the same order, so a row of x copies whole with `copy_from_slice`,
  `filled` stands, and each axis' plane counts are the old ones shifted by
  however far that origin moved. The general path — the one a trim or a scene
  resize needs, where cells can be dropped — walks every cell of the box doing a
  division and two remainders. Filling a 256³ scene grows the box about 768
  times, so that walk ran over 2.1 billion cells: five seconds, and the whole of
  why a bulk fill was slow. It is 319 ms now. Measured, and the reason the
  policy did **not** change — growing geometrically would have bought the last
  2.4× by making every box up to 1.5× too big on each axis, which is the
  guarantee this design exists to make.
- **It shrinks only when asked** (`trim_layer`), *except* when the layer becomes
  empty, which gives the box back at once. The high-water mark exists so that
  erasing and redrawing in one spot does not reallocate every stroke, and an
  empty layer has nothing to churn — where holding its old extent is pure waste.
  Measured: a new model seeds one voxel at the scene's centre, so erasing the
  seed and then building near the floor used to keep a box spanning both, 26×
  the size of the work and 487 MB against 35 MB at 256³.

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

### An object is what a thing is; a layer is how pixels combine

Layers were doing two jobs. One of them — a grid, a box, a stack order, a
visibility — is compositing, and it works. The other was "this is the left arm",
and for that a flat list of sixteen with no way to say one thing is *part of*
another is the wrong shape.

`Object` is a name, a visibility and a parent. Layers name the object they
belong to, every scene has a **root at index 0** that cannot be removed or
reparented — so "no object" and "the whole scene" are one answer rather than two
that can disagree — and the tree is a flat arena with parent indices rather than
nested children, because every structural change here is already
snapshot-based: a flat list clones, compares and serialises with no recursive
walk, and the shape is one `parent` field rather than a second structure to keep
in agreement.

Four rules:

- **There is no transform on an object.** `move_object` applies a translation to
  its layers' `Bounds::origin` at the moment it is asked, rather than storing one
  to compose on every read. Layer boxes stay in scene coordinates, so `get`, the
  raycaster and the extractor need to know nothing about objects at all — and the
  move costs three `u16` per layer instead of a re-voxelisation. A stored
  transform would put a matrix inside the hottest read in the codebase to buy
  something nobody asked for.
- **A move is all or nothing.** The whole subtree is checked against the scene
  before any of it moves; half a robot moved and half left behind is worse than a
  move that did not happen, and the refusal names the layer and the axis because
  an agent cannot see the scene edge.
- **A rotation cannot be free, so it bakes.** `rotate_object` rewrites the grids
  rather than sliding boxes — there is no stored transform for it to live in,
  which is the rule above — and it reuses `rotate_selection`'s conventions rather
  than inventing a second set: quarter turns, right-hand rule about the positive
  axis, pivot about the **low corner** so a turn and its inverse are exact. The
  pivot is the union of the whole subtree's occupied cells, computed **once**: a
  pivot per layer would turn every part about its own middle and the robot would
  come apart. All or nothing, like a move. An **instance cannot be turned** — its
  placement holds an offset and a mirror and nowhere to keep a rotation, so the
  next rebuild would undo one; the refusal points at the source, whose rotation
  turns every copy.
- **Removing an object removes a label, never the work.** Its children and its
  layers move up to its parent. It also changes what is on screen — a layer that
  was inside a hidden object is not any more — so it recounts, which the drift
  test caught and nothing else would have.
- **A cycle is refused in both directions.** `reparent_object` refuses a parent
  that is `i` or under it; `set_objects` refuses a file whose chain loops. A file
  is not a caller, and a cycle would make the visibility walk and every tree draw
  run forever.

`Layer::shown` is `visible` **and every object above it visible**, cached and
recomputed by `refresh_shown` — the single place it moves, for the reason
`Layer::note` is the single place a tally moves. Compositing asks `shown`, never
`visible`: walking to the root per lookup would put the depth of the tree inside
`get`. The layer's own flag never changes when an object hides it, so showing
the object again restores exactly what was shown before.

The layer panel draws four states for that reason — filled for on screen,
filled with a notch for an instance's copy, hollow with a pip for "switched on,
hidden by an object", hollow for switched off here. Filled would be a lie about
the screen; plain hollow would make `V` look broken.

### The panel is the tree, and the hit test is not an inverse

The panel used to list layers flat, so the object tree existed in the file and
over MCP and was invisible at the window — the pip was the only hint that
objects were a thing at all. It now draws objects with their layers under them,
indented, with a switch and a fold on each object row.

`hud::panel_rows` is what makes that safe. The flat panel's `layer_hit` was a
hand-written *inverse* of the drawing code: correct, tested row by row, and a
standing invitation to drift. A tree makes that inverse harder — rows are no
longer "layer count minus one minus n", and the switch moves with the indent —
so there is no longer an inverse. The row list is built once and **both** the
drawing and the hit test index into it. They cannot disagree about what row
four is, because there is one answer to that question.

Three rules:

- **Objects in arena order; layers within an object still top of the stack
  first.** What this gives up is reading the *whole* stack's order off the
  panel: two layers in different objects appear in tree order, not stack order.
  That is the trade for showing the tree at all, and it is the right way round —
  the tree is what the file, the MCP surface and every "make the left arm
  longer" is expressed in.
- **The switch and the fold are separate targets.** On an object row the switch
  hides the part and anywhere else folds it. "Stop showing the arm" and "stop
  listing the arm's layers" are different intentions, and a panel that guessed
  between them would be wrong half the time.
- **A fold is not data.** `Editor::collapsed` is view state: not saved, not
  undoable, dropped with the selection and the clipboard when the document is
  replaced. It is held by object index, which is the one wart — removing an
  object renumbers the ones above it, so a fold can end up on a neighbour. One
  click puts it right, and the alternative is an identity on `Object` that the
  file format would have to carry for the sake of a triangle in a panel.

### An instance is a reference, and it is baked

Four wheels are one wheel and three references to it. `Object::instance` is
`Some(Instance { source, offset, mirror })` for an object that repeats another
one, and the whole feature turns on where that reference is *resolved*.

It is not resolved in `get`. The rule above — that `get`, the raycaster and the
extractor know nothing about objects — is the one thing here worth protecting,
and putting a stored transform inside the hottest read to save a byte per cell
would trade it away. So an instance owns **one ordinary layer**, marked
`Layer::generated`, holding the source subtree's composite, and
`rebuild_instances` rewrites it whenever the source moves. Compositing, meshing,
the raycaster, `owner_at`, `occupied_bounds` and every maintained tally needed no
changes at all, because a derived layer *is* a layer.

**One layer, not one per source layer.** A rebuild must never change the layer
*count*: removing or inserting a layer renumbers the ones around it, and every
`Edit::layer` already on the undo stack would start pointing at the wrong grid.
Flattening the source to a single grid is what makes a rebuild a pure cell
operation, and it is the same answer `.vox` export already gives.

Five rules:

- **Derived cells are never in the history.** Undo puts the *source* back and
  the rebuild runs again, which is why undoing through a source edit leaves the
  copies in step instead of restoring four stale ones. `Editor::refresh_instances`
  is the one place it is asked for, at the commit points — a stroke, a batch, an
  undo, a structural change — never inside `set_in`, or a fill would pay for a
  source walk a million times.
- **An instance repeats content, not appearance.** Hidden layers of the source
  are repeated like any other. Visibility is a property of the view, and a copy
  that emptied itself because somebody switched a layer off while working would
  be a reference to the screen rather than to the part.
- **The placement is a delta, not a destination.** "The corner lands at
  [24,0,0]" comes apart the moment the source grows a voxel on its left. The
  mirror reflects about the middle of the *source's own* box, so a mirrored arm
  lands beside the body instead of across the world, and `lo + hi - v` needs no
  centre cell — exact at either parity, the same reason `flip_selection` has no
  rounding problem.
- **Placing is all or nothing; rebuilding clips.** A placement is something a
  caller chose and can be told to choose differently, so one cell outside the
  scene refuses the whole thing. A rebuild is a *consequence* of an unrelated
  edit, and refusing there would leave every instance stale with nothing to
  press — so it clips, and `instance_clipped` is how the listing says so. Found
  by driving the real binary: a stray voxel far out on a source grew its box and
  carried the copy over the edge in silence.
- **A write to a copy is refused, and names the source.** Not detached-on-write:
  that quietly stops tracking the source, and the user finds out much later when
  an edit no longer propagates. `select_layer` is the single gate — a derived
  layer never becomes active, so build, erase, paint, fill, paste and every
  selection made from the active layer are off it without a check of their own.
  `detach_instance` is the way out, and changes what the layer *means* rather
  than what it holds.

Removing an object detaches both directions — the instances of it, and it if it
was one — because removing a label must not remove the work, and an instance's
work is the copy it is holding.

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

### Selecting by hand: a fifth tool, and two grains

The selection and its transforms shipped as MCP tools with no keys behind them,
so a person could build, erase, paint, fill, mirror and slice by hand and could
not select anything — while looking at the outline of a selection an agent had
made. `Tool::Select` closes that.

A **tool** rather than a modifier, because a drag already means "apply the
current tool", and span and brush then compose with selecting exactly as they do
with the other four — which is the whole reason `Tool` and `Span` are separate.
It is the one tool that never writes, so `app.rs` handles its click directly
rather than through a stroke: a choice, like a click on a panel, costing no
undo. `begin_stroke` returns early on it as a floor under that.

**Two grains, one question.** `O` switches between cells and objects, because
"that thing there" is asked at two sizes:

- **Cells** go through `select_with_span`, the same `region` walk the drawing
  tools use, matched on *material* rather than colour — an arm is one part
  whether or not the glove is a different index.
- **Objects** set `Editor::selected_object`, which is an object index and
  deliberately **not** a `Selection`. A selection is cells on one layer; an
  object spans as many layers as it likes. Gathering a part's voxels into a
  selection would silently take only the active layer's share and tear the part
  in half on the first move. So the transforms it feeds are `move_object` and
  `rotate_object`, which already carry a whole subtree all-or-nothing.

Both outline in the viewport, in two colours, because both can be set at once
and they mean different things.

The arrow keys move whatever is selected, deciding by *what is actually
selected* rather than by which mode is showing — pressing an arrow with a part
outlined and having some cells move instead would be the surprising answer.
`shift+X/Y/Z` turns, `alt+X/Y/Z` flips. Flip is cells only: an object has no
stored transform to hold a reflection, and `create_instance`'s `mirror` is where
a mirrored part lives.

There is no duplicate key. Paste puts the clipboard back at the corner it was
copied from — `Clipboard::origin` exists for that — so copy, paste, arrows *is*
`duplicate_selection`, with the offset chosen by eye instead of typed as an
argument.

**A click on nothing clears both selections.** Pointing at empty space and
pressing is how everything else with a selection says "never mind", and the
alternative is an outline on screen with no obvious way to be rid of it. Both go
rather than only the one the mode is showing, because "nothing selected" is one
idea and leaving the other outline up would make the click look like it had
missed. The select tool gets no work-plane fallback for the same reason: that
exists so build has something to aim at on an empty layer, and here it would
make a click on the sky select a cell of air instead of clearing.

The tool's click sits *after* the alt and shift branches in `on_mouse_down`, not
before. An early return there swallowed alt-orbit and shift-pan — looking at the
thing you are about to select is part of selecting it, and a tool you cannot aim
is not a tool.

The select mode gets a chip, and only while the tool is running: "cells" and
"objects" look identical until you press an arrow, and by then the wrong thing
has moved. A setting with no chip is a setting nobody finds.

### Sculpting is two tools and one locked frame

Most of what #29 asked for arrived with bounded regions: a disc on the surface
is `Span::Plane` with a radius, and raising or lowering one is `Build` or
`Erase` over it. What was left is the pair of operations that cannot be
expressed as "a colour applied to a reach", because they decide **per cell**
what to write.

`sculpt_value` is where that decision lives, and it is a free function rather
than a method for one reason: the hand and the agent both reach it. A smooth
that rounded a corner at the window and not over MCP would be two tools wearing
one name.

- **A flatten fills a dent, not the room under a table.** Every cell below a
  slab is "behind the plane", so filling all of them was the obvious reading and
  the wrong one — flattening a table top packed the space beneath it, 147 cells
  under a 16² slab. An air cell fills only when the cell one step *further* from
  the plane holds material, already or because this same pass filled it. Cells
  are therefore walked deepest-first, so a dent two deep still fills from its
  floor upward in one pass. This is the one place the read-everything-first rule
  bends, and it bends on purpose: a column has to see itself being built.
- **`Flatten` needs a frame, not a reach.** `Drag::reference` locks a plane at
  mouse-down — the cell that was hit, and the normal of the face that was hit —
  and never re-estimates it. Re-deriving it per frame would make the direction
  flap as the brush crossed a corner, `+X → +Y → +X`, and the stroke would fight
  the hand. It is the same rule `Drag::plane` and `Drag::before` follow: a
  stroke commits to what it started on. The brush centre rides that plane rather
  than the ray, or a flatten would sink as it carved — each pass exposing a
  deeper cell for the next to centre on.
- **`Smooth` is a majority vote over the six face neighbours.** A solid cell
  with two or fewer solid neighbours is a spur and goes; an air cell with four
  or more is a notch and fills. Face neighbours only: counting the twenty-six
  would let a diagonal contact hold a spur on, which is the thing a smooth is
  being asked to remove. Rounding a corner is not a bug — a slab's corner has
  exactly two solid neighbours, and taking it off is what a smooth is *for*.
- **Every decision is read before any is applied.** Written as it goes, one
  smoothing pass cascades into itself and eats a surface in a single
  application: the cell behind each rounded corner becomes a corner. So one call
  is one pass, and going further means calling again. The same rule
  `transform_selection` follows, for the same reason.
- **Smooth rounds a sheet's corners, not a block's.** Two-or-fewer is a rule
  about *thin* material: a one-cell sheet's corner has two solid neighbours and
  goes, where a solid block's corner has three and stays. That is consistent and
  predictable, and the description says so rather than promising corner rounding
  in general — `flatten` is what cuts a block back. The threshold was left where
  it is deliberately; raising it to three would take the top row off every thin
  wall.
- **A fill takes the colour around it, not the palette selection.** A sculpt
  repairs a surface that is already there, so `fill_color` uses the commonest
  colour among the cell's solid face neighbours and falls back to the tool's
  own. Filling a notch in a red panel with whatever the brush happened to be set
  to is not what "close this up" means.
- **Mirroring reflects the frame with the cells.** `write_cells` reflected the
  cells and left the reference alone, so every reflected cell was judged against
  the *original* plane, came out in front of it, and was carved away — a
  mirrored flatten of one wall took 52 cells instead of 2 and ate half the wall
  opposite. The stroke now walks one group per mirror combination, each carrying
  its own reflected plane. A position reflects as `size - 1 - p` and a normal by
  negating that component; they are not the same operation, which is what made
  the bug easy to write.
- **A sculpt tool takes the brush ball whatever the span row says.** It has to
  see the material behind a cell as well as the air in front, which a surface
  flood does not give it. No span is lit while one is running: a highlighted
  span that is not being consulted is worse than none.

`sculpt_surface` is the agent's door onto the same rule — `flatten`, `smooth`,
`raise`, `lower` at a point with a radius. `normal` matters only to flatten, and
defaults to the first face of the cell with air against it, +Y first; a buried
cell has none to offer and the refusal says so rather than guessing.

### The gizmo is one list, drawn and hit-tested

Arrow keys move a selection one cell a press. `gizmo.rs` is the other end of the
same operation: three axis arrows on the selection's box, dragged.

`gizmo::handles` is built once and **both** the drawing and the hit test index
into it — the rule `hud::panel_rows` follows, for the reason written there. The
panel's hit test used to be a hand-written inverse of its layout, and an arrow
in perspective is far harder to invert than a row twenty pixels tall.

Four rules:

- **A drag writes nothing.** `Editor::preview_offset` moves the *outline*, and
  mouse-up performs the move once. That is not a nicety: `move_selection` is an
  undo step per call and `move_object` takes a whole **layer snapshot** per
  call, so committing per cell of travel would put dozens of snapshots on the
  stack for one drag. It is also the rule that has a brush previewed and a
  region not — what is cheap to show is shown, what is expensive waits until you
  ask.
- **It follows what is *actually* selected**, the same question the arrow keys
  ask: the part if a part is selected, the cells otherwise. Both go through
  `move_whatever_is_selected`, so the key and the handle cannot come to differ
  about what they move.
- **It is claimed before the tool.** In `on_mouse_down` a handle is taken after
  the panel and before anything that edits or picks — otherwise grabbing one
  places a voxel, and the select tool's "a click on nothing clears both
  selections" fires while you are reaching for an arrow.
- **The scale comes from the handle's own screen length.** An arrow spans a
  known number of voxels, so a drag that far along it is that many cells,
  correct at any angle and any zoom with no second projection to keep in
  agreement. An arrow pointing nearly at the camera is nearly a point on
  screen — that one declines rather than returning a wild number.

Drawn over the scene, not lifted. A lift of 0.01 along the normal is enough for
a highlight lying *on* a face; an arrow sticks out through the model and no lift
saves it, so the depth is biased past anything the scene can hold.

Rotation handles are the next stage and are deliberately not here. Rotation is
quarter turns — pivoting about the low corner is what makes a turn and its
inverse exact — so a continuous ring would promise something the model cannot
do.

### A selection is cells, on one layer, and not part of the document

`Editor::selection` is what makes an existing shape something you can pick up
rather than only draw. Four decisions hold it together, and each has a failure
mode on the other side:

- **Cells, not a box.** A box would carry the air inside it, and moving "this
  arm" would drag a cube of nothing along and erase whatever it landed on.
- **One layer, named at selection time.** Tools write to the active layer; a
  selection that followed the *current* active layer would move a different set
  of voxels than the one the user was shown.
- **Never empty.** `Editor::selection` holds `None` rather than an empty
  `Selection`, so "is anything selected" is one question with one answer instead
  of two that can disagree.
- **Dropped on undo.** A selection names coordinates and an undo changes what is
  at them — including putting back voxels a move took away. Keeping it would
  leave it pointing at cells it was not made from.

`Editor::transform_selection` is the shape every transform has: read **every
colour before anything moves**, then emit the clears ahead of the writes in one
`apply_writes`. A transform whose result overlaps its source — a short move, a
rotation of a squat shape — would otherwise carry a voxel along instead of
leaving it where it landed, or erase its own arrival. Move, rotate and flip are
three cell mappings over that one body.

**A rotation pivots about the low corner, not the centre.** Centring reads
better and is not invertible: a quarter turn swaps two extents, and where those
differ in parity the centre falls between cells and has to be rounded. Rounding
the same way each time accumulates — `rotate(+1)` then `rotate(-1)` came back a
whole cell out, which is how this was found. Turning something to look at it and
turning it back has to be exact, and a square footprint pivots identically
either way. Flipping has no such problem: `lo + hi - v` needs no centre cell.

The sense of a positive turn is the right-hand rule about the positive axis, the
same convention face winding uses (`e_b × e_c = e_a`) — one meaning of "positive
rotation" in this codebase rather than two. It is pinned by a test that asserts
the exact cells an L-shape produces, because a description of a rotation's
direction is the easiest thing in this file to get backwards.

`region::Match` splits the question a region answers. A *fill* asks about a
colour and stops where the colour changes, which is what makes it a fill. A
*selection* asks about material: an arm is one part whether or not the glove on
the end is a different index.

`region::Reach::within` bounds the growth. Connectivity alone can only ever
answer "the whole figure", because a limb is attached to its body — this was
found by driving the real binary, where `select_connected` on an arm returned all
324 voxels of the figure. The box is checked **during** growth, not applied to
the result: a flood that spread through cells outside it and was trimmed at the
end would reach parts the box was meant to keep out.

### The clipboard is not the document, and neither is the selection

Both live on `Editor` and neither is saved, but they differ on undo, and the
difference is the point:

- **The selection is dropped.** It names coordinates, and an undo changes what
  is at them.
- **The clipboard survives.** Undo puts the *model* back; a clipboard that
  emptied itself when you undid the copy would be a surprise rather than a rule,
  and a paste you undid is exactly the thing you want to paste again.

Cells are stored relative to the copied selection's **low corner**, so
`paste` at that corner is what was copied rather than an arithmetic guess. The
clipboard carries no layer: a paste writes to the *active* layer, which is what
makes copying between layers a paste instead of a separate tool.

`duplicate_selection` is copy-then-paste-at-an-offset, and it exists because the
mirrored-pair case — the commonest reason to copy anything — should not require
naming the corner the original happens to sit at. The copy lands selected, so
`duplicate` then `flip` is the whole of it.

### Subdividing, and what a snapshot has to hold

`VoxelModel::subdivide` scales the scene so every voxel becomes `factor`³ of
them: the way a coarse shape you are happy with becomes one with room for
detail. Each layer's box scales with its contents, so the cost is `factor`³ of
what was allocated rather than of the scene's range — a 16³ scene of 525 cells
becomes a 32³ scene of 4 200, not of 32 768.

It is a **plain replication, not a smoothing**. A subdivide that rounded corners
would be a different model rather than a finer one, and you could not carve
against it and get back what you drew.

It also forced a fix. `layer_snapshot` recorded the layers and the active index
but **not the scene's size**, which was fine while every structural change left
that alone. Undoing a subdivide would have restored layers at twice their
coordinates into a scene half the size — every box outside it, every voxel gone
from the composite. `Snapshot` now carries the size, and is the one type both
`model` and `edit` use for it.

The camera and the slice scale with it too. Neither is part of the document, but
both are measured in voxels: leaving the camera would make the model appear to
leap towards you, and leaving the slice would cut through a different part of
the shape than the one on screen.

### Colour is a way of naming a part

"All the red" is a part in a way that "all the cells in this box" is not, which
is what `select_by_color` and `count_by_color` are for — an agent could already
see the picture with `screenshot` and count cells with `describe_model`, and
could not ask what the model was *made of*.

Two rules the palette operations turn on:

- **Index 0 is air, not a colour, and every one of them refuses it.** `slot_arg`
  is a separate argument helper from the drawing tools' `color` for exactly this
  reason: a drawing tool takes 0 and erases with it, where "replace 0 with white"
  means every empty cell and would fill the model.
- **Recolouring moves voxels between slots; `set_palette_color` changes what a
  slot means.** `swap_colors` therefore swaps the *voxels*, not the palette
  entries. Both readings put the same picture on screen — a swap of two slots'
  colours looks identical to a swap of which slot each voxel names — but only
  this one leaves index 3 meaning the red it meant, so a brush set to 3 still
  paints red.

`compact_palette` is the one that touches both halves: it renumbers indices and
rewrites every voxel that used one. It goes through the snapshot path, which is
why `Snapshot` carries the **palette** — a compact recorded as cell edits alone
would undo the voxels and leave them pointing at colours that had moved. It also
follows the editor's selected colour through the mapping, and reports the
mapping, because every index a caller was holding is stale afterwards.

Adding the palette to `Snapshot` is also why `Change::Layers` boxes its
snapshots: the variant is the odd one out by two orders of magnitude, and inline
it made every one-cell edit on the undo stack carry 1680 bytes.

### Structural changes store the stack whole

`History` holds a `Change`, which is either cells or layers. A layer snapshot
carries the **object tree** as well as the stack and the scene size: removing an
object renumbers the ones above it exactly the way removing a layer does, so a
snapshot that put the stack back without the tree would leave every layer filed
under the wrong part of it.

Object *visibility* is outside the history for the same reason layer visibility
is — it is toggled constantly while working, and undo would spend its first few
presses turning things back on. It still dirties the document.

 A cell edit names
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

### The file tools are confined to a set of directories

`mcp::Roots` resolves `open`/`save` paths inside the directories `voxeler mcp`
was given, and refuses to leave them. An MCP server is driven by a model reading
content nobody vetted, so this is a boundary rather than a convenience.

**Absolute paths are accepted, and that is the point.** They were refused at
first, on the reasoning that a relative path under a known root is unambiguous.
It is not: a desktop MCP client spawns its servers with whatever working
directory the *app* had, so neither the user nor the agent can say where a bare
file name lands. Reported from use. Three things follow:

- The roots are named in the `initialize` **instructions**, so the agent is told
  where it may write before it tries rather than after a refused save.
- A relative path resolves against the *first* root; an absolute one may be in
  any of them. Returned file paths follow that same rule, so a saved path can
  be passed back without accidentally selecting a same-named file in another
  root. `list_models` also includes each file's absolute path.
- `voxeler mcp` with no argument refuses to root at the filesystem root, which
  is what a desktop client's working directory often is. Explicit is still
  allowed — `voxeler mcp /` means what it says.

Two rules on the check itself, and both are load-bearing:

- **`..` is refused lexically**, before anything touches the filesystem, and
  *every* `..` rather than only the escaping ones. `a/../b` is harmless and
  `../b` is not, and telling them apart after the fact is exactly the reasoning
  that goes wrong.
- **An absolute path is checked against its canonical form**, via
  `canonical_enough` — which canonicalises the deepest ancestor that exists and
  puts the rest back, because `save_model` names a file that does not exist yet.
  A prefix test on the name alone would be satisfied by a symlink inside a root
  pointing anywhere at all.
- **A symlink anywhere below a root is refused outright**, on top of both. It
  would survive the canonical test when it points inside, and it can be
  repointed outside between the check and the write. Nothing about a voxel model
  needs to be reached through a link, so the cheapest sound rule is to decline
  them — and `list_models` skips them when it recurses for the same reason.

Under `--mcp` the roots are empty and the file tools reach nothing: you opened
that document yourself, and an agent there has no business opening another.

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

### A rebuild is proportional to the edit, not to the model

Face extraction is what a change makes necessary, a full one at 256³ costs about
300 ms, and almost every change is one voxel. So the mesh is kept **per chunk**
(`CHUNK` = 16, a 4 KiB dense one — 8 multiplies the bookkeeping, 32 makes one
voxel's work touch thirty-two thousand cells), and only the chunks the model
says have changed are walked again.

`VoxelModel::dirty` is a bitset over the scene's chunk grid, not a set of
coordinates: a fill writes sixteen million cells, and sixteen million hash
inserts would cost more than the rebuild they were meant to save. At 256³ the
whole thing is 4 096 bits.

`set_in` marks the chunk a cell fell in, **and the neighbour across a shared
face when the cell sits against one** — a face belongs to the cell that emits it,
so a boundary cell is part of its neighbour's answer too. Fourteen of every
sixteen cells along an axis are interior and cost one mark. Anything not
attributable to cells — a slice moving, a layer hidden, a subdivide — calls
`dirty_all`, and the editor does the same for the slice because the cut is not a
property of any one chunk.

`Editor::mesh` is the consumer that catches up, so it is where `clear_dirty` is
called. The model does not know who has looked.

Two things fall out of walking a chunk's cells rather than a layer's:

- **The `owner_at` dedupe is gone.** A chunk walk asks `get`, which is already
  one value per cell, so an overlap resolves itself.
- **A chunk no layer reaches is free**, which is what keeps a sparse scene cheap
  — but the test has to ask whether the layer is *empty* as well as where its box
  is. A box is a high-water mark: a cleared layer keeps its size until someone
  trims it, and without that test a cleared 256³ scene swept every cell it used
  to have to find nothing. It did, briefly: 19 ms became 40 ms before the check
  went in.

### Tallies are maintained, not walked

`VoxelModel::filled_count`, `Layer::filled_count` and `Layer::occupied` are all
O(1)-ish because the answers are asked for far more often than they change:
every MCP tool call reports a count, the status line reads one on every redraw,
the layer panel reads a per-layer one per row per redraw, and **every screenshot
frames**, which asks for the occupied box.

Measured before fixing, at 256³: a single voxel edit cost 43 ms, and a
screenshot after one cost 348 ms. Both are now under 15 ms.

A **count** can be maintained by adding and subtracting one. A **box** cannot —
erasing the cell that was furthest out has to find the next furthest, and
nothing short of a walk knows where that is. So `Layer::planes` counts filled
cells per plane of each axis: a write touches three counters, and the tight box
is the first and last non-zero plane on each axis, a scan of a few hundred
numbers rather than of sixteen million cells. Exact, not an approximation, and
it shrinks the moment the last cell of a plane goes.

`VoxelModel::occupied_bounds` is then the union of the *visible* layers' boxes,
which is exactly right rather than merely close: a filled cell on any visible
layer makes that scene cell non-air whether or not another layer covers it.

Three rules keep them honest:

- **`Layer::note` is the only place a single cell moves a tally**, so `filled`
  and `planes` cannot drift apart by one being updated and the other forgotten.
- **Paths that replace the whole array tally as they build it**, never by a
  second walk over the result. `reshape` is called on every write that falls
  outside a growing box, so a second pass there cost three seconds on a full
  256³ fill — it was measured, not guessed.
- **`the_maintained_count_never_drifts_from_a_fresh_walk`** runs every operation
  that can change what is visible and checks every cheap answer against the
  expensive one, including erasing the furthest cell on each side so the box has
  to shrink from both ends.

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
known to be right — and it now has a second constraint: **it may only merge
quads whose four AO levels match.** Merging quads with different corner shading
would average away the very thing the shading adds, so greedy has to compare the
`ao` byte along with the palette index.

### Shading reads the shape, not just the face

Flat shading gives every face one colour, and a voxel model drawn that way reads
as a set of tinted rectangles rather than a solid. Vertex ambient occlusion is
the cheapest thing that fixes it: for each of a quad's four corners, look at the
three cells meeting it *in the air in front of the face* — the two along the
face's own axes and the one diagonally between — and count them. Three
neighbours give four levels, which is why `FaceQuad::ao` is two bits per corner
in one byte.

Three things it turns on:

- **The corner order is derived the same way twice.** `corner_shade` walks the
  cyclic `(b, c)` pair exactly as `face_corners` does, so corner *i* means the
  same corner in both. Tabulating it in one place and deriving it in the other
  is how they would come apart.
- **Both edge neighbours solid is fully dark, whatever the diagonal holds.** The
  diagonal is not reachable from outside a crease anyway, and without the
  special case an inside corner reads *lighter* than the flat wall beside it.
- **The rasterizer interpolates it with the barycentrics it already has.**
  `fill_triangle` was computing `w0*a.z + w1*b.z + w2*c.z` for depth; the shade
  is the same three multiplies at the same site. The flat path is kept separate
  and unchanged — the grid, the gizmos and the volume box are one colour by
  nature, and making them pay a per-pixel multiply to say "times one" would be a
  cost for nothing.

`Light::occlusion` is the whole of the tuning, and 0 gives back exactly the
picture this renderer drew before — which is what the test compares against,
because "it looks nicer" is not an assertion.

**It widened the dirty rule, and that is the part to be careful with.**
`set_in` used to mark the chunk a cell fell in and the neighbour across a
*shared face*. A corner's shade reads the cell diagonally across from it, so a
cell on a chunk's corner is visible to all eight chunks meeting there, and a
per-axis walk names only four of them. Miss the diagonals and the shading along
a seam stays as it was before the edit — invisible until somebody draws exactly
on a boundary. `mark` now takes the *product* of each axis' affected indices,
which is still exactly one chunk for the fourteen interior cells in every
sixteen.

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

### The work plane is where a layer starts, not a standing offer

Building places a voxel against a face, so a layer with nothing in it has
nothing to build against. The work plane rescues that — but only while the
**active layer is empty**. `Editor::plane_is_open` is the whole rule, and three
things fall out of it:

- An empty layer can always be started, which was the fallback's original
  purpose: erasing your last voxel must not make the layer unrecoverable.
- A *part* can be started. A new layer for a tree is empty, so its first voxel
  goes anywhere on the plane; after that the tree is what you build against.
  `A` is therefore the answer to "how do I put something over there".
- Empty space stops being clickable the moment there is something to aim at.
  Leaving the plane permanently open made the entire viewport a build surface,
  and a click meant for the camera placed a voxel instead. That was reported
  from use, and it is the reason the rule exists.

`Drag::on_plane` keeps the plane open for the rest of a stroke that began on it
— the stroke's own first placement fills the layer, which would otherwise close
the plane out from under the remaining moves.

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

### A stroke aims at the model it started on

The plane pin only ever covered half the problem, and the other half shipped:
**one click added two voxels.** Placing a voxel puts a new *side* face under the
pointer, and a side face's adjacent cell is on the same plane, so the pin waved
it through — and a press emits a move or two of its own, so the very next event
built again. Erasing has the mirror of it: the hole it opens lets the next event
reach the wall behind, and one click erased four.

`Drag::before` records what each touched cell held before the stroke touched it,
and `raycast::cast_masked` casts through that map, so a stroke's own work is
invisible to its own aim. A mask rather than a snapshot: the touched cells are a
handful, where cloning the scene on every mouse-down would be a copy per click.

Two rules, not one, because they fail differently:

- **The mask** stops a stroke re-targeting onto itself. It is what makes a click
  one voxel however many events the platform emits for it.
- **`app::is_click`'s dead zone** stops a click becoming a drag at all. It is
  needed *as well*, because near the horizon one pixel of the work plane really
  is a whole cell away — that second placement is geometrically correct and
  still not what anyone meant by clicking.

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

- **Procedural edits validate before writing.** `mcp/tools/modeling.rs` expands
  voxel, rect, ellipsoid, capsule and tapered-line operations under a candidate-cell budget.
  Only once every argument is valid does `Editor::apply_writes` commit the ordered
  writes through one `History::edit`. Layer/color defaults can be overridden per
  operation; selection is unchanged. Reports count attempts, including overlaps.
- **Tapered ends follow the branch, not world-up.** `put_tapered_line` and the
  batch `tapered_line` operation interpolate `radius_from` to `radius_to` along
  distinct endpoints. End discs are perpendicular to that axis; a zero radius
  makes a point. Equal radii make a flat-ended cylinder, leaving `put_line`'s
  capsule semantics unchanged. Both-zero radii and coincident endpoints are
  refused. This replaces the per-voxel arithmetic a pointed, oblique accessory
  needed in a real modeling session.
- **Palette history stores colours, not grids.** `Change::Palette` records one
  index's old/new RGB. An unchanged RGB adds no undo step. Palette changes dirty
  the document and are included in the existing native/VOX serializers.
- **Preview options are separate from editor overlays.** Interactive `render`
  keeps bounds and editor lighting. `render_with_options` serves thumbnails and
  MCP screenshots with bounds off and softer lighting by default. Screenshot
  arguments are validated before moving the camera; camera/grid are restored
  before file I/O, including failure paths. Presets view from +Z (front) or +X
  (right), and explicit angles override the preset.
- **Recursive discovery does not follow links.** `list_models` defaults to a
  recursive walk across all roots, accepts a directory relative to the first
  root or absolute inside any allowed root, and sorts the results.
  `Roots::resolve` rejects paths outside the allowed roots, symlink components
  and parent components. PNG exports use that same confinement and require a
  `.png` extension.

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
- **An unbounded region is a click; a bounded one is a stroke.** A region with
  no limit applies once and `continue_stroke` returns early afterwards:
  re-flooding as the pointer moves would re-seed several times a frame and turn
  one intended fill into a wandering pile of them. The rule used to read "any
  span but `Voxel`", which named the wrong thing — what makes a fill
  unstrokeable is that it has no limit, not that it is a flood. Give a flood a
  radius and it covers a patch the size you chose, and dragging one is how a
  surface gets worked. `Editor::region_is_bounded` is the whole of it.
- **The brush is a reach, not a shape one span happens to own.** For
  `Span::Voxel` the brush *is* the region — its shape, radius 0 meaning the seed
  alone. For every span that grows, the radius is a bound on the growth, applied
  during the flood in the same place `Reach::within` is: a flood trimmed at the
  end would spread round a corner and come back, which is not what a disc on a
  surface means. Radius 0 therefore means "unbounded" to a flood and "one cell"
  to a voxel span, and the two never meet because a voxel span does not grow.
  Resizing the brush no longer snaps the span back to `Voxel`.
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
- **A setting with no chip is a setting nobody finds.** Mirroring was per axis
  from the day it landed and was only ever named in the help card, so it was
  asked about as though it were missing. The `MIRROR X Y Z` row says both things
  at once: that it is per axis, and which key each axis is on.
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
- **An instance costs eight bytes on disk.** `.vxm` writes the reference — one
  source index, three `i16` of offset, one byte of mirrored axes — and a voxel
  count of zero for the derived layer, whose header still goes out so its place
  in the stack and its name survive. `restore_instances` fills it from the
  reference on the way in, which is also why a model comes back matching the
  source it has *now* rather than what the source looked like when it was saved.
  A reference that cannot mean anything — missing, self, or another instance —
  is dropped rather than refused: the layer under it is real work.
- **The file stores each layer, not the composite.** `.vxm` is `VXM5`: a scene
  range, the object table, a layer count, the active layer, then per layer its
  flags, its object, name, origin, size and own sparse voxel list. Coordinates
  are relative to the layer's origin, so one byte covers a layer anywhere in the
  scene. An object's parent is stored as `parent + 1`, so the root's "no parent"
  is a zero rather than a sentinel that could be read as object 0. `VXM4` (no
  instances), `VXM3` (no objects), `VXM2` (layers, no boxes) and `VXM1` (no
  layers) all still load — an
  older file arrives as a single root object holding every layer, and the two
  oldest are additionally **trimmed** on the way in, so they gain the smaller
  shape by being opened. A file already on disk is not free to rewrite itself.
  That rule has now held five times.
- **An export names its unit, so a voxel has to become a length.** One voxel is
  one millimetre (`format::MM_PER_VOXEL`), which makes a 32³ character 32 mm —
  about right for a desk print and easy arithmetic to scale from. `.vxm` and
  `.vox` are unitless and unaffected.
- **The exporters share one surface extractor, and it is not the renderer's.**
  `format::surface` welds corners — a cube is eight vertices, not twenty-four —
  because that is what makes a mesh *manifold*, which is the thing a slicer
  needs and the thing an unwelded soup renders identically without. It cannot be
  `voxel-render::mesh`: that culls back faces, keeps quads per chunk for
  incremental rebuilds, and knows nothing about which part a cell belongs to —
  and `voxel-core` cannot depend on `voxel-render` anyway, which is what keeps
  the arrow pointing one way.
- **A part is closed on its own.** Under `Grouping::Objects` a face is emitted
  wherever the neighbour is not in the *same* part, so two parts that touch each
  get their own wall. Sharing it would leave both open. Checked by a property
  rather than a count: a closed genus-0 surface satisfies `V - E + F = 2`, which
  a mesh with a hole or a duplicated vertex fails and a face count does not.
- **A file is the model, not the view.** Hidden layers are exported, the rule
  `format::native` already follows — hiding a layer to work on what is under it
  must not quietly drop it from a file.
- **`.obj` is for editing, `.3mf` is for printing, and the difference is quads.**
  OBJ keeps them, which is what a voxel surface is made of; a modeller handed
  triangles has twice the faces and a diagonal through each. 3MF has no quads,
  so each becomes two triangles sharing its diagonal — but it carries units,
  several named objects and colour, which is what a slicer wants and what `.vox`
  and STL cannot say. The ZIP writer is hand-rolled and stored-only, for the
  reason the PNG writer is: the alternative reads six compression methods for a
  format we only write.
- **The interchange formats are write-only.** Reading OBJ or 3MF back means
  voxelising an arbitrary mesh, which is a different program. `.vxm` is the
  document; these are what a finished model leaves in.
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
cd crates && cargo fmt --check   # CI gates this; `cargo fmt` fixes it

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
│   ├── format/            .vxm (ours) and .vox (MagicaVoxel)
│   └── examples/          write-cost.rs — what a write costs, by shape
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
├── skills/                voxeler-modeling, voxeler-editing
└── docs/                  ARCHITECTURE.md, DEVELOPMENT.md
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
  `owner_at` filter on `Layer::shown`, not on `visible` — a layer inside a
  hidden object is on screen if some path asks the wrong one. A path that reads
  `Layer::at` directly skips both.
- **A layer is switched on and still not drawn.** An object above it is hidden.
  `list_objects` says which, and the layer panel draws a hollow box with a pip
  for exactly this case.
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
- **An agent cannot find where to save.** It is not being told: check that
  `ToolHost::instructions` is reaching `initialize`, and that `voxeler mcp` was
  given the directories rather than left to inherit a working directory.
- **An MCP client connects and then nothing works.** Check the `endpoint` event
  is absolute and that `POST /messages` is served as well as `/message`; both
  spellings are in the wild, and serving one silently breaks the other's clients.
- **A click on empty space does nothing.** That is `plane_is_open`: the active
  layer has something in it, so the work plane is closed. `A` starts a layer
  that can be placed anywhere.
- **One click does two edits.** The stroke is re-targeting onto its own work.
  Check that `target_at` is going through `cast_masked` with `Drag::before`, and
  that `continue_stroke` is not being reached inside `app::is_click`'s dead zone.
- **A click does nothing.** Check `over_panel` first — a HUD rectangle that
  claims more than it draws eats clicks with no feedback at all. After that,
  check whether `target_at` is returning `None`: for everything but build, no
  voxel under the cursor means no target, by design.
- **The editor feels slow on a large display.** The renderer caps at
  `MAX_PIXELS` (1.4 M) and upscales; if that is being hit, the cost is the
  rasterizer, and greedy meshing is the lever, not the cap.
- **The editor feels slow on a large *scene*.** Different cost, and measure
  before guessing. At 256³ what is left is the undo history, which stores an
  `Edit` per changed cell and so costs ~170 MB for a fill of the whole scene.
  Lookups, extraction and box growth are no longer among them: a full 256³ fill
  is 319 ms against the 134 ms it costs with the box declared up front, and the
  remaining 2.4× is memcpy traffic that only a looser box would remove.
