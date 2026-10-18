//! The fingerprint's test vectors (docs/fingerprint.md, *Test vectors*), with the public key.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::fingerprint::{self, Distance, FingerprintDiff};
use stream3dm::{
    Fingerprint, FingerprintOptions, Fingerprinter, Finish, InspectOptions, Inspector, MeshSink,
    fingerprint_bytes,
};

fn synthetic(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/synthetic")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn print(bytes: &[u8], options: InspectOptions, clear_names: bool) -> Fingerprint {
    let fp = FingerprintOptions {
        key: None,
        clear_names,
    };
    fingerprint_bytes(bytes, options, &fp)
        .expect("fingerprint")
        .1
}

fn house(clear_names: bool) -> Fingerprint {
    print(
        &synthetic("house.3dm"),
        InspectOptions::default(),
        clear_names,
    )
}

fn compare(before: &Fingerprint, after: &Fingerprint) -> FingerprintDiff {
    fingerprint::diff(before, after).expect("comparable")
}

#[test]
fn house_matches_golden() {
    let actual = serde_json::to_string_pretty(&house(false)).unwrap() + "\n";
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/house.fingerprint.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    let expected =
        std::fs::read_to_string(&path).expect("golden fingerprint (UPDATE_GOLDEN=1 writes it)");
    assert_eq!(
        actual,
        expected,
        "house fingerprint differs from {}",
        path.display()
    );
}

#[test]
fn every_triangle_of_the_visible_geometry_is_counted() {
    let bytes = synthetic("house.3dm");
    let (report, fp) = fingerprint_bytes(
        &bytes,
        InspectOptions::default(),
        &FingerprintOptions::default(),
    )
    .unwrap();
    // The hidden "Note nascoste" box (12 triangles) is not displayed, so not fingerprinted.
    assert_eq!(
        fp.summary.triangles as usize,
        report.inventory.mesh_triangles - 12
    );
    assert_eq!(fp.summary.counts.objects, report.inventory.objects);
    assert!(
        fp.objects.windows(2).all(|w| w[0].0 < w[1].0),
        "objects sorted by key"
    );
}

#[test]
fn batch_and_slice_sizes_do_not_change_it() {
    let bytes = synthetic("house.3dm");
    let reference = house(false);
    for batch in [Some(1), Some(64 * 1024), None] {
        let options = InspectOptions {
            batch_bytes: batch,
            ..Default::default()
        };
        assert_eq!(print(&bytes, options, false), reference, "batch {batch:?}");
    }
    for slice in [7usize, 4096] {
        let sink = Rc::new(RefCell::new(Fingerprinter::new()));
        let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
        let report = loop {
            for part in bytes.chunks(slice) {
                inspector.push(part).unwrap();
            }
            if let Finish::Report(report) = inspector.finish().unwrap() {
                break report;
            }
        };
        drop(inspector);
        let fp = sink
            .borrow()
            .finish(&report, &FingerprintOptions::default());
        assert_eq!(fp, reference, "slices of {slice} bytes");
    }
}

#[test]
fn record_order_does_not_change_it() {
    let bytes = synthetic("house.3dm");
    let reference = print(
        &stream3dm::shuffled_file(&bytes, 0).unwrap(),
        InspectOptions::default(),
        false,
    );
    // Seed 0 is one order among others: compare several orders with each other and with the
    // original file's objects (block members included, identified by their instance's UUID).
    for seed in [1, 2, 3, 42] {
        let shuffled = stream3dm::shuffled_file(&bytes, seed).unwrap();
        let fp = print(&shuffled, InspectOptions::default(), false);
        assert_eq!(fp.digests, reference.digests, "seed {seed}");
    }
    let original = house(false);
    assert_eq!(reference.digests.objects, original.digests.objects);
    assert_eq!(reference.digests.geometry, original.digests.geometry);
}

#[test]
fn rhino_7_copy_has_the_same_objects() {
    let v8 = house(false);
    let v7 = print(&synthetic("house_v7.3dm"), InspectOptions::default(), false);
    assert_eq!(v7.summary.archive_version, Some(70));
    let d = compare(&v8, &v7);
    assert!(d.same_geometry);
    assert_eq!(v7.digests.objects, v8.digests.objects);
    assert_eq!(d.summary, vec!["archive_version".to_string()]);
    assert!(d.is_empty(), "{d:#?}");
}

