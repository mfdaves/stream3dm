//! Curves and points reach the sink: every curve object as a polyline, every point object and
//! point cloud as its points, in model space and once per placement of a block. Counts are
//! rhino3dm.js's (McNeel's reader) on the same files.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::{
    DocumentInfo, Finish, InspectOptions, Inspector, MeshSink, Pick, PlacedMesh, PlacedWire,
};

#[derive(Default)]
struct Wires {
    /// (object, pick, points) per curve and per set of points.
    curves: Vec<(u64, u64, usize)>,
    points: Vec<(u64, u64, usize)>,
    picks: HashMap<u64, &'static str>,
    all_finite: bool,
}

impl MeshSink for Wires {
    fn document(&mut self, _: &DocumentInfo) {
        self.all_finite = true;
    }
    fn mesh(&mut self, _: &PlacedMesh) {}
    fn curve(&mut self, curve: &PlacedWire) {
        self.all_finite &= curve.positions.iter().flatten().all(|v| v.is_finite());
        self.curves
            .push((curve.object, curve.pick, curve.positions.len()));
    }
    fn points(&mut self, points: &PlacedWire) {
        self.all_finite &= points.positions.iter().flatten().all(|v| v.is_finite());
        self.points
            .push((points.object, points.pick, points.positions.len()));
    }
    fn pick(&mut self, pick: &Pick) {
        self.picks.insert(pick.pick, pick.kind);
    }
}

/// The file's wires, or `None` when the samples have not been fetched.
fn wires(path: &str) -> Option<Wires> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples")
        .join(path);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipped: {} not fetched (samples/fetch.sh)", path.display());
        return None;
    };
    let sink = Rc::new(RefCell::new(Wires::default()));
    let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
    loop {
        inspector.push(&bytes).unwrap();
        if let Finish::Report(_) = inspector.finish().unwrap() {
            break;
        }
    }
    drop(inspector);
    let w = Rc::try_unwrap(sink)
        .ok()
        .expect("sink released")
        .into_inner();
    assert!(w.all_finite, "{}: non-finite position", path.display());
    for &(_, pick, _) in w.curves.iter().chain(&w.points) {
        assert!(
            w.picks.contains_key(&pick),
            "{}: pick {pick:x} undescribed",
            path.display()
        );
    }
    Some(w)
}

fn total(list: &[(u64, u64, usize)]) -> usize {
    list.iter().map(|w| w.2).sum()
}

#[test]
fn curve_objects_are_polylines() {
    for (file, curves) in [
        ("opennurbs/example_files/V7/v7_my_curves.3dm", 4),
        ("opennurbs/example_files/V2/v2_5_curves.3dm", 5),
        // Rhino 2 arrows, read as lines as openNURBS does (3, 5 and 2 of them).
        ("opennurbs/example_files/V2/v2_T-Joint1.3dm", 6),
        ("opennurbs/example_files/V2/v2_Patch.3dm", 17),
        ("opennurbs/example_files/V2/v2_Stearman.3dm", 117),
        ("three.js/examples/models/3dm/Rhino_Logo.3dm", 218),
    ] {
        let Some(w) = wires(file) else { continue };
        assert_eq!(w.curves.len(), curves, "{file}");
        assert!(
            w.curves.iter().all(|c| c.2 >= 2),
            "{file}: every curve has a segment"
        );
    }
}

#[test]
fn points_and_point_clouds_are_points() {
    for (file, sets, points) in [
        ("opennurbs/example_files/V7/v7_my_points.3dm", 3, 5),
        ("opennurbs/example_files/V3/v3_my_points.3dm", 3, 5),
        ("opennurbs/example_files/V2/v2_23_points.3dm", 23, 23),
        ("opennurbs/example_files/V4/v4_example_file.3dm", 4, 8),
        ("opennurbs/example_files/V5/v5_example_file.3dm", 2, 12),
        (
            "opennurbs/example_files/V8/v8_selective_clipping.3dm",
            6,
            279,
        ),
        ("rhino3dm/tests/models/file3dm_stuff.3dm", 9, 16),
        // Its point cloud (848 points) is on the layer "pointclouds", which is off.
        ("three.js/examples/models/3dm/Rhino_Logo.3dm", 0, 0),
    ] {
        let Some(w) = wires(file) else { continue };
        assert_eq!((w.points.len(), total(&w.points)), (sets, points), "{file}");
    }
}

#[test]
fn a_curve_in_a_block_is_drawn_once_per_placement() {
    let Some(w) = wires("rhino3dm/tests/models/blocks.3dm") else {
        return;
    };
    assert_eq!(w.curves.len(), 2, "one curve member, two placements");
    let picks: Vec<u64> = w.curves.iter().map(|c| c.1).collect();
    assert_ne!(picks[0], picks[1], "each placement is its own pick");
    for &(object, pick, _) in &w.curves {
        assert_ne!(object, pick, "a block member keeps its own object id");
        assert_eq!(w.picks[&pick], "block");
    }
}
