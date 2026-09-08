use super::*;

fn editor() -> Editor {
    Editor::new(VoxelModel::new(16, 12, 10), PathBuf::from("test.vxm"))
}
fn data(r: &CallResult) -> Value {
    let Content::Text { text } = &r.content[0] else {
        panic!("expected text")
    };
    serde_json::from_str(text.lines().last().unwrap()).unwrap_or_else(|err| panic!("{err}: {text}"))
}
fn run(e: &mut Editor, name: &str, args: Value) -> Value {
    let r = call(e, name, &args);
    assert_ne!(r.is_error, Some(true), "{name}: {:?}", r.content);
    data(&r)
}
fn voxel(e: &mut Editor, p: [i32; 3], color: u8) {
    run(
        e,
        "put_voxel",
        json!({"x":p[0],"y":p[1],"z":p[2],"color":color}),
    );
}
fn unchanged(e: &Editor) -> Value {
    json!({
        "model": voxel_core::format::native::encode(e.model()),
        "selection": selection_json(e),
        "undo": e.undo_depth(),
        "redo": e.redo_depth(),
        "dirty": e.is_dirty(),
        "id": e.document_id(),
        "camera": format!("{:?}", e.camera),
        "slice": e.slice,
        "grid": e.show_grid,
        "color": e.color,
    })
}

#[test]
fn symmetry_reports_pairs_colours_planes_and_missing_sides() {
    let mut e = editor();
    voxel(&mut e, [2, 3, 4], 7);
    voxel(&mut e, [13, 3, 4], 8);
    let j = run(&mut e, "check_symmetry", json!({"axis":"x"}));
    assert_eq!(j["mismatched_pairs"], 1);
    assert_eq!(j["samples"][0]["layer"], 0);
    assert_eq!(
        run(
            &mut e,
            "check_symmetry",
            json!({"axis":"x","compare_color":false})
        )["symmetric"],
        true
    );
    voxel(&mut e, [13, 3, 4], 7);
    assert_eq!(
        run(&mut e, "check_symmetry", json!({"axis":"x"}))["symmetric"],
        true
    );
    let j = run(
        &mut e,
        "check_symmetry",
        json!({"axis":"x","plane":0,"limit":1}),
    );
    assert_eq!(j["mismatched_pairs"], 2);
    assert_eq!(j["samples"].as_array().unwrap().len(), 1);
    assert_eq!(j["truncated"], true);
    assert_eq!(j["samples"][0]["position"][0], -13);
    assert_eq!(
        call(&mut e, "check_symmetry", &json!({"axis":"x","plane":2.25})).is_error,
        Some(true)
    );
}

#[test]
fn checks_distinguish_face_from_corner_contact_and_bound_the_report() {
    let mut e = editor();
    voxel(&mut e, [1, 1, 1], 2);
    voxel(&mut e, [2, 2, 2], 3);
    voxel(&mut e, [3, 2, 2], 4);
    let j = run(&mut e, "check_components", json!({"limit":1}));
    assert_eq!(j["component_count"], 2);
    assert_eq!(j["components"][0]["voxels"], 2);
    assert_eq!(j["truncated"], true);
    assert_eq!(
        run(&mut e, "check_components", json!({"connectivity":26}))["component_count"],
        1
    );
    assert_eq!(
        call(&mut e, "check_components", &json!({"connectivity":18})).is_error,
        Some(true)
    );
}

#[test]
fn inspection_scopes_respect_layers_objects_visibility_and_selection() {
    let mut e = editor();
    voxel(&mut e, [1, 1, 1], 2);
    run(&mut e, "create_object", json!({"name":"GROUP"}));
    run(
        &mut e,
        "create_object",
        json!({"name":"PART","parent":"GROUP"}),
    );
    run(&mut e, "add_layer", json!({"name":"TOP"}));
    run(
        &mut e,
        "set_layer_object",
        json!({"layer":"TOP","object":"PART"}),
    );
    voxel(&mut e, [1, 1, 1], 3);
    voxel(&mut e, [14, 1, 1], 3);
    assert_eq!(
        run(
            &mut e,
            "check_symmetry",
            json!({"axis":"x","object":"GROUP"})
        )["symmetric"],
        true
    );
    run(&mut e, "select_box", json!({"from":[1,1,1],"to":[1,1,1]}));
    let before = unchanged(&e);
    assert_eq!(
        run(&mut e, "check_components", json!({"selection":true}))["voxels"],
        1
    );
    assert_eq!(unchanged(&e), before);
    run(
        &mut e,
        "set_object_visible",
        json!({"object":"GROUP","visible":false}),
    );
    assert_eq!(
        run(&mut e, "check_components", json!({"object":"GROUP"}))["voxels"],
        0
    );
    assert_eq!(
        run(
            &mut e,
            "check_components",
            json!({"object":"GROUP","include_hidden":true})
        )["voxels"],
        2
    );
    assert_eq!(run(&mut e, "check_components", json!({}))["voxels"], 1);
    assert_eq!(
        run(
            &mut e,
            "check_components",
            json!({"include_hidden":true,"from":[10,0,0],"to":[15,5,5]})
        )["voxels"],
        1
    );
    for args in [
        json!({"object":"GROUP","layer":0}),
        json!({"from":[0,0,0]}),
        json!({"limit":0}),
        json!({"include_hidden":"yes"}),
    ] {
        assert_eq!(call(&mut e, "check_components", &args).is_error, Some(true));
    }
}

