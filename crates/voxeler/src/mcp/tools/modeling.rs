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
    let taper_radius = json!({"type":"number","minimum":0,"maximum":256});
    let tapered_line = json!({"from":triple, "to":triple,
        "radius_from":taper_radius, "radius_to":taper_radius, "color":color, "layer":layer});
    let prism = json!({"axis":{"type":"string","enum":["x","y","z"]},
        "vertices":{"type":"array","minItems":3,"maxItems":64,"items":{"type":"array","items":{"type":"number"},"minItems":2,"maxItems":2}},
        "start":{"type":"integer"},"end":{"type":"integer"},"color":color,"layer":layer});
    vec![
        ToolInfo {
            name:"put_prism",
            description:"Extrude a simple polygon into solid armour. axis names the extrusion direction; vertices are [y,z] for x, [x,z] for y, [x,y] for z. start/end are inclusive integer coordinates. 3..64 vertices, either winding, concave allowed; no repeated vertices, self-intersections or holes. Integer voxel centers on polygon edges are included, preserving reflected boundaries. All coordinates must be inside the scene. Colour 0 erases; explicit layer supported; one undo step.",
            input_schema:json!({"type":"object","properties":prism,"required":["axis","vertices","start","end"]}),
        },
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
            name: "put_tapered_line",
            description: "Draw a cone or tapered branch along from -> to, with radius interpolated linearly from radius_from to radius_to. End faces are perpendicular to the branch, not world-up; ends are flat, not rounded. A zero radius makes a pointed tip; equal radii make a cylinder. Radii must be 0..256, not both zero. Endpoints must be distinct and inside the scene; fractional coordinates denote voxel centers, +Y up. Thickness clips at scene edges. Colour 0 erases. One undo step.",
            input_schema: json!({"type":"object","properties":tapered_line,"required":["from","to","radius_from","radius_to"]}),
        },
        ToolInfo {
            name: "apply_edits",
            description: "Apply ordered voxel, rect, ellipsoid, line, tapered_line and prism operations as ONE undo step, across explicit layers. tapered_line takes from/to and radius_from/radius_to (0 makes a pointed tip), with flat ends perpendicular to its axis. prism takes axis, vertices, start/end as put_prism. Validates the entire request before writing. Later operations win on overlaps. Reports write attempts (including repeated cells), not unique cells. Top-level layer/color supply defaults; each operation can override them. Selection stays unchanged. At most 262144 operations and 2097152 candidate cells per call; split larger requests.",
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
                        },"required":["op","from","to","radius"]},
                        {"type":"object","properties":{
                            "op":{"const":"tapered_line"},"from":triple,"to":triple,
                            "radius_from":taper_radius,"radius_to":taper_radius,"color":color,"layer":layer
                        },"required":["op","from","to","radius_from","radius_to"]},
                        {"type":"object","properties":{"op":{"const":"prism"},"axis":prism["axis"],"vertices":prism["vertices"],"start":prism["start"],"end":prism["end"],"color":color,"layer":layer},"required":["op","axis","vertices","start","end"]}
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
        writable_layer_arg(editor, args)?
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
            match name {
                "put_ellipsoid" => "ellipsoid",
                "put_line" => "line",
                "put_tapered_line" => "tapered_line",
                "put_prism" => "prism",
                _ => return Err(format!("unknown modeling tool {name:?}")),
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
                writable_layer_arg(editor, params)?
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
        "tapered_line" => tapered_line_cells(model, args, remaining),
        "prism" => prism_cells(model, args, remaining),
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

fn tapered_line_cells(
    model: &VoxelModel,
    args: &Value,
    remaining: &mut usize,
) -> Result<Vec<[i32; 3]>, String> {
    let size = model.size();
    let a = center(triple(args, "from")?, size)?;
    let b = center(triple(args, "to")?, size)?;
    let read_radius = |name: &str| -> Result<f64, String> {
        let r = number(
            args.get(name).ok_or_else(|| format!("missing `{name}`"))?,
            name,
        )?;
        if !(0. ..=f64::from(voxel_core::MAX_DIM)).contains(&r) {
            return Err(format!("`{name}` must be between 0 and 256"));
        }
        Ok(r)
    };
    let ra = read_radius("radius_from")?;
    let rb = read_radius("radius_to")?;
    if ra == 0. && rb == 0. {
        return Err("at least one endpoint radius must be greater than 0".into());
    }
    let v: [f64; 3] = std::array::from_fn(|i| b[i] - a[i]);
    let len2 = v.iter().map(|v| v * v).sum::<f64>();
    if len2 == 0. {
        return Err(
            "tapered_line endpoints must be distinct; use put_ellipsoid for a sphere".into(),
        );
    }
    // A conservative box around the two end discs. Charge candidates before
    // walking it, just like every other procedural shape.
    let lo = std::array::from_fn(|i| ((a[i] - ra).min(b[i] - rb).ceil() as i32).max(0));
    let hi = std::array::from_fn(|i| {
        ((a[i] + ra).max(b[i] + rb).floor() as i32).min(i32::from(size[i]) - 1)
    });
    budget(lo, hi, remaining)?;
    let mut out = Vec::new();
    for p in box_cells(lo, hi) {
        let d: [f64; 3] = std::array::from_fn(|i| f64::from(p[i]) - a[i]);
        let t = (0..3).map(|i| d[i] * v[i]).sum::<f64>() / len2;
        // Do not clamp into a capsule: the end planes must follow the branch
        // axis too. The small tolerance keeps reversed fractional ends equal.
        if !(-1e-12..=1. + 1e-12).contains(&t) {
            continue;
        }
        let t = t.clamp(0., 1.);
        let r = ra * (1. - t) + rb * t;
        let distance2 = (0..3).map(|i| (d[i] - t * v[i]).powi(2)).sum::<f64>();
        if distance2 <= r * r + 1e-12 {
            out.push(p);
        }
    }
    Ok(out)
}

fn prism_cells(
    model: &VoxelModel,
    args: &Value,
    remaining: &mut usize,
) -> Result<Vec<[i32; 3]>, String> {
    let (axis, u, v) = match args.get("axis").and_then(Value::as_str) {
        Some("x") => (0, 1, 2),
        Some("y") => (1, 0, 2),
        Some("z") => (2, 0, 1),
        _ => return Err("prism axis must be x, y or z".into()),
    };
    let size = model.size();
    let depth = |key: &str| -> Result<i32, String> {
        args.get(key)
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0 && *n < i64::from(size[axis]))
            .map(|n| n as i32)
            .ok_or_else(|| format!("{key} must be an integer inside the scene"))
    };
    let (start, end) = (depth("start")?, depth("end")?);
    let points = args
        .get("vertices")
        .and_then(Value::as_array)
        .filter(|p| (3..=64).contains(&p.len()))
        .ok_or("vertices must contain 3..=64 points")?;
    let points: Vec<[f64; 2]> = points
        .iter()
        .map(|p| {
            let a = p
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or("each vertex must have two numbers")?;
            let p = [number(&a[0], "vertex")?, number(&a[1], "vertex")?];
            if p[0] < 0.
                || p[0] > f64::from(size[u] - 1)
                || p[1] < 0.
                || p[1] > f64::from(size[v] - 1)
            {
                return Err("polygon vertex outside scene".into());
            }
            Ok(p)
        })
        .collect::<Result<_, String>>()?;
    let cross = |a: [f64; 2], b: [f64; 2], p: [f64; 2]| {
        (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
    };
    let on = |a: [f64; 2], b: [f64; 2], p: [f64; 2]| {
        cross(a, b, p).abs() <= 1e-9
            && (0..2).all(|i| p[i] >= a[i].min(b[i]) - 1e-9 && p[i] <= a[i].max(b[i]) + 1e-9)
    };
    let n = points.len();
    for i in 0..n {
        for j in i + 1..n {
            if points[i] == points[j] {
                return Err("repeated polygon vertex; do not repeat the closing point".into());
            }
            if j == i + 1 || (i == 0 && j == n - 1) {
                continue;
            }
            let (a, b, c, d) = (
                points[i],
                points[(i + 1) % n],
                points[j],
                points[(j + 1) % n],
            );
            if on(a, b, c)
                || on(a, b, d)
                || on(c, d, a)
                || on(c, d, b)
                || (cross(a, b, c) * cross(a, b, d) < 0. && cross(c, d, a) * cross(c, d, b) < 0.)
            {
                return Err("polygon edges intersect".into());
            }
        }
        // Adjacent collinear edges may continue straight, but may not double
        // back over each other.
        let (a, b, c) = (points[(i + n - 1) % n], points[i], points[(i + 1) % n]);
        if cross(a, b, c).abs() <= 1e-9
            && ((a[0] - b[0]) * (c[0] - b[0]) + (a[1] - b[1]) * (c[1] - b[1])) > 0.
        {
            return Err("polygon edges overlap".into());
        }
    }
    let area = (0..n)
        .map(|i| points[i][0] * points[(i + 1) % n][1] - points[(i + 1) % n][0] * points[i][1])
        .sum::<f64>();
    if area.abs() <= 1e-9 {
        return Err("polygon has zero area".into());
    }
    let mut lo = [0; 3];
    let mut hi = [0; 3];
    lo[axis] = start.min(end);
    hi[axis] = start.max(end);
    for (dim, coord) in [(u, 0), (v, 1)] {
        lo[dim] = points
            .iter()
            .map(|p| p[coord])
            .fold(f64::INFINITY, f64::min)
            .ceil() as i32;
        hi[dim] = points
            .iter()
            .map(|p| p[coord])
            .fold(f64::NEG_INFINITY, f64::max)
            .floor() as i32;
    }
    budget(lo, hi, remaining)?;
    let mut out = Vec::new();
    for a in lo[u]..=hi[u] {
        for b in lo[v]..=hi[v] {
            let p = [f64::from(a), f64::from(b)];
            let mut inside = false;
            for i in 0..n {
                let (q, r) = (points[i], points[(i + 1) % n]);
                if on(q, r, p) {
                    inside = true;
                    break;
                }
                if (q[1] > p[1]) != (r[1] > p[1])
                    && p[0] < (r[0] - q[0]) * (p[1] - q[1]) / (r[1] - q[1]) + q[0]
                {
                    inside = !inside;
                }
            }
            if inside {
                for d in lo[axis]..=hi[axis] {
                    let mut p = [0; 3];
                    p[axis] = d;
                    p[u] = a;
                    p[v] = b;
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}
