//! Runs the extractor over the public samples (`samples/fetch.sh`). Skips when they are absent.
//!
//! Golden reports live in `tests/golden/`; regenerate them with `UPDATE_GOLDEN=1 cargo test`.
//!
//! Without the `cadmpeg` feature every report must be the same as with it.

use std::path::{Path, PathBuf};
use stream3dm::{DiagnosticCode, inspect};

/// Samples whose reports are frozen, together covering every extraction concept.
const GOLDEN: [&str; 11] = [
    "rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh8_NamedViews.3dm",
    "rhino-developer-samples/rhino3dm/models/RenderContent.3dm",
    "rhino3dm/tests/models/blocks.3dm",
    "rhino3dm/tests/models/groups.3dm",
    "rhino3dm/tests/models/object_UserStrings.3dm",
    "rhino3dm/tests/models/subd.3dm",
    "opennurbs/example_files/V4/v4_Wheel_PG.3dm",
    "opennurbs/example_files/V5/v5_disk_brake.3dm",
    "opennurbs/example_files/V7/v7_my_brep.3dm",
    "opennurbs/example_files/V7/v7_rhino_logo_nurbs.3dm",
    "three.js/examples/models/3dm/Rhino_Logo.3dm",
];

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn manifest_paths() -> Vec<String> {
    let manifest = std::fs::read_to_string(samples_dir().join("manifest.tsv"))
        .expect("samples/manifest.tsv is tracked");
    manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split('\t').next().map(str::to_string))
        .collect()
}

#[test]
fn every_sample_yields_a_report() {
    let paths = manifest_paths();
    let mut checked = 0;
    for path in &paths {
        let Ok(bytes) = std::fs::read(samples_dir().join(path)) else {
            continue;
        };
        checked += 1;
        let report = inspect(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::to_string(&report)
            .unwrap_or_else(|e| panic!("{path}: report does not serialize: {e}"));
        assert!(
            report
                .diagnostics
                .iter()
                .all(|d| d.code != DiagnosticCode::UnparsedRecords),
            "{path}: native records failed to parse"
        );
    }
    if checked == 0 {
        eprintln!("skipped: no samples present (run samples/fetch.sh)");
    } else {
        assert_eq!(
            checked,
            paths.len(),
            "some manifest samples are missing; run samples/fetch.sh"
        );
    }
}

#[test]
fn golden_reports() {
    let golden_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures = Vec::new();
    for path in GOLDEN {
        let Ok(bytes) = std::fs::read(samples_dir().join(path)) else {
            eprintln!("skipped {path}: not present (run samples/fetch.sh)");
            continue;
        };
        let report = inspect(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
        let actual = serde_json::to_string_pretty(&report).unwrap() + "\n";
        let file = golden_dir.join(path.rsplit('/').next().unwrap().replace(".3dm", ".json"));
        if update {
            std::fs::create_dir_all(&golden_dir).unwrap();
            std::fs::write(&file, &actual).unwrap();
        } else if std::fs::read_to_string(&file).ok().as_deref() != Some(actual.as_str()) {
            failures.push(file.display().to_string());
        }
    }
    assert!(
        failures.is_empty(),
        "reports differ from golden files (UPDATE_GOLDEN=1 to accept): {failures:#?}"
    );
}

/// Files where only rhino3dm.js showed geometry before (tools/bench/rhino3dm.mjs,
/// 2026-10-15): no unit system, Rhino 1, Rhino 4 blocks, SubD. Triangle counts are the ones
/// rhino3dm.js 8.35 displays; SubD ours subdivides, so only "some" is asserted.
#[test]
fn former_gaps_against_rhino3dm() {
    let expected: &[(&str, Option<usize>)] = &[
        ("opennurbs/example_files/V5/v5_disk_brake.3dm", Some(60_844)),
        ("opennurbs/example_files/V5/v5_my_mesh.3dm", Some(6)),
        (
            "rhino-developer-samples/rhino3dm/models/MeshBox_r5.3dm",
            Some(12),
        ),
        (
            "rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm",
            Some(180),
        ),
        ("opennurbs/example_files/V4/v4_WishBone.3dm", Some(4_946)),
        (
            "opennurbs/example_files/V4/v4_example_file.3dm",
            Some(34_782),
        ),
        ("opennurbs/example_files/V1/v1_Flow3.3dm", Some(11_608)),
        ("rhino3dm/tests/models/subd.3dm", None),
        ("rhino3dm/tests/models/subdBox.3dm", None),
        ("rhino3dm/tests/models/subd_creases.3dm", None),
        ("opennurbs/example_files/V7/v7_rhino_logo_subd.3dm", None),
    ];
    let dir = samples_dir();
    let mut checked = 0;
    for (path, triangles) in expected {
        if !cfg!(feature = "cadmpeg") && (triangles.is_none() || path.contains("/V1/")) {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join(path)) else {
            continue;
        };
        let report = stream3dm::inspect(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
        let shown = report.inventory.mesh_triangles;
        match triangles {
            Some(n) => assert_eq!(shown, *n, "{path}"),
            None => assert!(shown > 0, "{path}: no SubD mesh"),
        }
        checked += 1;
    }
    if checked == 0 {
        eprintln!("samples not fetched; run samples/fetch.sh");
    }
}
