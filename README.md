# furnance

A voxel model editor and software renderer in Rust — the 3D successor to
[`rs-kessel`](../rs-kessel), and a Rust rewrite of the ideas in `voxeler`.

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
| `voxeler` | the editor: a window, six tools' worth of keys, and a palette you can click |

## Using the editor

```bash
voxeler [FILE] [--size N]
voxeler FILE --thumbnail out.png [--width N] [--height N]
```

`FILE` defaults to `model.vxm` and does not have to exist — a missing file
starts a 64³ volume with one voxel on its floor to build against. A `.vox`
extension reads and writes MagicaVoxel's format; anything else is the editor's
own `.vxm`.

With the build tool you can also click bare floor: a ray that misses the model
falls back to the ground plane, so an empty model is never a dead end. The other
tools act on a voxel, and do nothing over empty space.

| | |
| --- | --- |
| left click / drag | apply the current tool |
| right drag, alt+left drag | orbit |
| middle drag, shift+drag | pan |
| wheel | zoom |
| `B` `E` `P` `I` | build, erase, paint, pick colour |
| `[` `]` / `-` `=` | colour ∓1 / ∓16 |
| `M` `G` | mirror across X, ground grid |
| `,` `.` `\` | slice down, slice up, slice off |
| `F` `R` | frame the model, reset the view |
| `ctrl+Z` `ctrl+Y` | undo, redo |
| `ctrl+S` `ctrl+E` | save, export `.vox` |
| `ctrl+R` `ctrl+N` `ctrl+Q` | reload, clear, quit |
| `H` | the same list, in the window |

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