#[test]
fn one_object_moved_by_a_centimetre() {
    let before = house(true);
    let after = print(
        &synthetic("house_moved.3dm"),
        InspectOptions::default(),
        true,
    );
    let d = compare(&before, &after);
    assert!(!d.same_geometry);
    let o = &d.objects;
    assert_eq!(o.changed.len(), 1, "{d:#?}");
    let change = &o.changed[0];
    assert!(change.moved, "a translation keeps the shape");
    assert_eq!(change.distance, Distance::Within0_1m);
    assert!(!change.layer_changed && !change.area_changed);
    assert!(
        o.added.is_empty()
            && o.removed.is_empty()
            && o.moved_to_layer.is_empty()
            && o.recreated.is_empty()
    );
    assert_eq!(o.unchanged, before.objects.len() - 1);
    // Its layer's content changed with it; nothing else did.
    let layer = |f: &Fingerprint, k: &str| {
        f.layers
            .iter()
            .find(|l| l.key == k)
            .and_then(|l| l.name.clone())
    };
    assert_eq!(d.layers.changed.len(), 1);
    assert_eq!(
        layer(&before, &d.layers.changed[0]).as_deref(),
        Some("Studio::Volume")
    );
    assert!(
        d.materials.added.is_empty() && d.views.changed.is_empty() && d.blocks.changed.is_empty()
    );
}

/// A closed box of 12 triangles from `min` to `max` (metres).
fn cube(min: [f64; 3], max: [f64; 3]) -> Mesh {
    let p = |i: u32| {
        [
            if i & 1 == 0 { min[0] } else { max[0] },
            if i & 2 == 0 { min[1] } else { max[1] },
            if i & 4 == 0 { min[2] } else { max[2] },
        ]
    };
    let positions = (0..8).map(p).collect();
    let faces = [
        [0, 2, 3, 1],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 4, 6, 2],
        [1, 3, 7, 5],
    ];
    let triangles = faces
        .iter()
        .flat_map(|f| [[f[0], f[1], f[2]], [f[0], f[2], f[3]]])
        .collect();
    (positions, triangles)
}

/// Positions and triangles of one mesh.
type Mesh = (Vec<[f64; 3]>, Vec<[u32; 3]>);

/// A fingerprint of synthetic objects `(id, layer, meshes)` against the house's tables.
fn of(objects: &[(u64, usize, Vec<Mesh>)]) -> Fingerprint {
    let report = stream3dm::inspect(&synthetic("house.3dm")).unwrap();
    let mut f = Fingerprinter::new();
    for (id, layer, meshes) in objects {
        for (positions, triangles) in meshes {
            f.mesh(&stream3dm::PlacedMesh {
                positions,
                triangles,
                object: *id,
                pick: *id,
                layer: Some(*layer),
                material: None,
                color: None,
            });
        }
    }
    f.finish(&report, &FingerprintOptions::default())
}

