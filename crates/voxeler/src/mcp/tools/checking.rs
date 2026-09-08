//! Read-only model inspection. Findings describe geometry, not artistic intent.
use super::*;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

pub(super) type Cells = BTreeMap<[i32; 3], (u8, usize)>;
const MAX_CELLS: usize = 2_097_152;

pub(super) fn scope_schema() -> Value {
    json!({
        "layer": {"type": ["integer", "string"]},
        "object": {"type": ["integer", "string"], "description": "Includes descendant objects."},
        "selection": {"type": "boolean", "default": false},
        "include_hidden": {"type": "boolean", "default": false},
        "from": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3},
        "to": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}
    })
}

pub(super) fn schemas() -> Vec<ToolInfo> {
    let mut symmetry = scope_schema();
    symmetry["axis"] = json!({"type": "string", "enum": ["x", "y", "z"]});
    symmetry["plane"] = json!({
        "type": "number",
        "description": "Integer or half-integer model coordinate; default is the scene midpoint.",
    });
    symmetry["compare_color"] = json!({"type": "boolean", "default": true});
    symmetry["limit"] = json!({"type": "integer", "minimum": 1, "maximum": 1000, "default": 100});
    let mut components = scope_schema();
    components["connectivity"] = json!({"type": "integer", "enum": [6, 26], "default": 6});
    components["limit"] = symmetry["limit"].clone();
    vec![
        ToolInfo {
            name: "check_symmetry",
            description:
                "Read-only mirror comparison of geometry and optionally palette indices. Give \
                 one of layer/object/selection, or use the visible composite. Optional from/to \
                 clips the inspected region; include_hidden composites hidden layers too. \
                 Reports mismatched PAIRS, bounded coordinate samples and owning layers. \
                 Asymmetry is a finding, not necessarily a defect. At most 2097152 inspected \
                 source voxels; narrow the scope if refused.",
            input_schema: json!({
                "type": "object",
                "properties": symmetry,
                "required": ["axis"],
            }),
        },
        ToolInfo {
            name: "check_components",
            description:
                "Read-only connected components, ignoring colour seams. 6 means face contact; \
                 26 includes edges and corners. Give one of layer/object/selection, optionally \
                 from/to and include_hidden. Returns total component count and the largest \
                 components first, with sizes, bounds and layers. Separate components are NOT \
                 automatically floating defects. At most 2097152 inspected source voxels; \
                 narrow the scope if refused.",
            input_schema: json!({"type": "object", "properties": components}),
        },
        ToolInfo {
            name: "compare_saved_model",
            description:
                "Compare the current document with a native .vxm file without opening it, \
                 changing selection or discarding undo. Omit path for the working file. \
                 Compares canonical native document content, including hidden layers, objects, \
                 palette and active layer; not camera, selection, clipboard or allocation \
                 high-water marks. A .vox export is refused because it flattens the document. \
                 File access obeys the same roots as open_model.",
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
            }),
        },
    ]
}

pub(super) fn collect(editor: &Editor, args: &Value) -> Result<Cells, String> {
    let selected = bool_arg(args, "selection", false)?;
    let hidden = bool_arg(args, "include_hidden", false)?;
    if usize::from(selected)
        + usize::from(args.get("layer").is_some())
        + usize::from(args.get("object").is_some())
        > 1
    {
        return Err("choose only one of layer, object or selection".into());
    }
    let region = match (args.get("from"), args.get("to")) {
        (None, None) => None,
        (Some(_), Some(_)) => Some(range(args, editor.model())?),
        _ => return Err("from and to must be supplied together".into()),
    };
    let inside =
        |p: [i32; 3]| region.is_none_or(|(lo, hi)| (0..3).all(|i| p[i] >= lo[i] && p[i] <= hi[i]));
    let model = editor.model();
    let layers = if selected {
        vec![editor.selection.as_ref().ok_or("nothing selected")?.layer()]
    } else if args.get("layer").is_some() {
        vec![layer_arg(editor, args)?]
    } else if args.get("object").is_some() {
        let subtree = model.subtree(object_arg(editor, args, "object")?);
        (0..model.layer_count())
            .filter(|i| subtree.contains(&model.layers()[*i].object))
            .collect()
    } else {
        (0..model.layer_count()).collect()
    };
    let mut result = Cells::new();
    let mut inspected = 0;
    for layer in layers {
        if !hidden && !model.layers()[layer].shown() {
            continue;
        }
        let mut insert = |p, c| -> Result<(), String> {
            if c == 0 || !inside(p) {
                return Ok(());
            }
            inspected += 1;
            if inspected > MAX_CELLS {
                return Err(format!(
                    "inspection exceeds {MAX_CELLS} source voxels; narrow layer/object or from/to"
                ));
            }
            result.insert(p, (c, layer));
            Ok(())
        };
        if selected {
            for p in editor.selection.as_ref().unwrap().cells() {
                insert(p, model.get_in(layer, p[0], p[1], p[2]))?;
            }
        } else {
            for (p, c) in model.iter_filled_in(layer) {
                insert(p.map(i32::from), c)?;
            }
        }
    }
    Ok(result)
}

pub(super) fn bounds(cells: &Cells) -> Option<([i32; 3], [i32; 3])> {
    let mut keys = cells.keys();
    let first = *keys.next()?;
    Some(keys.fold((first, first), |(lo, hi), p| {
        (
            std::array::from_fn(|i| lo[i].min(p[i])),
            std::array::from_fn(|i| hi[i].max(p[i])),
        )
    }))
}

fn limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None => Ok(100),
        Some(v) => v
            .as_u64()
            .filter(|v| (1..=1000).contains(v))
            .map(|v| v as usize)
            .ok_or("limit must be 1..=1000".into()),
    }
}