#[test]
fn previews_focus_on_parts_and_never_mutate_the_live_editor() {
    let mut e = editor();
    voxel(&mut e, [1, 2, 3], 1);
    run(&mut e, "add_layer", json!({"name":"PART"}));
    voxel(&mut e, [10, 7, 4], 6);
    run(&mut e, "select_box", json!({"from":[10,7,4],"to":[10,7,4]}));
    run(&mut e, "copy_selection", json!({}));
    e.slice = Some(3);
    e.camera.yaw = 0.8;
    let before = unchanged(&e);
    for name in ["preview_model", "screenshot_views"] {
        let r = call(
            &mut e,
            name,
            &json!({"selection":true,"width":64,"height":80}),
        );
        assert_ne!(r.is_error, Some(true), "{:?}", r.content);
        assert_eq!(data(&r)["target_voxels"], 1);
        assert_eq!(data(&r)["target_bounds"]["min"], json!([10, 7, 4]));
        assert!(matches!(&r.content[1],Content::Image{mime_type,..} if mime_type=="image/png"));
        assert_eq!(unchanged(&e), before);
        assert!(e.clipboard.is_some());
    }
    for args in [
        json!({"width":1}),
        json!({"views":[]}),
        json!({"views":["unknown"]}),
        json!({"selection":true,"layer":1}),
        json!({"path":"denied.png"}),
        json!({"isolate":false,"include_hidden":true}),
    ] {
        assert_eq!(call(&mut e, "screenshot_views", &args).is_error, Some(true));
        assert_eq!(unchanged(&e), before);
    }
    let j = run(
        &mut e,
        "screenshot_views",
        json!({"views":["front","back"],"width":64,"height":64}),
    );
    assert_eq!(j["width"], 128);
    assert_eq!(j["height"], 82);
    assert_eq!(j["views"][1]["x"], 64);
    assert_eq!(j["views"][1]["view"], "back");
}

#[test]
fn empty_checks_are_explicit_and_empty_previews_are_refused() {
    let mut e = editor();
    assert_eq!(
        run(&mut e, "check_symmetry", json!({"axis":"x"}))["empty"],
        true
    );
    assert_eq!(
        run(&mut e, "check_components", json!({}))["component_count"],
        0
    );
    assert_eq!(
        call(&mut e, "preview_model", &json!({})).is_error,
        Some(true)
    );
    assert_eq!(
        call(&mut e, "check_components", &json!({"selection":true})).is_error,
        Some(true)
    );
}

