# rs-voxeler

A voxel model editor and software renderer in Rust — the 3D successor to
[`rs-kessel`](../rs-kessel), and a Rust rewrite of the ideas in the Python
`voxeler`.

The first milestone is deliberately small: **build a model in your own editor,
save it, load it back, and see it rendered.** No VM, no scene graph, no game
yet — those come after the asset pipeline is real.

```bash
cd crates && cargo build --release
./crates/target/release/voxeler models/robot.vxm
```

![the editor](docs/editor.png)

## What exists

| crate | what it is |
| --- | --- |
| `voxel-core` | the model: a stack of dense grids, a 256-colour palette, undo/redo, `.vxm` and MagicaVoxel `.vox` |
| `voxel-render` | a software rasterizer: face extraction, orbit camera, z-buffer, flat shading, PNG output |
| `voxeler` | the editor: a window, four tools across four reaches, layers, a palette you can click, and an MCP server |

## Using the editor

```bash
voxeler [FILE] [--size N] [--mcp [PORT]]
voxeler mcp [DIR...]              serve an agent over stdio, headless
voxeler attach [DIR]              open a window onto a running `voxeler mcp`
voxeler FILE --thumbnail out.png [--width N] [--height N]
```

`FILE` defaults to `model.vxm` and does not have to exist — a missing file
starts a **32³** volume with one voxel at its centre to build against.
`--size 64` gives you the larger volume; a file that already exists keeps
whatever size it was saved at, so `--size` only ever describes a new model. A
`.vox` extension reads and writes MagicaVoxel's format; anything else is the
editor's own `.vxm`.

The volume is centred on the origin, spanning ±size/2 on every axis, and the
grid runs through the middle of it. That grid is the work plane, and it is where
a layer *starts*: while the active layer is empty you can click the plane
directly, from either side, so a new layer — or one you have just erased the last
voxel of — is never a dead end. Once that layer holds something, the plane
closes and you build against what you made, so empty space stops being clickable
and a click meant for the camera cannot place a voxel. Press `A` for a new layer
to start another part somewhere else. The other tools act on a voxel, and do
nothing over empty space.

| | |
| --- | --- |
| left click / drag | apply the current tool |
| right drag, alt+left drag | orbit |
| middle drag, shift+drag | pan |
| wheel | zoom |
| `B` `E` `P` `I` | build, erase, paint, pick colour |
| `1` `2` `3` `4` | reach: voxel, axis, plane, volume |
| `9` `0` `C` | brush smaller, bigger, cube/ball |
| `[` `]` / `-` `=` | colour ∓1 / ∓16 |
| `X` `Y` `Z` | mirror the edit across each axis (`M` = `X`) |
| `L` `shift+L` | next / previous layer |
| `A` `D` | add / delete a layer |
| `T` | trim the layer's box to its contents |
| `V` `N` | show-hide / rename the active layer |
| `K` `J` `U` | move the layer up / down, merge it down |
| `G` | ground grid |
| `,` `.` `\` | slice down, slice up, slice off |
| `F` `R` | frame the model, reset the view |
| `ctrl+Z` `ctrl+Y` | undo, redo |
| `ctrl+S` `ctrl+E` | save, export `.vox` |
| `ctrl+D` | subdivide the scene ×2 |
| `ctrl+R` `ctrl+N` `ctrl+Q` | reload, clear, quit |
| `H` | the same list, in the window |

## Two choices, not eleven tools

What a click does and how far it reaches are separate settings, so every tool
is available at every reach rather than as its own entry in a long list:

| | `1` voxel | `2` axis | `3` plane | `4` volume |
| --- | --- | --- | --- | --- |
| `B` build | place a voxel, or stamp a brush | extrude to the wall | fill across the face | flood the cavity |
| `E` erase | remove one, or a brush of them | drill through | strip the face | delete the part |
| `P` paint | recolour one | recolour the run | recolour the face | recolour the part |

A **region** — plane or volume — grows over cells holding the same palette index
as the one under the cursor. Building starts on air, so it floods air; erasing
and painting start on a voxel, so they stop where the colour changes. On a
one-colour model that is "everything connected"; on a model with a red panel on
a blue body it is the panel. Nothing reaches past a slice, and a fill is a
click rather than a drag — it happens once, and undoes in one step.

The **brush** is the voxel span's shape: `9` and `0` size it, `C` switches
between a cube and a ball, and the outline in the viewport shows its extent.
Radius 0 is the single voxel everything else is measured from.

## Layers

A layer is a grid of its own, and the stack composites top down: what you see at
a cell is the topmost visible layer holding something there. So an armour layer
sits *over* the body it covers rather than consuming it — hide the armour and
the body is still underneath, whole.

```text
  ARMOUR   · █ █ ·        hide ARMOUR →   █ █ █ █
  BODY     █ █ █ █                        (the body, intact)
