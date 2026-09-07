---
name: voxeler-editing
description: Revise an existing voxel model with the rs-voxeler MCP tools — select parts, move, rotate, flip, copy and duplicate them, and adjust layers and palette. Use when changing a model that already exists, rather than building one from scratch or changing the editor's source code.
---

# Revising a model with rs-voxeler

For changing a model that is already there. Building one from a reference is
`voxeler-modeling`; this is for the request that starts "make the arm longer",
"turn the tower", "give it a second wheel".

The difference matters: drawing tools put voxels at coordinates you compute,
where editing tools act on **the voxels that are already there**. Reach for a
selection before reaching for arithmetic.

## Read before you change anything

- `describe_model` first: scene size, layers, active layer, `unsaved`. Every
  coordinate you are about to use is inside that size.
- `screenshot` before and after. A voxel count cannot tell you the arm ended up
  on backwards. Use the same `yaw`/`pitch` both times so the two are comparable.
- If the user has the window open (`voxeler attach`), they are watching. Say
  what you are about to do before a large change.

## Select, then act

```
select_box         from, to
select_connected   x, y, z [, from, to]
describe_selection
```

`select_connected` grows by **material, not colour**, so a part made of several
colours comes out whole. But a limb is attached to its body: without `from`/`to`
the answer to "this arm" is the whole figure. Bound it.

```
select_connected  x=8 y=12 z=11  from=[7,0,0] to=[9,23,23]
```

**Always `describe_selection` before transforming.** It costs one call and tells
you whether you got the part you meant — the voxel count and the box it
occupies. Moving the whole figure when you meant one arm is the mistake this
prevents, and it is not visible until afterwards.

A selection belongs to the layer it was made on and does not follow the active
layer. It is dropped by `undo`, because undo changes what is at those
coordinates.

## Transform

```
move_selection    dx, dy, dz
rotate_selection  axis, turns        quarter turns, either direction
flip_selection    axis
```

Each is one undo step and each leaves the result selected, so they chain without
recomputing coordinates.

`rotate_selection` pivots about the **low corner** of the selection's box, not
its centre. A square footprint is unaffected; anything else will shift, so
follow with `move_selection` if it matters. Turns are counter-clockwise about
the positive axis; `turns: -1` goes the other way, and `+1` then `-1` is exactly
where you started.

`flip_selection` mirrors within the selection's own box — it turns a hand over,
it does not send it to the far side of the scene. For mirroring a part *across
the model*, flip and then move, or use the editor's `X`/`Y`/`Z` mirror when
drawing new geometry.

Voxels pushed outside the scene are **lost**, and the report counts them. Check
`dropped` when moving something near an edge.

## Copy and duplicate

```
copy_selection / cut_selection
paste                 at=[x,y,z]      the clipboard's low corner goes here
duplicate_selection   dx, dy, dz
```

`duplicate_selection` then `flip_selection` is the whole of a mirrored pair —
no coordinate arithmetic:

```
select_connected  ... from/to around the left arm
duplicate_selection  dx=0 dy=0 dz=6
flip_selection       axis="x"
```

A paste writes to the **active layer**, not the one the voxels came from, which
is how you copy a part onto a layer of its own. What lands is left selected.

The clipboard survives `undo` — undo puts the model back, not the clipboard — so
a paste you undid can be pasted again.

## Objects, layers and palette

```
list_objects / create_object / rename_object / delete_object
set_layer_object / reparent_object / set_object_visible / move_object
select_layer / add_layer / set_layer_visible / trim_layer
set_color / find_color / set_palette_color
```

`list_objects` is how you find out what a model's parts are called before
changing one. An object says what a thing is; a layer says how pixels combine.
If the model has no objects yet, naming the parts you touch is a courtesy to the
next session.

`move_object` repositions a part and everything under it in one undo step
without re-creating a voxel — reach for it before erasing and redrawing a limb
somewhere else. It is refused outright if any part of the subtree would leave
the scene.

`delete_object` removes a label, not the work: children and layers move up to
its parent.

A layer reported with `"shown": false` while `"visible"` is true is inside a
hidden object. That is why it is not on screen and not in the voxel count, and
`set_layer_visible` will not bring it back — `set_object_visible` will.

Editing tools write to the active layer only. If a change appears to do nothing,
the voxels probably belong to another layer: the report will name it.

`set_palette_color` recolours every voxel using that index, across all layers —
useful for a scheme change, wrong if you meant one part. To recolour one part,
select it and `paint`.

`trim_layer` after a large erase gives back the space; it moves no voxels.

## Finish

- `screenshot` and compare against the before.
- `save_model` only when the user asked for it, or say that you have not. An
  export to `.vox` flattens layers; `.vxm` keeps them.
- One tool call is one undo step, and the history is shared with whoever has the
  window. If you got it wrong, `undo` — do not paper over it with more edits.
