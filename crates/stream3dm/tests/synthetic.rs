//! The synthetic house (`samples/synthetic/`, made by `tools/fixtures/generate.mjs`): every
//! value the generator knows is recovered from the file, in both archive versions written.

use serde_json::Value;
use std::path::Path;
#[cfg(feature = "cadmpeg")]
use stream3dm::{InspectOptions, inspect_streaming, inspect_whole};
use stream3dm::{context, inspect};

fn synthetic(name: &str) -> Option<Vec<u8>> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../samples/synthetic")
            .join(name),
    )
    .ok()
}

fn expected() -> Value {
    serde_json::from_slice(&synthetic("house.expected.json").expect("expected values")).unwrap()
}

fn report(name: &str) -> Value {
    serde_json::to_value(inspect(&synthetic(name).expect("house file")).unwrap()).unwrap()
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

fn num(v: &Value) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| panic!("number expected, got {v}"))
}

fn vec3(v: &Value) -> [f64; 3] {
    let a = v.as_array().expect("array");
    [num(&a[0]), num(&a[1]), num(&a[2])]
}

/// Collects mismatches instead of stopping at the first.
struct Check(Vec<String>);

impl Check {
    fn eq(&mut self, what: &str, actual: &Value, expected: &Value) {
        if actual != expected {
            self.0
                .push(format!("{what}: {actual} vs expected {expected}"));
        }
    }

    fn near(&mut self, what: &str, actual: &Value, expected: &Value, tol: f64) {
        let ok = match (actual.as_f64(), expected.as_f64()) {
            (Some(a), Some(b)) => close(a, b, tol),
            _ => actual == expected,
        };
        if !ok {
            self.0
                .push(format!("{what}: {actual} vs expected {expected} (±{tol})"));
        }
    }

    fn near3(&mut self, what: &str, actual: &Value, expected: &Value, tol: f64) {
        let (a, b) = (vec3(actual), vec3(expected));
        if !(0..3).all(|i| close(a[i], b[i], tol)) {
            self.0
                .push(format!("{what}: {a:?} vs expected {b:?} (±{tol})"));
        }
    }
}

fn find<'a>(list: &'a Value, key: &str, value: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|v| v[key] == value)
}