```

### Each layer has its own box

A scene's size is the **range** voxels may occupy, not a grid that gets
allocated. Each layer carries an origin and a size of its own and costs only
that, so a ground plane, a tree and a character can share a 64³ scene without
three copies of it:

```text
  GROUND     origin  0, 0, 0    size 64 x  5 x 64
  TREE       origin 20, 5,10    size 16 x 32 x 16
  CHARACTER  origin 40, 5,40    size 16 x 16 x 16
                                     ─────────────
  allocated                          32 769 cells
  four 64³ grids would be         1 048 576 cells
```

The box **grows to fit** whatever you draw, so a new layer starts empty and
costs nothing until used, and you never have to size one before drawing in it.
Declaring a box up front is a statement of where a part will live, not a wall:

```
add_layer  name=TREE  origin=[20,5,10]  size=[16,32,16]
```

It shrinks only when asked — `T` in the editor, `trim_layer` over MCP — so
erasing and redrawing in one spot does not churn the allocation. The active
layer's box is outlined in the viewport, since where it sits is otherwise the
one thing you cannot see.

`.vxm` is now **VXM3** and stores each layer's box. `VXM2` and `VXM1` still
load, and are trimmed on the way in: an old file gains the smaller shape simply
by being opened.

The panel under the palette lists the stack, top first, with each layer's own
voxel count. Click a row to select it, click its box to show or hide it.

Interactive tools write to the **active layer** and only to it. That is what makes layers
worth having, and it has one consequence worth knowing: clicking a voxel some
other layer owns does nothing, and the status line says which layer holds it.
Building is the mirror image — a voxel a *higher* layer is showing never blocks
a build underneath it, because working under something is what a lower layer is
for. A fill selects on what you can see and writes what you own.

`ctrl+Z` undoes layers being added, deleted, moved and merged, along with the
edits made on them. Showing and hiding is not undoable — it is a thing you do
constantly, and undo would spend its first few presses turning layers back on —
but it *is* saved, because which layers you had hidden is part of the model.

`.vxm` stores every layer's own grid, its name, whether it was shown, and which
one you were editing, so hiding a layer and saving never throws it away. `.vox`
has nowhere to put a stack, so `ctrl+E` writes the composite as one model and
says so.

**Mirroring** is per axis: `X`, `Y` and `Z` each toggle a plane through the
middle of that axis, and the `MIRROR X Y Z` row under the tools shows which are
on. Two active planes give four copies of every stroke and three give eight. It
reflects the *edit*, not the model — nothing is written that the stroke did not
touch.
Turning mirroring on does not go back and symmetrise what is already there, so
a model that is deliberately lopsided stays lopsided while you work on it
symmetrically.

`ctrl` is `cmd` on macOS; both work everywhere.

## Driving it from an agent

The repository includes a reusable modeling skill at
[`skills/voxeler-modeling/SKILL.md`](skills/voxeler-modeling/SKILL.md), covering
reference-based modeling, layered batch edits and verified previews. Ask your
agent to read it for an asset task, or install the `voxeler-modeling` directory
in your agent's skill directory for discovery.

Two transports, because they answer different questions.

**`voxeler mcp` — stdio, headless.** The agent starts it, so "is the server
running?" never comes up. Name the directories its `open`/`save` tools may
reach; they are the only part of the filesystem it can see:

```json
{ "mcpServers": { "voxeler": { "command": "voxeler",
                               "args": ["mcp", "/path/to/models", "/path/to/voxels"] } } }
