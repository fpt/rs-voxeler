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
| `voxel-core` | the model: a dense grid, a 256-colour palette, undo/redo, `.vxm` and MagicaVoxel `.vox` |
| `voxel-render` | a software rasterizer: face extraction, orbit camera, z-buffer, flat shading, PNG output |
| `voxeler` | the editor: a window, four tools across four reaches, and a palette you can click |

## Using the editor

```bash
voxeler [FILE] [--size N]
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

**Mirroring** reflects the *edit*, not the model. `X` `Y` `Z` each add a plane
through the middle of that axis, so two give four copies of every stroke and
three give eight — but nothing is written that the stroke did not touch.
Turning mirroring on does not go back and symmetrise what is already there, so
a model that is deliberately lopsided stays lopsided while you work on it
symmetrically.

`ctrl` is `cmd` on macOS; both work everywhere.

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