#[test]
fn saved_comparison_preserves_history_and_detects_hidden_geometry_and_palette() {
    let dir = std::env::temp_dir().join(format!(
        "voxeler-check-saved-{}-{}",
        std::process::id(),
        editor().document_id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let root = Roots::new([dir.clone()]);
    let mut e = editor();
    e.open(VoxelModel::new(16, 12, 10), dir.join("test.vxm"));
    voxel(&mut e, [1, 1, 1], 7);
    assert_ne!(
        call_in(&mut e, &root, "save_model", &json!({})).is_error,
        Some(true)
    );
    let before = unchanged(&e);
    assert_eq!(
        data(&call_in(&mut e, &root, "compare_saved_model", &json!({})))["equal"],
        true
    );
    assert_eq!(unchanged(&e), before);
    voxel(&mut e, [15, 11, 9], 2);
    voxel(&mut e, [15, 11, 9], 0);
    let slack = unchanged(&e);
    assert_eq!(
        data(&call_in(&mut e, &root, "compare_saved_model", &json!({})))["equal"],
        true
    );
    assert_eq!(unchanged(&e), slack);
    let png = dir.join("preview.png");
    let r = call_in(
        &mut e,
        &root,
        "preview_model",
        &json!({"width":64,"height":64,"path":png}),
    );
    assert_ne!(r.is_error, Some(true));
    let Content::Image { data: encoded, .. } = &r.content[1] else {
        panic!("missing image")
    };
    assert_eq!(*encoded, base64(&std::fs::read(&png).unwrap()));
    assert_eq!(unchanged(&e), slack);
    run(&mut e, "add_layer", json!({"name":"HIDDEN"}));
    voxel(&mut e, [8, 8, 8], 7);
    run(
        &mut e,
        "set_layer_visible",
        json!({"layer":"HIDDEN","visible":false}),
    );
    assert_eq!(
        data(&call_in(&mut e, &root, "compare_saved_model", &json!({})))["equal"],
        false
    );
    call_in(&mut e, &root, "save_model", &json!({}));
    run(
        &mut e,
        "set_palette_color",
        json!({"index":7,"r":1,"g":2,"b":3}),
    );
    assert_eq!(
        data(&call_in(&mut e, &root, "compare_saved_model", &json!({})))["equal"],
        false
    );
    for args in [
        json!({"path":"../outside.vxm"}),
        json!({"path":"flat.vox"}),
        json!({"path":"missing.vxm"}),
    ] {
        assert_eq!(
            call_in(&mut e, &root, "compare_saved_model", &args).is_error,
            Some(true)
        );
    }
    assert_eq!(
        call(&mut e, "compare_saved_model", &json!({})).is_error,
        Some(true)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn prism_is_inclusive_symmetric_and_winding_independent() {
    let mut e = editor();
    let shape = json!({"axis":"z","vertices":[[2,2],[13,2],[7.5,10]],"start":3,"end":5,"color":8});
    run(&mut e, "put_prism", shape.clone());
    assert_eq!(e.model().get(2, 2, 3), 8);
    assert_eq!(e.model().get(13, 2, 5), 8);
    assert_eq!(e.model().get(2, 2, 6), 0);
    assert_eq!(
        run(&mut e, "check_symmetry", json!({"axis":"x"}))["symmetric"],
        true
    );
    let before: Vec<_> = e.model().iter_filled().collect();
    e.undo();
    let mut reverse = shape;
    reverse["vertices"].as_array_mut().unwrap().reverse();
    reverse["start"] = json!(5);
    reverse["end"] = json!(3);
    run(&mut e, "put_prism", reverse);
    assert_eq!(e.model().iter_filled().collect::<Vec<_>>(), before);
}

#[test]
fn prism_axes_concavity_and_batch_undo() {
    let mut e = editor();
    let points = json!([[1, 1], [5, 1], [5, 2], [2, 2], [2, 5], [1, 5]]);
    for axis in ["x", "y", "z"] {
        let before = e.undo_depth();
        run(
            &mut e,
            "apply_edits",
            json!({"color": 4, "edits": [
                {"op": "prism", "axis": axis, "vertices": points, "start": 6, "end": 7}
            ]}),
        );
        assert_eq!(e.undo_depth(), before + 1);
        let (a, u, v) = match axis {
            "x" => (0, 1, 2),
            "y" => (1, 0, 2),
            _ => (2, 0, 1),
        };
        let mut p = [0; 3];
        p[a] = 6;
        p[u] = 1;
        p[v] = 4;
        assert_eq!(e.model().get(p[0], p[1], p[2]), 4);
        p[u] = 4;
        assert_eq!(e.model().get(p[0], p[1], p[2]), 0, "concave notch");
        e.undo();
        assert_eq!(e.model().filled_count(), 0);
    }
}

#[test]
fn invalid_prisms_do_not_apply_earlier_batch_operations() {
    let mut e = editor();
    voxel(&mut e, [0, 0, 0], 1);
    e.undo();
    let before = unchanged(&e);
    for vertices in [
        json!([[1, 1], [5, 5], [1, 5], [5, 1]]),
        json!([[1, 1], [2, 2], [3, 3]]),
        json!([[1, 1], [2, 2], [1, 1]]),
        json!([[0, 0], [99, 1], [1, 2]]),
    ] {
        let r = call(
            &mut e,
            "apply_edits",
            &json!({"edits": [
                {"op": "voxel", "x": 3, "y": 3, "z": 3},
                {"op": "prism", "axis": "z", "vertices": vertices, "start": 1, "end": 2}
            ]}),
        );
        assert_eq!(r.is_error, Some(true));
        assert_eq!(unchanged(&e), before);
    }
}

#[test]
fn document_identity_survives_edits_but_not_replacement() {
    let mut e = editor();
    let id = e.document_id();
    let session = session_identity();
    voxel(&mut e, [1, 1, 1], 3);
    run(&mut e, "select_box", json!({"from":[1,1,1],"to":[1,1,1]}));
    run(&mut e, "copy_selection", json!({}));
    assert_eq!(e.document_id(), id);
    e.open(VoxelModel::new(8, 8, 8), PathBuf::from("another.vxm"));
    assert_ne!(e.document_id(), id);
    assert!(e.selection.is_none());
    assert!(e.clipboard.is_none());
    assert_eq!(session_identity(), session);
}
