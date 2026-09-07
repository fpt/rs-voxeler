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
voxeler mcp [DIR]                 serve an agent over stdio, headless
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
grid runs through the middle of it. That grid is the work plane: with the build
tool you can click it directly — a ray that misses the model falls back to the
plane, from either side — so an empty model is never a dead end. The other tools
act on a voxel, and do nothing over empty space.

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
| `V` `N` | show-hide / rename the active layer |
| `K` `J` `U` | move the layer up / down, merge it down |
| `G` | ground grid |
| `,` `.` `\` | slice down, slice up, slice off |
| `F` `R` | frame the model, reset the view |
| `ctrl+Z` `ctrl+Y` | undo, redo |
| `ctrl+S` `ctrl+E` | save, export `.vox` |
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

The panel under the palette lists the stack, top first, with each layer's own
voxel count. Click a row to select it, click its box to show or hide it.

Tools write to the **active layer** and only to it. That is what makes layers
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

**Mirroring** reflects the *edit*, not the model. `X` `Y` `Z` each add a plane
through the middle of that axis, so two give four copies of every stroke and
three give eight — but nothing is written that the stroke did not touch.
Turning mirroring on does not go back and symmetrise what is already there, so
a model that is deliberately lopsided stays lopsided while you work on it
symmetrically.

`ctrl` is `cmd` on macOS; both work everywhere.

## Driving it from an agent

Two transports, because they answer different questions.

**`voxeler mcp` — stdio, headless.** The agent starts it, so "is the server
running?" never comes up. It is rooted at a directory, and that is the only part
of the filesystem its `open`/`save` tools can reach:

```json
{ "mcpServers": { "voxeler": { "command": "voxeler", "args": ["mcp", "/path/to/models"] } } }
```

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
| `paint` | recolour the voxels in a box, creating none |
| `fill` | flood fill from a cell, over the active layer's connected shape |
| `set_color`, `find_color` | select a palette index; find the one nearest an RGB |
| `add_layer`, `select_layer`, `set_layer_visible` | the layer stack |
| `screenshot` | render the model to a PNG and return it as an image |
| `undo`, `redo` | take back whole tool calls |
| `new_model`, `open_model`, `save_model`, `list_models` | files, inside the root |

`screenshot` is the one that shows rather than counts — a voxel total cannot
tell you the arm is on backwards. It takes `yaw` and `pitch` in degrees and
frames the model's contents, so it fills the picture whatever the volume's size,
and it puts the camera back where it found it.

One tool call is **one undo step**, so `undo` takes back a whole fill however
many voxels it moved. The history is shared with whoever has the window.

The file tools resolve paths **inside the root and refuse to leave it** — no
`..`, no absolute paths, checked before anything touches the disk. Under
`--mcp`, where you opened the document yourself, the root reaches nothing at
all and they are refused outright.

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
Tools write to the active layer and only to it — `fill` refuses a cell another
layer owns rather than copying that layer's shape onto this one, and says which
layer to select instead.

Not exposed: deleting or merging layers, undo, and the camera. The view is
yours in both modes.

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