fn check_house(name: &str) -> Vec<String> {
    let e = expected();
    let r = report(name);
    let mut c = Check(Vec::new());

    c.eq("unit system", &r["file"]["unit_system"], &e["unit_system"]);
    for (key, value) in e["inventory"].as_object().unwrap() {
        c.eq(&format!("inventory.{key}"), &r["inventory"][key], value);
    }

    for (path, layer) in e["layers"].as_object().unwrap() {
        let Some(actual) = find(&r["structure"]["layers"], "path", path) else {
            c.0.push(format!("layer {path} missing"));
            continue;
        };
        for key in ["objects", "visible", "material", "color"] {
            c.eq(&format!("layer {path} {key}"), &actual[key], &layer[key]);
        }
        c.near(
            &format!("layer {path} area"),
            &actual["mesh_area_m2"],
            &layer["mesh_area_m2"],
            1e-5,
        );
    }

    for (material, m) in e["materials"].as_object().unwrap() {
        let Some(actual) = find(&r["materials"], "name", material) else {
            c.0.push(format!("material {material} missing"));
            continue;
        };
        c.eq(
            &format!("material {material} colour"),
            &actual["color"],
            &m["color"],
        );
        c.near(
            &format!("material {material} area"),
            &actual["mesh_area_m2"],
            &m["mesh_area_m2"],
            1e-5,
        );
    }

    for (block, b) in e["blocks"].as_object().unwrap() {
        let Some(actual) = find(&r["blocks"], "name", block) else {
            c.0.push(format!("block {block} missing"));
            continue;
        };
        c.eq(
            &format!("block {block} placements"),
            &actual["placements"],
            &b["placements"],
        );
        c.eq(
            &format!("block {block} members"),
            &actual["members"],
            &b["members"],
        );
        c.near3(
            &format!("block {block} size"),
            &actual["size"],
            &b["size"],
            1e-6,
        );
    }

    for (group, n) in e["groups"].as_object().unwrap() {
        match find(&r["structure"]["groups"], "name", group) {
            Some(actual) => c.eq(&format!("group {group}"), &actual["objects"], n),
            None => c.0.push(format!("group {group} missing")),
        }
    }
    c.eq(
        "object names",
        &r["structure"]["object_names"],
        &e["object_names"],
    );
    for (key, u) in e["user_strings"].as_object().unwrap() {
        match find(&r["structure"]["user_strings"], "key", key) {
            Some(actual) => {
                c.eq(
                    &format!("user string {key} objects"),
                    &actual["objects"],
                    &u["objects"],
                );
                c.eq(
                    &format!("user string {key} values"),
                    &actual["values"],
                    &u["values"],
                );
            }
            None => c.0.push(format!("user string {key} missing")),
        }
    }

    for view in e["named_views"].as_array().unwrap() {
        let name = view["name"].as_str().unwrap();
        let Some(actual) = find(&r["views"], "name", name) else {
            c.0.push(format!("view {name} missing"));
            continue;
        };
        c.eq(
            &format!("view {name} kind"),
            &actual["kind"],
            &Value::from("named"),
        );
        c.eq(
            &format!("view {name} projection"),
            &actual["projection"],
            &view["projection"],
        );
        c.near(
            &format!("view {name} lens"),
            &actual["lens_35mm"],
            &view["lens"],
            1e-3,
        );
        for key in ["camera", "target", "direction"] {
            c.near3(
                &format!("view {name} {key}"),
                &actual[key],
                &view[key],
                1e-5,
            );
        }
    }

    let p = &r["proportions"];
    c.near3("bbox min", &p["bbox"]["min"], &e["bbox"]["min"], 1e-6);
    c.near3("bbox max", &p["bbox"]["max"], &e["bbox"]["max"], 1e-6);
    let s = &e["storeys"];
    c.eq("storeys", &p["storeys"]["count"], &s["count"]);
    c.near(
        "storey height",
        &p["storeys"]["typical_height"],
        &s["typical_height"],
        0.05,
    );
    let levels = p["storeys"]["levels"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let expected_levels = s["levels"].as_array().unwrap();
    if levels.len() != expected_levels.len()
        || levels
            .iter()
            .zip(expected_levels)
            .any(|(a, b)| !close(num(a), num(b), 0.05))
    {
        c.0.push(format!("storey levels: {levels:?} vs {expected_levels:?}"));
    }

    for (key, value) in e["style"].as_object().unwrap() {
        c.near(&format!("style {key}"), &r["style"][key], value, 1e-4);
    }

    // Every Brep in the house is flat-faced, so all are tessellated and none is reported.
    if find(&r["diagnostics"], "code", "breps_without_render_mesh").is_some() {
        c.0.push("Breps reported as not displayed".to_string());
    }
    c.0
}

/// D1: known values, archive 80 and archive 70.
#[test]
fn house_known_values() {
    if expected_missing() {
        return;
    }
    for name in ["house.3dm", "house_v7.3dm"] {
        let problems = check_house(name);
        assert!(problems.is_empty(), "{name}:\n{}", problems.join("\n"));
    }
}

/// D2: our block expansion (streaming) agrees with the decoder's own (whole file) on geometry.
#[cfg(feature = "cadmpeg")]
#[test]
fn house_blocks_match_whole_file_expansion() {
    if expected_missing() {
        return;
    }
    let bytes = synthetic("house.3dm").unwrap();
    // The whole-file decode does not tessellate the house's Brep; leave it out of ours too.
    let options = InspectOptions {
        skip_tessellation: true,
        ..Default::default()
    };
    let ours = inspect_streaming(&bytes, options).unwrap();
    let whole = inspect_whole(&bytes).unwrap();
    assert_eq!(
        ours.inventory.mesh_triangles,
        whole.inventory.mesh_triangles
    );
    let (a, b) = (ours.proportions.unwrap(), whole.proportions.unwrap());
    for i in 0..3 {
        assert!(close(a.bbox.min[i], b.bbox.min[i], 1e-6), "bbox min {i}");
        assert!(close(a.bbox.max[i], b.bbox.max[i], 1e-6), "bbox max {i}");
    }
    assert_eq!(
        a.storeys.as_ref().map(|s| s.count),
        b.storeys.as_ref().map(|s| s.count)
    );
    // Areas by orientation; materials are left out because the whole-file expansion loses
    // the members' own materials (D1 checks those against the known values).
    for (what, x, y) in [
        (
            "horizontal",
            ours.style.horizontal_area_share,
            whole.style.horizontal_area_share,
        ),
        (
            "vertical",
            ours.style.vertical_area_share,
            whole.style.vertical_area_share,
        ),
    ] {
        assert!(
            close(x.unwrap(), y.unwrap(), 1e-4),
            "{what} share: {x:?} vs {y:?}"
        );
    }
}

fn expected_missing() -> bool {
    let missing = synthetic("house.expected.json").is_none();
    if missing {
        eprintln!("samples/synthetic not present; skipped");
    }
    missing
}

/// D3: the raw context names every layer, material, block and view exactly as written, and
/// matches its snapshot (`tests/golden/house.context.json`; `UPDATE_GOLDEN=1` accepts changes).
#[test]
fn house_context() {
    if expected_missing() {
        return;
    }
    let report = inspect(&synthetic("house.3dm").unwrap()).unwrap();
    let ctx = serde_json::to_value(context(&report)).unwrap();
    let names = |list: &str, key: &str| -> Vec<String> {
        ctx[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v[key].as_str().unwrap().to_string())
            .collect()
    };
    let e = expected();
    for (path, layer) in e["layers"].as_object().unwrap() {
        if layer["objects"].as_u64().unwrap() > 0 {
            assert!(names("layers", "path").contains(path), "layer {path}");
        }
    }
    for material in e["materials"].as_object().unwrap().keys() {
        assert!(
            names("materials", "name").contains(material),
            "material {material}"
        );
    }
    for block in e["blocks"].as_object().unwrap().keys() {
        assert!(names("blocks", "name").contains(block), "block {block}");
    }
    for view in e["named_views"].as_array().unwrap() {
        let name = view["name"].as_str().unwrap().to_string();
        assert!(names("views", "name").contains(&name), "view {name}");
    }

    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/house.context.json");
    let actual = serde_json::to_string_pretty(&ctx).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden, &actual).unwrap();
    } else {
        let frozen = std::fs::read_to_string(&golden).unwrap_or_default();
        assert!(
            frozen == actual,
            "context differs from tests/golden/house.context.json (UPDATE_GOLDEN=1 to accept)"
        );
    }
}
