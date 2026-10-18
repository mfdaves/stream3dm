//! Surface objects (plane, NURBS, revolution, sum surfaces saved on their own, not as Breps)
//! have no trims and no render mesh: they are drawn as one untrimmed face over their domain.
//! The openNURBS examples write one in each of these files, from Rhino 2's format to Rhino 7's.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::{
    DiagnosticCode, DocumentInfo, Finish, InspectOptions, Inspector, MeshSink, PlacedMesh,
};

#[derive(Default)]
struct Area(f64);

impl MeshSink for Area {
    fn document(&mut self, _: &DocumentInfo) {}
    fn mesh(&mut self, mesh: &PlacedMesh) {
        for t in mesh.triangles {
            let [a, b, c] = t.map(|i| mesh.positions[i as usize]);
            let (u, v) = (
                [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
                [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
            );
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            self.0 += (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() / 2.0;
        }
    }
}

#[test]
fn surface_objects_are_drawn() {
    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/opennurbs/example_files");
    let files = [
        "V2/v2_my_surfaces.3dm",
        "V2/v2_my_spot_light.3dm",
        "V3/v3_my_surfaces.3dm",
        "V3/v3_my_spot_light.3dm",
        "V5/v5_my_surfaces.3dm",
        "V5/v5_my_spot_light.3dm",
        "V6/my_viewports.3dm",
        "V6/my_spot_light.3dm",
        "V7/v7_my_surfaces.3dm",
        "V7/v7_my_spot_light.3dm",
        "V7/v7_my_viewports.3dm",
    ];
    for f in files {
        let Ok(bytes) = std::fs::read(dir.join(f)) else {
            eprintln!("skipped: {f} not fetched (samples/fetch.sh)");
            return;
        };
        let sink = Rc::new(RefCell::new(Area::default()));
        let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
        let report = loop {
            inspector.push(&bytes).unwrap();
            if let Finish::Report(r) = inspector.finish().unwrap() {
                break r;
            }
        };
        drop(inspector);
        let area = sink.borrow().0;
        assert!(area > 0.0, "{f}: nothing drawn");
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::BrepsWithoutRenderMesh),
            "{f}: a surface is reported undisplayed"
        );
    }
}
