//! Focused and multi-view inspection without touching the live editor.
use super::*;
use voxel_render::{Framebuffer, Vec3};

const VIEWS: [&str; 6] = ["front", "right", "back", "left", "top", "three_quarter"];

pub(super) fn schemas() -> Vec<ToolInfo> {
    let mut properties = checking::scope_schema();
    properties["isolate"] = json!({
        "type": "boolean",
        "default": true,
        "description": "Show only the inspected cells; false keeps visible surrounding \
         geometry and cannot combine with include_hidden=true.",
    });
    properties["width"] =
        json!({"type": "integer", "minimum": 64, "maximum": 1024, "default": 512});
    properties["height"] = properties["width"].clone();
    properties["view"] = json!({"type": "string", "enum": VIEWS, "default": "three_quarter"});
    properties["ambient"] = json!({"type": "number", "minimum": 0, "maximum": 1, "default": 0.7});
    properties["diffuse"] = json!({"type": "number", "minimum": 0, "maximum": 1, "default": 0.3});
    properties["path"] =
        json!({"type": "string", "description": "Optional PNG path within server roots."});
    let mut multi = properties.clone();
    multi.as_object_mut().unwrap().remove("view");
    multi["views"] = json!({
        "type": "array",
        "minItems": 1,
        "maxItems": 6,
        "items": {"type": "string", "enum": VIEWS},
        "description": "Order in the labelled sheet; default \
         front/right/back/left/top/three_quarter.",
    });
    multi["width"]["maximum"] = json!(512);
    multi["height"]["maximum"] = json!(512);
    vec![
        ToolInfo {
            name: "preview_model",
            description:
                "Read-only close-up of a layer, object subtree, selection or from/to region. \
                 Same scopes as check_symmetry. Isolates the target by default; isolate=false \
                 retains context. Uses a temporary editor, leaving camera, slice, visibility, \
                 selection, undo and save state untouched. Empty targets are refused. All \
                 previews are perspective, not orthographic.",
            input_schema: json!({"type": "object", "properties": properties}),
        },
        ToolInfo {
            name: "screenshot_views",
            description:
                "Read-only labelled contact sheet of up to six model views at a shared framing \
                 and lighting. Same target scopes and isolation as preview_model. width/height \
                 are per tile (64..512); the sheet has up to three columns. Returns view labels \
                 and tile rectangles. Does not alter the working editor or save its model. All \
                 views are perspective; an occluded feature is not necessarily missing.",
            input_schema: json!({"type": "object", "properties": multi}),
        },
    ]
}

fn angle(name: &str) -> Result<(f32, f32), String> {
    Ok(match name {
        "front" => (0., 0.),
        "back" => (180., 0.),
        "left" => (-90., 0.),
        "right" => (90., 0.),
        "top" => (0., 89.),
        "three_quarter" => (45., 30.),
        _ => return Err(format!("unknown view {name:?}")),
    })
}