```

Replace those paths with existing absolute directories. JSON arguments are passed
directly to the process: `~` and environment variables are not shell-expanded.

**Name them.** A desktop MCP client spawns its servers with whatever working
directory the *app* happened to have, so without an argument neither you nor the
agent can say where a bare file name lands. (If that working directory turns out
to be the filesystem root, the server refuses to start rather than quietly
handing an agent every file on the machine.)

The directories are reported back in the server's `initialize` instructions, so
the agent is told where it may write before it tries. Paths given to the tools
may be **absolute inside any of them**, or relative to the first.

**`voxeler attach` — a window onto that session.** The stdio server is headless,
so this is how a person joins and watches the model being built:

```bash
voxeler attach                # the session rooted here, or the only one running
voxeler attach ~/models       # a particular one
```

It is a viewer, not a second editor: the camera is yours — orbit, pan, zoom, `F`
to frame, `,` `.` `\` to slice, `G` for the grid — and the keyboard is the
agent's. There is one document and one undo history, and they live in the
server. If nothing is running, it says so and how to start one.

**`voxeler FILE --mcp [PORT]` — SSE, from a window you already have open.** For
when you were editing first and want an agent to join *you*:

```json
{ "mcpServers": { "voxeler": { "type": "sse", "url": "http://127.0.0.1:8730/sse" } } }
```

Both listeners bind loopback only — they hand control of your document to
whoever connects.

| tool | what it does |
| --- | --- |
| `describe_model` | size, voxel count, bounds, the layer stack, active layer, colour |
| `put_voxel` | one voxel; colour 0 erases |
| `put_rect` | a solid axis-aligned box, corners inclusive |
| `apply_edits` | ordered voxel/box/ellipsoid/line edits across layers, one undo step |
| `put_ellipsoid`, `put_line` | ellipsoid or rounded thick line, optionally on a named layer |
| `paint` | recolour the voxels in a box, creating none |
| `fill` | flood fill from a cell, over the active layer's connected shape |
| `set_color`, `find_color` | select a palette index; find the one nearest an RGB |
| `set_palette_color` | change an index's RGB across all layers, with undo/redo |
| `add_layer`, `select_layer`, `set_layer_visible`, `trim_layer` | the layer stack and its boxes |
| `screenshot` | clean PNG preview, camera presets, lighting, optional file output |
| `undo`, `redo` | take back whole tool calls |
| `subdivide` | scale the scene up so every voxel becomes `factor`³ of them |
| `new_model`, `open_model`, `save_model`, `list_models` | files inside the root; listing includes subdirectories |

`screenshot` is the one that shows rather than counts — a voxel total cannot
tell you the arm is on backwards. It takes `yaw` and `pitch` in degrees and
frames the model's contents, so it fills the picture whatever the volume's size,
and it puts the camera back where it found it.

One tool call is **one undo step**, so `undo` takes back a whole fill however
many voxels it moved. The history is shared with whoever has the window.

`subdivide` is how a coarse model becomes a detailed one: get the silhouette
right at 16³, then scale up and carve into the room you have made. It applies to
**every layer at once** and is one undo step — the scene's own size included, so
`ctrl+Z` really does put the coarse model back. It is a plain replication rather
than a smoothing: corners stay square, so what you carve is carved against what
you drew. `ctrl+D` in the editor does the same by 2.

The file tools accept absolute paths inside any allowed directory, or paths
relative to the first directory. They refuse paths outside those directories,
`..` components and symlinks. Under `--mcp`, where you opened the document
yourself, file access is unavailable.

Returned `path` fields from `list_models`, `save_model` and `screenshot` follow
the same rule: relative to the first directory, absolute otherwise. They can be
passed back to file tools without changing which file they name. `list_models`
also returns an `absolute` field for each file; prefer it when selecting assets
across multiple directories. `describe_model.path` is only a display basename.

Coordinates are model space — `0..size` on each axis, +Y up, the same
coordinates the file stores.

**Every edit reports what it did**, because an agent cannot see the screen and
"ok" tells it nothing:

```json
{"targeted": 32, "added": 16, "removed": 0, "repainted": 1, "unchanged": 15,
 "layer": "ARMOUR", "layer_voxels": 16, "model_voxels": 32}
