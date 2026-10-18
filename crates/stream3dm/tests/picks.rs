//! Picks: the selectable unit each displayed mesh belongs to (the object itself, or the
//! top-level block instance of a block member) and what each unit is, as a viewer selects them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use stream3dm::{DocumentInfo, Finish, InspectOptions, Inspector, MeshSink, Pick, PlacedMesh};

fn synthetic(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/synthetic")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[derive(Debug, Clone, PartialEq)]
struct Described {
    kind: &'static str,
    uuid: Option<String>,
    name: Option<String>,
    block: Option<String>,
    layer: Option<usize>,
    groups: Vec<usize>,
    user_strings: usize,
}

#[derive(Default)]
struct Picks {
    /// (object, pick, triangles) per displayed mesh.
    meshes: Vec<(u64, u64, usize)>,
    described: HashMap<u64, Described>,
    duplicates: usize,
}

impl MeshSink for Picks {
    fn document(&mut self, _: &DocumentInfo) {}
    fn mesh(&mut self, mesh: &PlacedMesh) {
        self.meshes
            .push((mesh.object, mesh.pick, mesh.triangles.len()));
    }
    fn pick(&mut self, pick: &Pick) {
        let d = Described {
            kind: pick.kind,
            uuid: pick.uuid.map(str::to_string),
            name: pick.name.map(str::to_string),
            block: pick.block.map(str::to_string),
            layer: pick.layer,
            groups: pick.groups.to_vec(),
            user_strings: pick.user_strings.len(),
        };
        if self.described.insert(pick.pick, d).is_some() {
            self.duplicates += 1;
        }
    }
}

fn picks(bytes: &[u8], options: InspectOptions) -> Picks {
    let sink = Rc::new(RefCell::new(Picks::default()));
    let mut inspector = Inspector::with_sink(options, Box::new(sink.clone()));
    loop {
        inspector.push(bytes).unwrap();
        if let Finish::Report(_) = inspector.finish().unwrap() {
            break;
        }
    }
    drop(inspector);
    Rc::try_unwrap(sink)
        .ok()
        .expect("sink released")
        .into_inner()
}

/// Pick → (description, meshes, triangles), in a stable order.
fn summary(p: &Picks) -> BTreeMap<u64, (Described, usize, usize)> {
    let mut out = BTreeMap::new();
    for &(_, pick, triangles) in &p.meshes {
        let entry = out
            .entry(pick)
            .or_insert_with(|| (p.described[&pick].clone(), 0, 0));
        entry.1 += 1;
        entry.2 += triangles;
    }
    out
}

#[test]
fn every_displayed_mesh_belongs_to_a_described_pick() {
    let p = picks(&synthetic("house.3dm"), InspectOptions::default());
    assert_eq!(p.duplicates, 0, "each pick described once");
    for &(object, pick, _) in &p.meshes {
        let d = p
            .described
            .get(&pick)
            .unwrap_or_else(|| panic!("pick {pick:x} undescribed"));
        if d.kind == "block" {
            assert_ne!(object, pick, "a block member keeps its own object id");
            assert!(d.block.is_some() && d.uuid.is_some(), "{d:?}");
        } else {
            assert_eq!(object, pick, "a model-space object is its own pick: {d:?}");
            assert_eq!(d.block, None);
        }
    }
    // Only units with displayed meshes are described.
    let shown: std::collections::HashSet<u64> = p.meshes.iter().map(|m| m.1).collect();
    assert_eq!(shown.len(), p.described.len());
}

#[test]
fn a_block_instance_is_one_pick_with_its_nested_members() {
    let p = picks(&synthetic("house.3dm"), InspectOptions::default());
    let s = summary(&p);
    let instances = |block: &str| {
        s.values()
            .filter(|(d, _, _)| d.block.as_deref() == Some(block))
            .collect::<Vec<_>>()
    };
    // Placements as the house's expected values count them (the nested "Finestra doppia"
    // places two more windows, which belong to its own instance, not to "Finestra 120x150").
    assert_eq!(instances("Finestra doppia").len(), 1);
    assert_eq!(instances("Finestra 120x150").len(), 10 - 2);
    assert_eq!(instances("Albero").len(), 6);
    assert_eq!(instances("Divano").len(), 2);
    assert_eq!(instances("Porta 90x210").len(), 1);
    let window = instances("Finestra 120x150")[0];
    let double = instances("Finestra doppia")[0];
    assert_eq!(double.1, 2 * window.1, "two windows' meshes");
    assert_eq!(double.2, 2 * window.2, "two windows' triangles");
    // Objects keep their names; Breps and meshes are told apart.
    let studio = s
        .values()
        .find(|(d, _, _)| d.name.as_deref() == Some("Volume studio"));
    assert!(
        studio.is_none_or(|(d, _, _)| d.kind == "brep"),
        "{studio:?}"
    );
    assert!(
        s.values()
            .any(|(d, _, _)| d.kind == "mesh" && d.layer.is_some())
    );
    assert!(
        s.values().any(|(d, _, _)| d.user_strings > 0),
        "user strings carried"
    );
}

#[test]
fn batch_size_does_not_change_the_picks() {
    let bytes = synthetic("house.3dm");
    let reference = summary(&picks(&bytes, InspectOptions::default()));
    for batch in [Some(1), Some(64 * 1024)] {
        let options = InspectOptions {
            batch_bytes: batch,
            ..Default::default()
        };
        assert_eq!(
            summary(&picks(&bytes, options)),
            reference,
            "batch {batch:?}"
        );
    }
}

#[test]
fn group_membership_is_carried() {
    // The house's only group, "Facciata sud", holds the five south-facing window instances.
    let p = picks(&synthetic("house.3dm"), InspectOptions::default());
    let grouped: Vec<&Described> = p
        .described
        .values()
        .filter(|d| !d.groups.is_empty())
        .collect();
    assert_eq!(grouped.len(), 5, "{grouped:?}");
    for d in grouped {
        assert_eq!(d.groups, [0]);
        assert_eq!(d.block.as_deref(), Some("Finestra 120x150"));
    }
}
