//! Procedural shapes and validated, ordered multi-layer edits.
//! No mutation happens until every operation has been expanded successfully.

use super::*;
use crate::editor::CellWrite;

const MAX_OPERATIONS: usize = 262_144;
const MAX_WRITES: usize = 2_097_152;

pub(super) fn schemas() -> Vec<ToolInfo> {
    let triple = json!({"type":"array", "items":{"type":"number"}, "minItems":3, "maxItems":3});
    let color = json!({"type":"integer", "minimum":0, "maximum":255, "description":"Palette index; 0 erases. Defaults to selected colour."});
    let layer = json!({"type":["integer","string"], "description":"Layer index or name; defaults to active layer. Does not change selection."});
    let mut ellipsoid = json!({"center":triple, "radii":triple, "color":color, "layer":layer});
    ellipsoid["radii"]["items"] = json!({"type":"number", "exclusiveMinimum":0, "maximum":256});
    let line = json!({"from":triple, "to":triple, "radius":{"type":"number","exclusiveMinimum":0,"maximum":256}, "color":color, "layer":layer});
    vec![
        ToolInfo {
            name: "put_ellipsoid",
            description: "Fill an ellipsoid (equal radii make a sphere). Center and radii may be fractional; integer coordinates denote voxel centers, +Y up. Center must be inside the scene. The surface is clipped at scene edges. Colour 0 erases. One undo step.",
            input_schema: json!({"type":"object","properties":ellipsoid,"required":["center","radii"]}),
        },
        ToolInfo {
            name: "put_line",
            description: "Draw a solid line with rounded ends (a capsule). Fractional endpoints denote voxel centers, +Y up. Endpoints must be inside the scene; thickness is clipped at scene edges. Coincident endpoints make a sphere. Colour 0 erases. One undo step.",
            input_schema: json!({"type":"object","properties":line,"required":["from","to","radius"]}),
        },
        ToolInfo {
            name: "apply_edits",
            description: "Apply ordered voxel, rect, ellipsoid and line operations as ONE undo step, across explicit layers. Validates the entire request before writing. Later operations win on overlaps. Reports write attempts (including repeated cells), not unique cells. Top-level layer/color supply defaults; each operation can override them. Selection stays unchanged. At most 262144 operations and 2097152 candidate cells per call; split larger requests.",
            input_schema: json!({"type":"object","properties":{
                "layer":layer, "color":color,
                "edits":{"type":"array","minItems":1,"maxItems":MAX_OPERATIONS,"items":{
                    "oneOf":[
                        {"type":"object","properties":{"op":{"const":"voxel"},"x":{"type":"integer"},"y":{"type":"integer"},"z":{"type":"integer"},"color":color,"layer":layer},"required":["op","x","y","z"]},
                        {"type":"object","properties":{"op":{"const":"rect"},"from":{"type":"array","items":{"type":"integer"},"minItems":3,"maxItems":3},"to":{"type":"array","items":{"type":"integer"},"minItems":3,"maxItems":3},"color":color,"layer":layer},"required":["op","from","to"]},
                        {"type":"object","properties":{
                            "op":{"const":"ellipsoid"},"center":ellipsoid["center"],"radii":ellipsoid["radii"],"color":color,"layer":layer
                        },"required":["op","center","radii"]},
                        {"type":"object","properties":{
                            "op":{"const":"line"},"from":line["from"],"to":line["to"],"radius":line["radius"],"color":color,"layer":layer
                        },"required":["op","from","to","radius"]}
                    ]
                }}
            },"required":["edits"]}),
        },
        ToolInfo {
            name: "set_palette_color",
            description: "Set the RGB value of palette index 1..255. Recolours every voxel using that index across all layers. One undo step; marks the model unsaved. Does not change the selected colour.",
            input_schema: json!({"type":"object","properties":{
                "index":{"type":"integer","minimum":1,"maximum":255},
                "r":{"type":"integer","minimum":0,"maximum":255},
                "g":{"type":"integer","minimum":0,"maximum":255},
                "b":{"type":"integer","minimum":0,"maximum":255}
            },"required":["index","r","g","b"]}),
        },
    ]
}