```

The four outcomes are exclusive and sum to `targeted`, which is what makes them
a measurement rather than a reassurance: ask for a 5×5×5 box and read
`targeted: 125, added: 0` and you know you aimed at solid rock.

The agent and you share one editor, one undo history and one file. A whole tool
call is **one** undo step, so a box an agent filled costs you one `ctrl+Z`.
Basic MCP tools write to the active layer and only to it — `fill` refuses a cell another
layer owns rather than copying that layer's shape onto this one, and says which
layer to select instead. Batch and curved modeling tools also accept an explicit
layer; palette RGB edits affect every use of that index across the model.

Not exposed: deleting or merging layers and changing the user's camera.
Screenshot settings affect the returned image only; the editor's view is restored.

### Batch and curved modeling

`apply_edits` takes an `edits` array. Each operation has `op` set to `voxel`,
`rect`, `ellipsoid` or `line`. Top-level `layer` and `color` set defaults;
individual operations may override them. An omitted layer uses the active
layer, but explicit layer arguments never change the selection.

```json
{
  "layer": "BODY",
  "color": 1,
  "edits": [
    {"op": "ellipsoid", "center": [63.5, 40, 64], "radii": [20, 27, 16]},
    {"op": "line", "from": [45, 50, 64], "to": [33, 32, 64], "radius": 4},
    {"op": "rect", "from": [50, 5, 55], "to": [59, 12, 70]},
    {"op": "voxel", "x": 63, "y": 40, "z": 80, "color": 65}
  ]
}
```

Every operation is validated before any voxel is written. Later writes win on
overlaps, and the entire call is one undo step, even across layers. Reports count
write attempts: a cell touched twice contributes twice to `targeted`. A request
can contain up to 262,144 operations and 2,097,152 candidate cells; shape bounding
boxes count toward that budget before filtering. Split larger requests into
separate calls (the SSE transport also has an 8 MiB message limit).

`put_ellipsoid` uses the same `center`, `radii`, `color` and `layer` arguments.
`put_line` uses `from`, `to` and `radius`; rounded ends make it a capsule, or a
sphere when the endpoints coincide. Integer coordinates denote voxel centers;
fractional centers such as `63.5` allow exact symmetry in an even-sized scene.
Radii must be greater than zero and at most 256. Centers/endpoints must be inside
`0..=size-1`; curved surfaces are clipped at the scene boundary. Boxes and single
voxels reject out-of-range coordinates. Colour 0 erases the addressed layer.

`set_palette_color` accepts `{"index":65,"r":200,"g":25,"b":36}`. It changes
every voxel using index 65, including those in hidden layers, and is undoable.
It does not select that colour; use `set_color` for selection. RGB changes are
preserved in both `.vxm` and `.vox` files.

### Preview and file discovery

`screenshot` hides scene/layer bounds by default and uses softer lighting than
the editor. `show_bounds: true` includes those editing guides. `ambient` and
`diffuse` each range from 0 to 1 (defaults 0.7 and 0.3).

```json
{"view":"front","width":768,"height":768,"ambient":0.8,"diffuse":0.2,"path":"models/front.png"}
```

`view` accepts `front`, `back`, `left`, `right`, `top` and `three_quarter`.
Front views from +Z toward -Z; right views from +X. Explicit `yaw` or `pitch`
overrides that angle of the preset. Without a preset or explicit angles, the
editor camera's angles are used. Neither successful nor failed PNG output changes
the user's camera, grid setting, undo history or model save state.

An optional `path` writes the same PNG returned in the image block. Its parent
directory must exist, its extension must be `.png`, and it must be inside an
allowed directory. PNG file output is unavailable in the windowed SSE server, just like
model file output; returning an image still works. `--thumbnail` also frames the
model's contents, hides bounds and uses the softer preview lighting.

`list_models` searches all allowed directories recursively by default and sorts
paths. For one directory only, use `{"directory":"models","recursive":false}`.
Directory names may be absolute inside an allowed directory, or relative to the
first. File tools reject symlink paths, and recursive listing skips symlink files
and directories.

## Where this is going

The order is asset → renderer → one game → only the API that game needed:

1. ~~`voxel-core` — model, palette, `.vox` interop~~
2. ~~`voxeler` — the model editor~~
3. ~~`voxel-render` — visible faces, camera, z-buffer, flat light~~
4. `voxel-scene` — entities, transforms, parent-child
5. a Luax bridge — spawn, transform, camera, render
6. one fixed-camera action game
7. and only then: collision, world chunks, animation, particles

Deliberately *not* on that list yet: greedy meshing, a world editor, chunked
terrain, bone skinning, frame animation. Each is a real thing to want and each
would be built on the wrong foundation today.

## Licence

MIT OR Apache-2.0.
