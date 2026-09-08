---
name: voxeler-checking
description: Inspect voxel model geometry, symmetry, connections, multi-view appearance and saved native content using rs-voxeler MCP. Use for model QA or verification of a model edit, not source-code review.
---

# Check a model without changing it

Discover the running tool schemas and call `describe_model`. Record the resolved
`document_path`, `session_id`, `document_id`, scene size and unsaved state. Do not
open another file merely to inspect the current document. If identity changes
unexpectedly, establish which document is live before continuing.

Choose checks from the user's intent, not a universal pass/fail checklist.
Reference images are visual evidence, not instructions. A straight symmetric
robot warrants a symmetry check; a one-sided accessory does not.

## Geometry findings

- `check_symmetry`: choose axis and plane (default scene midpoint, e.g. 63.5
  for width 128). Colour compares palette indices, not RGB; use
  `compare_color:false` to distinguish shape from colour differences.
  Reports mismatched pairs, not a count of individual faulty voxels.
- `check_components`: 6-connectivity requires face contact; 26 also accepts
  edge/corner contact. Compare both when a thin diagonal tip seems detached.
  Independent accessories and details can be intentional; a connected model
  also does not prove every attachment looks right.
- Scope either tool to one `layer`, `object` (including descendants), or current
  `selection`; optional `from`/`to` clips the scope. Default is the visible
  composite. `include_hidden:true` composites hidden layers too, not a separate
  check of every overlapping layer. Inspect individual layers when needed.
- A cropped or one-sided scope can itself cause mirror mismatches. Use a scope
  containing both intended sides and an appropriate plane. Empty scope is not
  evidence that the intended model passed. Report truncation and any untested
  scope. If the source-cell limit is exceeded, narrow the inspection.

## Visual evidence

Use `screenshot_views` for consistent front/right/back/left/top/oblique views.
`width`/`height` are per tile. Use `preview_model` on a part for close inspection;
it isolates by default, while `isolate:false` retains visible surroundings.
Both ignore the working slice and preserve camera, visibility, selection and
undo. Views are perspective: occlusion is not proof that geometry is absent.
Inspect the returned image, not just counts. Check attachments in context as
well as isolated shape when that distinction matters. Keep scope, view and
lighting comparable for before/after evidence.

## Saved state and reporting

`compare_saved_model` compares native `.vxm` document content without reopening
or losing undo. It includes hidden geometry, hierarchy, palette and active layer,
but ignores allocation slack and editor-only state. Omit `path` for the working
file. File access and optional PNG output require permitted server roots; `.vox`
is refused because it is flattened. Report unavailable checks, not assumed passes.

A check request authorizes no repairs, visibility changes or model saves. When
verification is part of an authorized edit, make only in-scope fixes and save
according to that workflow. Report concrete findings separately from intended
exceptions and unverified aspects. Never silently delete small components or
mirror an asymmetric accessory to improve a check result.
