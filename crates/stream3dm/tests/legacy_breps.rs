//! Rhino 2 Breps (major 2: no chunks, no vertices) are tessellated like later ones. The
//! openNURBS example files hold each Rhino 2 model twice: as Rhino 2 wrote it (V2) and as
//! openNURBS converted it to the later layout (V3). Neither V2 file has Rhino's render meshes.
//! Most V3 files have none either, so both are tessellated by us and must agree; a few V3 files
//! carry Rhino's own meshes, which our V2 tessellation must come close to. (v3_SphericalSpiral
//! carries Rhino's meshes too, but one face of the model, a surface folded tighter than our
//! triangles, is rejected in both layouts, so it is left out.)

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::{DocumentInfo, Finish, InspectOptions, Inspector, MeshSink, PlacedMesh};

#[derive(Default)]
struct Area {
    area: f64,
    triangles: usize,
    min: [f64; 3],
    max: [f64; 3],
}

impl MeshSink for Area {
    fn document(&mut self, _: &DocumentInfo) {
        self.min = [f64::INFINITY; 3];
        self.max = [f64::NEG_INFINITY; 3];
    }
    fn mesh(&mut self, mesh: &PlacedMesh) {
        for t in mesh.triangles {
            let [a, b, c] = t.map(|i| mesh.positions[i as usize]);
            let (u, v) = (sub(b, a), sub(c, a));
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            self.area += (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() / 2.0;
            self.triangles += 1;
        }
        for p in mesh.positions {
            for (k, v) in p.iter().enumerate() {
                self.min[k] = self.min[k].min(*v);
                self.max[k] = self.max[k].max(*v);
            }
        }
    }
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// The meshes' area and box, or `None` when the samples have not been fetched.
fn area(path: &str) -> Option<Area> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/opennurbs/example_files")
        .join(path);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipped: {} not fetched (samples/fetch.sh)", path.display());
        return None;
    };
    let sink = Rc::new(RefCell::new(Area::default()));
    let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
    loop {
        inspector.push(&bytes).unwrap();
        if let Finish::Report(_) = inspector.finish().unwrap() {
            break;
        }
    }
    drop(inspector);
    Some(Rc::try_unwrap(sink).ok().unwrap().into_inner())
}

#[test]
fn rhino2_breps_match_their_conversion() {
    // (model, whether its V3 file holds Rhino's meshes)
    let models = [
        ("Camera", false),
        ("Lightbulb", false),
        ("Patch", false),
        ("DeformCubes", false),
        ("Soccer", false),
        ("MatchSrf", false),
        ("T-Joint1", false),
        ("T-Joint2", false),
        ("UntrimmedSurfaces", false),
        ("HumanHead2", false),
        ("SketchOnMesh", false),
        ("RhinoLogo", false),
        ("DeformSpheres", false),
        ("Flow2", false),
        ("Terrain", false),
        ("Stearman", false),
        ("Smooth", true),
        ("Speeder", true),
    ];
    let mut compared = 0;
    for (m, rhino) in models {
        let (Some(old), Some(new)) = (
            area(&format!("V2/v2_{m}.3dm")),
            area(&format!("V3/v3_{m}.3dm")),
        ) else {
            continue;
        };
        let size = (0..3).map(|k| new.max[k] - new.min[k]).fold(0.0, f64::max);
        let box_offset = (0..3)
            .map(|k| {
                (old.min[k] - new.min[k])
                    .abs()
                    .max((old.max[k] - new.max[k]).abs())
            })
            .fold(0.0, f64::max);
        let ratio = old.area / new.area;
        eprintln!(
            "{m:18} V2 {:>7} triangles, area ratio {ratio:.5}, box offset {box_offset:.2e} of {size:.3} m{}",
            old.triangles,
            if rhino {
                " (against Rhino's meshes)"
            } else {
                ""
            }
        );
        let (area_tolerance, box_tolerance) = if rhino { (0.01, 2e-3) } else { (1e-3, 1e-6) };
        assert!(
            (ratio - 1.0).abs() < area_tolerance,
            "{m}: area ratio {ratio}"
        );
        assert!(
            box_offset <= size * box_tolerance,
            "{m}: boxes {box_offset} apart"
        );
        compared += 1;
    }
    if compared > 0 {
        assert_eq!(compared, models.len());
    }
}