pub(super) fn check(editor: &Editor, name: &str, args: &Value) -> Result<CallResult, String> {
    let limit = limit(args)?;
    let cells = collect(editor, args)?;
    let report = if name == "check_symmetry" {
        let axis = match args.get("axis").and_then(Value::as_str) {
            Some("x") => 0,
            Some("y") => 1,
            Some("z") => 2,
            _ => return Err("axis must be x, y or z".into()),
        };
        let plane = match args.get("plane") {
            None => f64::from(editor.model().size()[axis] - 1) / 2.,
            Some(v) => v
                .as_f64()
                .filter(|p| {
                    p.is_finite()
                        && *p >= 0.
                        && *p <= f64::from(editor.model().size()[axis] - 1)
                        && (p * 2.).fract() == 0.
                })
                .ok_or("plane must be an integer or half-integer inside the scene")?,
        };
        let colors = bool_arg(args, "compare_color", true)?;
        let reflect = |mut p: [i32; 3]| {
            p[axis] = (plane * 2.) as i32 - p[axis];
            p
        };
        let mut pairs = BTreeSet::new();
        for &p in cells.keys() {
            pairs.insert(p.min(reflect(p)));
        }
        let mut mismatches = 0;
        let mut samples = Vec::new();
        for p in pairs {
            let q = reflect(p);
            let a = cells.get(&p);
            let b = cells.get(&q);
            if a.is_some() != b.is_some() || (colors && a.map(|v| v.0) != b.map(|v| v.0)) {
                mismatches += 1;
                if samples.len() < limit {
                    samples.push(json!({
                        "position": p,
                        "reflected": q,
                        "color": a.map(|v| v.0).unwrap_or(0),
                        "reflected_color": b.map(|v| v.0).unwrap_or(0),
                        "layer": a.map(|v| v.1),
                        "reflected_layer": b.map(|v| v.1),
                    }));
                }
            }
        }
        json!({
            "voxels": cells.len(),
            "empty": cells.is_empty(),
            "axis": args["axis"],
            "plane": plane,
            "compare_color": colors,
            "symmetric": mismatches == 0,
            "mismatched_pairs": mismatches,
            "samples": samples,
            "truncated": mismatches > limit,
        })
    } else {
        let connectivity = match args.get("connectivity") {
            None => 6,
            Some(v) => v
                .as_u64()
                .filter(|v| *v == 6 || *v == 26)
                .ok_or("connectivity must be 6 or 26")?,
        };
        let mut remaining: BTreeSet<_> = cells.keys().copied().collect();
        let mut components = BinaryHeap::new();
        let mut component_count = 0;
        while let Some(start) = remaining.pop_first() {
            let mut queue = VecDeque::from([start]);
            let (mut lo, mut hi, mut count) = (start, start, 0);
            let mut layers = BTreeSet::new();
            while let Some(p) = queue.pop_front() {
                count += 1;
                layers.insert(cells[&p].1);
                for i in 0..3 {
                    lo[i] = lo[i].min(p[i]);
                    hi[i] = hi[i].max(p[i]);
                }
                for x in -1i32..=1 {
                    for y in -1i32..=1 {
                        for z in -1i32..=1 {
                            let distance = x.abs() + y.abs() + z.abs();
                            if distance == 0 || (connectivity == 6 && distance != 1) {
                                continue;
                            }
                            let q = [p[0] + x, p[1] + y, p[2] + z];
                            if remaining.remove(&q) {
                                queue.push_back(q);
                            }
                        }
                    }
                }
            }
            component_count += 1;
            components.push((Reverse(count), lo, hi, layers));
            if components.len() > limit {
                components.pop();
            }
        }
        let mut components: Vec<_> = components
            .into_iter()
            .map(|(Reverse(n), lo, hi, layers)| (n, lo, hi, layers))
            .collect();
        components.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        json!({
            "voxels": cells.len(),
            "connectivity": connectivity,
            "component_count": component_count,
            "truncated": component_count > limit,
            "components": components
                .iter()
                .take(limit)
                .map(|(n, lo, hi, layers)| {
                    json!({"voxels": n, "min": lo, "max": hi, "layers": layers})
                })
                .collect::<Vec<_>>(),
            "note": "Components are observations, not automatically defects; \
                     26-connectivity includes corner-only contact.",
        })
    };
    Ok(CallResult::text(format!("{name}\n{report}")))
}

pub(super) fn compare_saved(
    editor: &Editor,
    root: &Roots,
    args: &Value,
) -> Result<CallResult, String> {
    let path = if args.get("path").is_some() {
        root.resolve(&path_arg(args)?)?
    } else {
        root.resolve(&editor.path().to_string_lossy())?
    };
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("vxm"))
    {
        return Err(
            "compare_saved_model requires a native .vxm file, not a flattened export".into(),
        );
    }
    let saved =
        voxel_core::format::load(&path).map_err(|e| format!("{}: {e}", root.relative(&path)))?;
    // Compact copies, never the editor: allocation high-water marks do not
    // change the document's geometry or hierarchy.
    let canonical = |model: &VoxelModel| {
        let mut m = model.clone();
        for i in 0..m.layer_count() {
            m.trim_layer(i);
        }
        voxel_core::format::native::encode(&m)
    };
    Ok(CallResult::text(format!(
        "saved document comparison\n{}",
        json!({
            "path": root.relative(&path),
            "equal": canonical(editor.model()) == canonical(&saved),
            "current_voxels": editor.model().filled_count(),
            "saved_voxels": saved.filled_count(),
            "unsaved": editor.is_dirty(),
            "comparison": "native document: geometry, palette, layers, objects and active \
                           layer; ignores allocated box slack",
        })
    )))
}
