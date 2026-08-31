# Furnance — Developer Guide

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
voxeler  (the window: winit + softbuffer)
   │
   ├── voxel-render   faces → transform → clip → raster → z-buffer → framebuffer
   │
   └── voxel-core     the grid, the palette, undo/redo, the file formats
```

The dependency only points downward. `voxel-core` knows nothing about drawing —
it does not even have a vector type, and the raycaster takes `[f32; 3]` arrays
so it stays that way. `voxel-render` knows nothing about editing. Everything
about *what a click means* is in `voxeler/src/editor.rs`, which is why that file
has tests and `app.rs` has almost none.

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

## Key patterns

- **Out of bounds reads as air.** `VoxelModel::get` returns 0 outside the grid,
  and takes `i32` rather than `u16`, because every caller arrives from
  arithmetic that can go negative. Face extraction leans on this: the outside of
  the model is air, so boundary faces come out of the same rule as interior
  ones, with no special case.
- **A no-op write is not an undo step.** `Stroke::set` drops a write that
  changes nothing, so a drag re-painting one voxel forty times produces one
  entry and a click that changed nothing does not consume an undo.
- **A slice hides layers from the mesh *and* the pick.** Treating hidden layers
  as air in the extractor is what puts a lid on the cross-section; re-casting
  against a sliced copy is what stops a click reaching a voxel that is not on
  screen. Both, or the slice is a lie in one direction.
- **The HUD's layout and its hit test live together.** `hud::palette_hit` is the
  exact inverse of the swatch layout, tested swatch by swatch, for the reason
  `kessel`'s `window_to_console` is: a click landing one swatch off reads as a
  broken editor rather than as an off-by-one.
- **A missing file is not an error.** `voxeler robot.vxm` in an empty directory
  starts a model. Refusing would mean the tool could only open what some other
  tool had already made.
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

`--thumbnail` renders one framed view and exits without opening a window. It is
how a model gets an icon, and how the renderer gets checked on a machine with no
display — which is also the fastest way to see whether a rendering change did
what you meant.

## Project Structure

```text
furnance/
├── crates/voxel-core/     the model, host- and render-free
│   ├── model.rs           the dense grid
│   ├── palette.rs         256 colours; 0 is air
│   ├── edit.rs            Stroke + History
│   ├── raycast.rs         Amanatides–Woo grid traversal
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
- **The editor feels slow on a large display.** The renderer caps at
  `MAX_PIXELS` (1.4 M) and upscales; if that is being hit, the cost is the
  rasterizer, and greedy meshing is the lever, not the cap.
