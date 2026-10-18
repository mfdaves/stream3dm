//! Rhino 1 files (archive 1) are read by our own reader: their trimmed surfaces tessellated,
//! curves, points and meshes drawn, layers with their visibility. The openNURBS examples hold
//! each Rhino 1 model as Rhino 2 converted it (V2); what we draw of the Rhino 1 file must cover
//! the same surface, facing the same way (signed volume), on the same layers. Known exceptions:
//! in T-Joint1 one named face sits on the layer "Hidden" in the Rhino 1 file and on "Default"
//! in its conversion (layers compared by name and visibility only); v1_SphericalSpiral keeps
//! Rhino's mesh of a face that folds against its surface on any grid, which its conversion
//! cannot draw (area not compared); two faces of v1_Soccer, on hidden layers, fold in their
//! Rhino 1 form and are not drawn.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::{DocumentInfo, Finish, InspectOptions, Inspector, MeshSink, PlacedMesh, Report};

#[derive(Default)]
struct Measure {
    area: f64,
    volume: f64,
}

impl MeshSink for Measure {
    fn document(&mut self, _: &DocumentInfo) {}
    fn mesh(&mut self, m: &PlacedMesh) {
        for t in m.triangles {
            let [a, b, c] = t.map(|i| m.positions[i as usize]);
            let cross = |u: [f64; 3], v: [f64; 3]| {
                [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ]
            };
            let bc = cross(b, c);
            self.volume += (a[0] * bc[0] + a[1] * bc[1] + a[2] * bc[2]) / 6.0;
            let n = cross(
                [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
                [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
            );
            self.area += (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() / 2.0;
        }
    }
}

fn measure(path: &str) -> Option<(Measure, Report)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/opennurbs/example_files")
        .join(path);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipped: {} not fetched (samples/fetch.sh)", path.display());
        return None;
    };
    let sink = Rc::new(RefCell::new(Measure::default()));
    let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
    let report = loop {
        inspector.push(&bytes).unwrap();
        if let Finish::Report(r) = inspector.finish().unwrap() {
            break *r;
        }
    };
    drop(inspector);
    Some((Rc::try_unwrap(sink).ok().unwrap().into_inner(), report))
}

#[test]
fn rhino1_models_match_their_rhino2_conversion() {
    let models = [
        "Camera",
        "DeformCubes",
        "DeformSpheres",
        "Flow2",
        "HumanHead2",
        "Lightbulb",
        "MatchSrf",
        "Patch",
        "RhinoLogo",
        "SketchOnMesh",
        "Smooth",
        "Soccer",
        "Speeder",
        "SphericalSpiral",
        "Stearman",
        "T-Joint1",
        "T-Joint2",
        "Terrain",
        "UntrimmedSurfaces",
    ];
    let mut compared = 0;
    for m in models {
        let (Some((old, old_report)), Some((new, new_report))) = (
            measure(&format!("V1/v1_{m}.3dm")),
            measure(&format!("V2/v2_{m}.3dm")),
        ) else {
            continue;
        };
        let ratio = old.area / new.area;
        let compare_area = m != "SphericalSpiral";
        eprintln!(
            "{m:18} area ratio {ratio:.4}, volume {:+.3e} vs {:+.3e}",
            old.volume, new.volume
        );
        // Both trace the same trims, Rhino 2 along the file's 3D edge curves, Rhino 1 along
        // the 2D trim curves placed on the surface: within a few per cent.
        assert!(
            !compare_area || (ratio - 1.0).abs() < 0.04,
            "{m}: area ratio {ratio}"
        );
        assert!(
            old.volume * new.volume >= 0.0,
            "{m}: faces turned the other way"
        );
        let layers = |r: &Report| {
            r.structure
                .layers
                .iter()
                .map(|l| {
                    (
                        l.name.clone(),
                        l.visible,
                        (m != "T-Joint1").then_some(l.objects),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(layers(&old_report), layers(&new_report), "{m}: layers");
        compared += 1;
    }
    if compared > 0 {
        assert_eq!(compared, models.len());
    }
}

#[test]
fn rhino1_files_need_no_cadmpeg() {
    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/opennurbs/example_files/V1");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipped: samples not fetched");
        return;
    };
    let mut files = 0;
    for e in entries.flatten() {
        let path = e.path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some((m, report)) = measure(&format!("V1/{name}")) else {
            continue;
        };
        let inv = &report.inventory;
        let drawn = m.area > 0.0 || inv.curves > 0 || inv.points > 0;
        assert!(drawn, "{name}: nothing drawn");
        let undrawn = report
            .diagnostics
            .iter()
            .filter(|d| format!("{:?}", d.code).contains("BrepsWithoutRenderMesh"))
            .filter_map(|d| d.count)
            .sum::<usize>();
        let known = match name.as_str() {
            "v1_SphericalSpiral.3dm" => 1,
            "v1_Soccer.3dm" => 2,
            _ => 0,
        };
        assert!(undrawn <= known, "{name}: {undrawn} Breps not fully drawn");
        files += 1;
    }
    assert!(files == 0 || files == 26, "{files} Rhino 1 files");
}