pub(super) fn render(
    editor: &Editor,
    root: &Roots,
    args: &Value,
    multi: bool,
) -> Result<CallResult, String> {
    let dimension = |key: &str| -> Result<u32, String> {
        let max = if multi { 512 } else { 1024 };
        match args.get(key) {
            None => Ok(512),
            Some(v) => v
                .as_u64()
                .filter(|n| (64..=max).contains(n))
                .map(|n| n as u32)
                .ok_or_else(|| format!("{key} must be 64..={max}")),
        }
    };
    let (w, h) = (dimension("width")?, dimension("height")?);
    let names: Vec<&str> = if multi {
        if args.get("view").is_some() {
            return Err("use views, not view, for a contact sheet".into());
        }
        match args.get("views") {
            None => VIEWS.to_vec(),
            Some(v) => v
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= 6)
                .ok_or("views must contain 1..=6 presets")?
                .iter()
                .map(|v| v.as_str().ok_or("view must be a string"))
                .collect::<Result<_, _>>()?,
        }
    } else {
        vec![match args.get("view") {
            None => "three_quarter",
            Some(v) => v.as_str().ok_or("view must be a string")?,
        }]
    };
    let angles = names
        .iter()
        .map(|n| angle(n))
        .collect::<Result<Vec<_>, _>>()?;
    let intensity = |key: &str, default: f32| -> Result<f32, String> {
        match args.get(key) {
            None => Ok(default),
            Some(v) => v
                .as_f64()
                .filter(|v| v.is_finite() && (0. ..=1.).contains(v))
                .map(|v| v as f32)
                .ok_or_else(|| format!("{key} must be 0..=1")),
        }
    };
    let mut options = crate::view::RenderOptions::default();
    options.light.ambient = intensity("ambient", 0.7)?;
    options.light.diffuse = intensity("diffuse", 0.3)?;
    let path = if args.get("path").is_some() {
        let p = root.resolve(&path_arg(args)?)?;
        if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
            return Err("preview output must end in .png".into());
        }
        Some(p)
    } else {
        None
    };
    let isolate = bool_arg(args, "isolate", true)?;
    if !isolate && bool_arg(args, "include_hidden", false)? {
        return Err(
            "include_hidden requires isolate=true; context retains the document's visibility"
                .into(),
        );
    }
    let cells = checking::collect(editor, args)?;
    let (lo, hi) = checking::bounds(&cells)
        .ok_or("the preview target contains no visible voxels; check scope or include_hidden")?;
    let model = if isolate {
        let s = editor.model().size();
        let mut m = VoxelModel::new(s[0], s[1], s[2]);
        m.set_palette(editor.model().palette().clone());
        // The layer is new and empty, so a box inside the scene cannot drop a
        // voxel and the refusal cannot fire. Declaring it up front is still
        // worth it: the writes below then cost one allocation rather than a
        // copy of the box per row they grow it by.
        let bounds = Bounds::new(
            lo.map(|v| v as u16),
            std::array::from_fn(|i| (hi[i] - lo[i] + 1) as u16),
        );
        if !m.set_layer_bounds(0, bounds) {
            return Err("the preview target does not fit the scene".into());
        }
        for (p, (c, _)) in &cells {
            m.set_in(0, p[0], p[1], p[2], *c);
        }
        m
    } else {
        editor.model().clone()
    };
    let mut preview = Editor::new(model, editor.path().to_path_buf());
    preview.show_grid = false;
    let offset = preview.offset();
    let min = Vec3 {
        x: lo[0] as f32,
        y: lo[1] as f32,
        z: lo[2] as f32,
    } + offset;
    let max = Vec3 {
        x: hi[0] as f32 + 1.,
        y: hi[1] as f32 + 1.,
        z: hi[2] as f32 + 1.,
    } + offset;
    preview.camera.frame(min, max);
    // OrbitCamera::frame fits a square viewport. Keep the same scale in all
    // tiles and account for portrait tiles as well.
    preview.camera.distance *= (h as f32 / w as f32).max(1.);
    let columns = if multi { names.len().min(3) as u32 } else { 1 };
    let rows = (names.len() as u32).div_ceil(columns);
    let label = if multi { 18 } else { 0 };
    let mut sheet = Framebuffer::new(columns * w, rows * (h + label));
    sheet.clear(0x141A24);
    let mut layout = Vec::new();
    for (i, (name, (yaw, pitch))) in names.iter().zip(angles).enumerate() {
        preview.camera.yaw = yaw.to_radians();
        preview.camera.pitch = pitch.to_radians().clamp(-1.55, 1.55);
        let mut tile = Framebuffer::new(w, h);
        crate::view::render_with_options(&mut tile, &mut preview, None, options);
        let x = (i as u32 % columns) * w;
        let y = (i as u32 / columns) * (h + label);
        for ty in 0..h {
            for tx in 0..w {
                sheet.set(x + tx, y + label + ty, tile.color_at(tx, ty));
            }
        }
        if multi {
            voxel_render::overlay::text(&mut sheet, x as i32 + 4, y as i32 + 4, name, 0xffffff, 1);
        }
        layout.push(json!({"view":name,"x":x,"y":y+label,"width":w,"height":h}));
    }
    let png = voxel_render::png::encode(sheet.width(), sheet.height(), sheet.color());
    if let Some(p) = &path {
        std::fs::write(p, &png).map_err(|e| format!("cannot write {}: {e}", root.relative(p)))?;
    }
    Ok(CallResult {
        content: vec![
            Content::Text {
                text: format!(
                    "model preview\n{}",
                    json!({
                        "width": sheet.width(),
                        "height": sheet.height(),
                        "views": layout,
                        "target_bounds": {"min": lo, "max": hi},
                        "target_voxels": cells.len(),
                        "isolate": isolate,
                        "projection": "perspective",
                        "path": path.map(|p| root.relative(&p)),
                    })
                ),
            },
            Content::Image {
                data: base64(&png),
                mime_type: "image/png".into(),
            },
        ],
        is_error: None,
    })
}