#[test]
fn moves_reshapes_and_new_identities_are_told_apart() {
    let a = cube([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let b = cube([5.0, 0.0, 0.0], [6.0, 2.0, 1.0]);
    let part = cube([10.0, 0.0, 0.0], [10.5, 0.5, 0.5]);
    let rest = cube([11.0, 0.0, 0.0], [12.0, 1.0, 1.0]);
    let before = of(&[
        (1, 0, vec![a.clone()]),
        (2, 0, vec![b.clone()]),
        (3, 0, vec![part.clone(), rest.clone()]),
        (4, 0, vec![cube([20.0, 0.0, 0.0], [21.0, 1.0, 1.0])]),
        (5, 0, vec![cube([30.0, 0.0, 0.0], [31.0, 1.0, 1.0])]),
        (6, 0, vec![cube([40.0, 0.0, 0.0], [41.0, 1.0, 1.0])]),
    ]);
    let after = of(&[
        // 1: translated by 3 m, same shape.
        (1, 0, vec![cube([3.0, 0.0, 0.0], [4.0, 1.0, 1.0])]),
        // 2: stretched by 2 cm, roughly in place, on another layer.
        (2, 1, vec![cube([5.0, 0.0, 0.0], [6.02, 2.0, 1.0])]),
        // 3: one of its two parts moved 0.3 m inside the object (same edges, other moments).
        (3, 0, vec![cube([10.3, 0.0, 0.0], [10.8, 0.5, 0.5]), rest]),
        // 4 → 40: same geometry under a new identity (copy-paste in place).
        (40, 0, vec![cube([20.0, 0.0, 0.0], [21.0, 1.0, 1.0])]),
        // 5 → 50: same shape elsewhere under a new identity.
        (50, 0, vec![cube([35.0, 2.0, 0.0], [36.0, 3.0, 1.0])]),
        // 6 removed; 7 added (a shape seen nowhere before).
        (7, 0, vec![cube([50.0, 0.0, 0.0], [52.0, 1.0, 1.0])]),
    ]);
    let d = compare(&before, &after);
    let o = &d.objects;
    assert_eq!(o.changed.len(), 3, "{o:#?}");
    let by_moved: Vec<_> = o
        .changed
        .iter()
        .map(|c| (c.moved, c.distance, c.layer_changed))
        .collect();
    assert!(
        by_moved.contains(&(true, Distance::Beyond1m, false)),
        "object 1 moved 3 m: {by_moved:?}"
    );
    assert!(
        by_moved.contains(&(false, Distance::Within0_1m, true)),
        "object 2 reshaped in place, new layer: {by_moved:?}"
    );
    assert!(
        by_moved.contains(&(false, Distance::Within1m, false)),
        "object 3 part moved: {by_moved:?}"
    );
    assert_eq!(o.recreated.len(), 2, "{o:#?}");
    assert_eq!(o.recreated.iter().filter(|r| r.in_place).count(), 1);
    assert_eq!(o.removed.len(), 1);
    assert_eq!(o.added.len(), 1);
}

#[test]
fn one_layer_renamed() {
    let before = house(true);
    let after = print(
        &synthetic("house_renamed.3dm"),
        InspectOptions::default(),
        true,
    );
    let d = compare(&before, &after);
    assert!(d.same_geometry, "a rename leaves the shapes alone");
    let names = |f: &Fingerprint, keys: &[String]| -> Vec<String> {
        keys.iter()
            .filter_map(|k| {
                f.layers
                    .iter()
                    .find(|l| &l.key == k)
                    .and_then(|l| l.name.clone())
            })
            .collect()
    };
    assert_eq!(names(&before, &d.layers.removed), ["Studio::Volume"]);
    assert_eq!(names(&after, &d.layers.added), ["Studio::Massa"]);
    assert_eq!(d.objects.moved_to_layer.len(), 1);
    assert!(d.objects.added.is_empty() && d.objects.removed.is_empty());
}

#[test]
fn one_object_added() {
    let before = house(false);
    let after = print(
        &synthetic("house_added.3dm"),
        InspectOptions::default(),
        false,
    );
    let d = compare(&before, &after);
    assert_eq!(d.objects.added.len(), 1);
    assert_eq!(d.objects.unchanged, before.objects.len());
    assert!(d.objects.removed.is_empty());
}

#[test]
fn nothing_of_the_model_is_in_clear() {
    let bytes = synthetic("house.3dm");
    let report = stream3dm::inspect(&bytes).unwrap();
    let json = serde_json::to_string(&house(false)).unwrap();
    let mut names: Vec<String> = Vec::new();
    names.extend(
        report
            .structure
            .layers
            .iter()
            .flat_map(|l| [l.name.clone(), l.path.clone()]),
    );
    names.extend(report.materials.iter().map(|m| m.name.clone()));
    names.extend(report.blocks.iter().map(|b| b.name.clone()));
    names.extend(report.views.iter().map(|v| v.name.clone()));
    names.extend(report.structure.object_names.iter().cloned());
    for name in names.iter().filter(|n| n.len() >= 3) {
        assert!(
            !json.contains(&format!("\"{name}\"")),
            "{name} appears in the fingerprint"
        );
    }
    // No fractional numbers: coordinates, sizes and parameters only enter through hashes.
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    fn no_floats(v: &serde_json::Value) {
        match v {
            serde_json::Value::Number(n) => assert!(n.is_i64() || n.is_u64(), "number {n}"),
            serde_json::Value::Array(a) => a.iter().for_each(no_floats),
            serde_json::Value::Object(o) => o.values().for_each(no_floats),
            _ => {}
        }
    }
    no_floats(&value);
}

#[test]
fn a_key_changes_every_hash_and_keys_must_match() {
    let bytes = synthetic("house.3dm");
    let public = house(false);
    let options = FingerprintOptions {
        key: Some([7; 32]),
        clear_names: false,
    };
    let keyed = fingerprint_bytes(&bytes, InspectOptions::default(), &options)
        .unwrap()
        .1;
    assert!(keyed.keyed && !public.keyed);
    assert_ne!(keyed.key_id, public.key_id);
    assert_ne!(keyed.digests.geometry, public.digests.geometry);
    let public_keys: std::collections::HashSet<_> = public.objects.iter().map(|o| &o.0).collect();
    assert!(keyed.objects.iter().all(|o| !public_keys.contains(&o.0)));
    assert_eq!(
        keyed.summary, public.summary,
        "the clear summary does not depend on the key"
    );
    assert!(fingerprint::diff(&public, &keyed).is_err());
}