pub(super) fn apply(editor: &mut Editor, name: &str, args: &Value) -> Result<CallResult, String> {
    let default_layer = if args.get("layer").is_some() {
        layer_arg(editor, args)?
    } else {
        editor.active_layer()
    };
    let default_color = color_arg(editor, args)?;
    let operations: Vec<(&str, &Value)> = if name == "apply_edits" {
        let edits = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or("`edits` must be an array")?;
        if edits.is_empty() || edits.len() > MAX_OPERATIONS {
            return Err(format!("expected 1..={MAX_OPERATIONS} operations"));
        }
        edits
            .iter()
            .map(|e| {
                Ok((
                    e.get("op")
                        .and_then(Value::as_str)
                        .ok_or("each edit needs `op`")?,
                    e,
                ))
            })
            .collect::<Result<_, String>>()?
    } else {
        vec![(
            if name == "put_ellipsoid" {
                "ellipsoid"
            } else {
                "line"
            },
            args,
        )]
    };
    let mut writes = Vec::new();
    let mut remaining = MAX_WRITES;
    let mut layers = std::collections::BTreeSet::new();
    for (i, (op, params)) in operations.iter().enumerate() {
        let result = (|| {
            let layer = if params.get("layer").is_some() {
                layer_arg(editor, params)?
            } else {
                default_layer
            };
            let color = if params.get("color").is_some() {
                color_arg(editor, params)?
            } else {
                default_color
            };
            let cells = cells(editor.model(), op, params, &mut remaining)?;
            layers.insert(layer);
            writes.extend(cells.into_iter().map(|pos| CellWrite { layer, pos, color }));
            Ok::<_, String>(())
        })();
        result.map_err(|e| format!("edit {i}: {e}"))?;
    }
    let mut report = Report::default();
    editor.apply_writes("mcp batch", writes, |before, after| {
        report.record(before, after)
    });
    Ok(CallResult::text(format!(
        "{}\n{}",
        summary(&report),
        json!({
            "targeted":report.targeted, "added":report.added, "removed":report.removed,
            "repainted":report.repainted, "unchanged":report.unchanged,
            "operations":operations.len(), "layers":layers, "active_layer":editor.active_layer(),
            "model_voxels":editor.model().filled_count()
        })
    )))
}

fn number(v: &Value, label: &str) -> Result<f64, String> {
    v.as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("`{label}` must be a finite number"))
}

fn triple(args: &Value, name: &str) -> Result<[f64; 3], String> {
    let v = args
        .get(name)
        .and_then(Value::as_array)
        .filter(|v| v.len() == 3)
        .ok_or_else(|| format!("`{name}` must have three numbers"))?;
    Ok([
        number(&v[0], name)?,
        number(&v[1], name)?,
        number(&v[2], name)?,
    ])
}

fn center(p: [f64; 3], size: [u16; 3]) -> Result<[f64; 3], String> {
    if (0..3).any(|i| p[i] < 0. || p[i] > f64::from(size[i] - 1)) {
        return Err(format!("center/endpoints {p:?} outside scene {size:?}"));
    }
    Ok(p)
}

fn radius(r: f64) -> Result<f64, String> {
    if r <= 0. || r > f64::from(voxel_core::MAX_DIM) {
        Err("radii must be greater than 0 and at most 256".into())
    } else {
        Ok(r)
    }
}

fn budget(lo: [i32; 3], hi: [i32; 3], remaining: &mut usize) -> Result<(), String> {
    let count = (0..3)
        .map(|i| (hi[i] - lo[i] + 1).max(0) as usize)
        .product();
    *remaining = remaining.checked_sub(count).ok_or_else(|| {
        format!("request exceeds {MAX_WRITES} candidate cells; split into smaller calls")
    })?;
    Ok(())
}

fn cells(
    model: &VoxelModel,
    op: &str,
    args: &Value,
    remaining: &mut usize,
) -> Result<Vec<[i32; 3]>, String> {
    match op {
        "voxel" => {
            let p = point_in(args, model)?;
            budget(p, p, remaining)?;
            Ok(vec![p])
        }
        "rect" => {
            let (lo, hi) = range(args, model)?;
            budget(lo, hi, remaining)?;
            Ok(box_cells(lo, hi).collect())
        }
        "ellipsoid" | "line" => {
            let size = model.size();
            let (a, b, r) = if op == "ellipsoid" {
                let c = center(triple(args, "center")?, size)?;
                let r = triple(args, "radii")?;
                for v in r {
                    radius(v)?;
                }
                (c, c, r)
            } else {
                let a = center(triple(args, "from")?, size)?;
                let b = center(triple(args, "to")?, size)?;
                let r = radius(number(
                    args.get("radius").ok_or("missing `radius`")?,
                    "radius",
                )?)?;
                (a, b, [r; 3])
            };
            let lo = std::array::from_fn(|i| ((a[i].min(b[i]) - r[i]).ceil() as i32).max(0));
            let hi = std::array::from_fn(|i| {
                ((a[i].max(b[i]) + r[i]).floor() as i32).min(i32::from(size[i]) - 1)
            });
            budget(lo, hi, remaining)?;
            let v: [f64; 3] = std::array::from_fn(|i| b[i] - a[i]);
            let len2 = v.iter().map(|v| v * v).sum::<f64>();
            let mut out = Vec::new();
            for z in lo[2]..=hi[2] {
                for y in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        let p = [f64::from(x), f64::from(y), f64::from(z)];
                        let t = if len2 == 0. {
                            0.
                        } else {
                            ((0..3).map(|i| (p[i] - a[i]) * v[i]).sum::<f64>() / len2).clamp(0., 1.)
                        };
                        let d = (0..3)
                            .map(|i| ((p[i] - a[i] - t * v[i]) / r[i]).powi(2))
                            .sum::<f64>();
                        if d <= 1. + 1e-12 {
                            out.push([x, y, z]);
                        }
                    }
                }
            }
            Ok(out)
        }
        _ => Err(format!("unknown edit operation {op:?}")),
    }
}
