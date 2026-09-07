# Development

How to build, test, measure and change rs-voxeler. `docs/ARCHITECTURE.md` says
what the pieces are and why; this says how to work on them without breaking the
things that were expensive to learn.

## Build and run

```bash
cd crates && cargo build --release
cd crates && cargo test
cd crates && cargo clippy --all-targets -- -D warnings
```

Or through the Makefile from the repository root, which is what CI runs:

```bash
make check          # test + lint, the two commands CI runs
make test
make lint
make run            # debug build on models/robot.vxm
make thumb          # render one frame to /tmp/voxeler.png
```

```bash
./crates/target/release/voxeler models/robot.vxm
./crates/target/release/voxeler models/robot.vxm --thumbnail shot.png
./crates/target/release/voxeler models/robot.vxm --mcp     # SSE on 127.0.0.1:8730
./crates/target/release/voxeler mcp models/                # stdio, headless
./crates/target/release/voxeler attach models/             # a window onto that
```

`--thumbnail` renders one framed view and exits without opening a window. It is
how a model gets an icon, how the renderer gets checked on a machine with no
display, and the fastest way to see whether a rendering change did what you
meant.

## Verify by exit status, never by reading the output

```bash
cargo test; echo $?
```

This is not a style preference. Summarising `cargo test`'s output with `grep` and
`awk` — for instance summing the "passed" column — happily sums the line
`FAILED. 205 passed; 2 failed` and prints a healthy-looking total, which is
exactly how a red build once got reported as green. The exit status is the only
answer that cannot be misread.

## Formatting: do not run `cargo fmt`

**The tree is deliberately not rustfmt-clean, and `cargo fmt --check` is
deliberately absent from CI.** Comments throughout are hand-wrapped to sit under
the code they explain, and rustfmt does not preserve that. Running `cargo fmt
--all` rewrites about twenty-five files and destroys it.

Reformatting is a decision to take on purpose, not a thing to discover from a red
build. Match the surrounding style by hand.

## CI

`.github/workflows/check.yml` runs `make test` and `make lint` on every push to
`main` and every pull request, then builds and runs `--thumbnail` on
`models/robot.vxm` and checks the PNG is non-empty — the one end-to-end check a
headless runner can make: load a model, extract its faces, rasterise them and
write a file. It
installs `libxkbcommon-dev` and `libwayland-dev`, which winit and softbuffer need
on Linux, and caches the cargo registry and `crates/target`.

It runs the same two commands the Makefile names, rather than a second definition
of "is this good" that drifts from the one people run locally.

## How to test

The testing culture here is specific, and worth reading before adding a test.

**Anything with an inverse is tested against its inverse**, not against a
hard-coded expectation. `camera::ray` is checked by reprojecting a ray back to
the pixel it came from; `hud::palette_hit` by walking every swatch the layout
draws. Both pairs are written independently, and nothing else would catch them
drifting apart.

**A test that cannot fail is worse than no test.** The near-plane clip test
asserts the *background* survives above the horizon, and has been checked to fail
when the clipper is disabled. When you add a test for a subtle invariant, break
the invariant on purpose once and watch the test go red before you keep it:

```bash
# e.g. drop the y shift in Layer::grow_into, run, confirm red, restore
cargo test -p voxel-core growing_a_box
```

**Format tests assert on file bytes, not on a round trip.** Export-then-import is
the identity whether the axis change is a rotation or a reflection, so a
round-trip test cannot see a mirrored model at all.

**Fixtures are non-cubic on purpose.** With `sx == sy == sz` the index strides
coincide and a wrong one still round-trips. Use different extents on each axis
and different colours per cell.

**Unit tests are not enough for the editor or the MCP surface.** Several real
bugs — the MCP `fill` ghost copies, the fill that selected all 324 voxels of a
figure, one click adding two voxels — were only visible when the actual binary
was driven end to end. Build it, run it, and check the numbers the tools report
against what you see.

## How to measure

Measure before changing, and let the measurement contradict you. Every
performance decision in this repository was made this way, and several of them
came out the opposite of the plan (see the table at the end of
`docs/ARCHITECTURE.md`).

### The write-cost harness

```bash
cd crates && cargo run --release --example write-cost -p voxel-core
```

`crates/voxel-core/examples/write-cost.rs` times the four write shapes the layer
box is a trade-off between: a bulk fill, a drag, a scatter, and a small part
built off in a corner. `declared` sets the box up front and is the floor — what
those writes cost with no growth at all.

Numbers from the machine this was developed on, which are the ones the
documentation quotes. Absolute values will differ; the *ratios* are the point,
and re-running before and after a change is what the harness is for:

```text
bulk fill  64^3         8 ms   (declared    4 ms)   262144 cells
bulk fill 128^3        31 ms   (declared   20 ms)   2097152 cells
bulk fill 256^3       340 ms   (declared  132 ms)   16777216 cells
drag 2 000 cells        0 ms                          13824 cells
scatter  1 000          13 ms                      16777216 cells
16x32x16 part           0 ms                           8192 cells (8192 ideal)
```

Two things to read off it. The last row is the number the whole per-layer-box
design is judged by: a part in a corner must allocate exactly what it occupies.
And the drag row is why the box policy has not been loosened — a drag was never
the problem, so trading tightness for growth speed would pay for a case that is
already free.

### Everything else

- **Rendering**: `--thumbnail` renders one frame headless. Time that, not the
  window, which is bounded by the display.
- **Editing latency**: drive the binary. The costs that mattered — 43 ms for a
  single voxel edit, 348 ms for a screenshot after one — were both invisible in
  unit tests and obvious in use.
- **Isolate before attributing.** The 3-second `reshape` regression was first
  blamed on the plane tallies; an isolation experiment pointed at the wrong
  suspect until the second walk was removed on its own. Change one thing.

## Where things live

```text
crates/voxel-core/     the model, host- and render-free
  model.rs             the scene range, and the boxed layers in it
  palette.rs           256 colours; 0 is air
  edit.rs              Stroke + History
  raycast.rs           Amanatides–Woo grid traversal
  region.rs            how far one edit reaches: brush, run, flood
  format/              .vxm (ours) and .vox (MagicaVoxel)
  examples/            write-cost.rs, the measurement harness
crates/voxel-render/   the software rasterizer, editor-free
  math.rs camera.rs mesh.rs raster.rs overlay.rs png.rs
crates/voxeler/        the editor
  editor.rs            state and every operation on it — the tested part
  mcp/                 dispatch, wire, http+sse, stdio, tools, session
  attach/              protocol, listener, viewer
  view.rs hud.rs app.rs
models/                sample models
skills/                agent-facing skills for modelling and editing
docs/                  this, ARCHITECTURE.md, and the screenshot
```

The rule for a new module is the crate dependency: if it needs to know what a
click means, it belongs in `voxeler`. If it needs a vector type, it does not
belong in `voxel-core`.

## Constants worth knowing

| | | |
| --- | --- | --- |
| `MAX_DIM` | 256 | the largest scene on any axis |
| `MAX_LAYERS` | 16 | bounds the per-layer cost |
| `CHUNK` | 16 | the unit a mesh rebuild happens in |
| `MAX_BRUSH` | 8 | radius, so the edge is always odd |
| `MAX_PIXELS` | 1 400 000 | the renderer upscales past this |
| default volume | 32³ | `--size` only describes a model that does not exist yet |

## Changing things safely

A short list of the mistakes this codebase has actually made, phrased as what to
check before you repeat one.

- **Do not sweep `model.size()`.** A loop `for z for y for x` over the scene is
  the allocation the layer boxes exist to avoid. Walk the layers.
- **Do not add a second walk to a hot path to keep a tally.** Tally inside the
  loop that is already running. `reshape` runs on every write outside a growing
  box, and a second pass there cost three seconds.
- **Do not read `Layer::at` directly where visibility matters.** `get` and
  `owner_at` filter on `visible`; a path that skips that filter draws a hidden
  layer, or hides a shown one.
- **Do not use `iter_filled` where `iter_filled_in` is meant.** The composited
  view in `format::native::encode` silently deletes hidden layers on save.
- **Do not grow a region on the composite and write it to the active layer.**
  Pass `Reach::layer`. That mismatch produced a hundred-cell invisible ghost.
- **Do not change the axis conversion outside `format::vox`.** The Y/Z change is
  a rotation and must stay at the format boundary.
- **Do not widen the MCP file tools.** `..` is refused lexically, absolute paths
  are checked canonically, symlinks are refused outright, listeners bind
  loopback, and `--mcp` refuses every file tool including `save_model`.
- **Do not make a tool call more than one undo step.** The user shares that
  history.
- **Do not let a palette operation take index 0.** It is air. "Replace black
  with white" on it fills the model, which is the one failure those tools are
  shaped against — use `slot_arg`, not the drawing tools' colour argument.

## Commits and pull requests

Commit subjects name the crate and what changed, in the imperative and in
lowercase after the prefix — `voxel-core: an emptied layer gives its box back at
once`. The body says *why*, and includes the measurement when one drove the
change.

Open a pull request against `main`; CI must be green before merge. Where a change
alters a documented rule, update `CLAUDE.md` and `docs/ARCHITECTURE.md` in the
same commit — those documents are the reason a decision survives long enough to
be reconsidered on purpose.
