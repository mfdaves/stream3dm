//! The cadmpeg path: document tables and object views from one cadmpeg decode, for archive 1
//! files, records our reader leaves to cadmpeg, and the tests that compare the two readers.

use super::*;
use crate::native::{LinksRec, ObjectRec, OccurrenceRec, arena, classify};
use crate::object::InstanceView;
use cadmpeg_ir::DecodeResult;
use cadmpeg_ir::geometry::{SolvedSurfaceGeometry, SurfaceGeometry};
use std::collections::HashSet;

/// Class id of block instance references.
const INSTANCE_REF_CLASS: &str = "f9cfb638-b9d4-4340-87e3-c56e7865d96a";
/// Class ids of object records the decoder may retain undecoded.
const CLASS_NAMES: [(&str, &str); 11] = [
    ("60b5dbc5-e660-11d3-bfe4-0010830122f0", "Brep"),
    ("4ed7d4e4-e947-11d3-bfe5-0010830122f0", "Mesh"),
    ("36f53175-72b8-4d47-bf1f-b4e6fc24f4b9", "Extrusion"),
    (INSTANCE_REF_CLASS, "InstanceRef"),
    ("4ed7d4db-e947-11d3-bfe5-0010830122f0", "LineCurve"),
    ("4ed7d4e6-e947-11d3-bfe5-0010830122f0", "PolylineCurve"),
    ("4ed7d4dd-e947-11d3-bfe5-0010830122f0", "NurbsCurve"),
    ("4ed7d4de-e947-11d3-bfe5-0010830122f0", "NurbsSurface"),
    ("4ed7d4e0-e947-11d3-bfe5-0010830122f0", "PolyCurve"),
    ("2488f347-f8fa-11d3-bfec-0010830122f0", "PointCloud"),
    ("57376349-62a9-4a16-b411-a46bcd544790", "Text"),
];

impl Tables {
    /// The document tables as cadmpeg decoded them.
    pub(crate) fn from_decoded(decoded: &DecodeResult) -> Self {
        Self::from_document(document_from_decoded(decoded))
    }
}

impl Facts {
    /// Adds the objects of one cadmpeg decode.
    pub(crate) fn absorb(&mut self, tables: &Tables, decoded: &DecodeResult, mode: Mode) {
        let batch = views_from_decode(decoded);
        for (name, n) in batch.retained {
            *self.retained.entry(name).or_default() += n;
        }
        if mode == Mode::Batch {
            self.inventory.lights += batch.lights;
        }
        self.unparsed += batch.unparsed;
        for definition in &batch.unmapped_occurrences {
            self.count_occurrence(definition.as_deref());
        }
        for view in &batch.views {
            self.absorb_object(tables, view, mode);
        }
        for mesh in &batch.orphans {
            self.absorb_orphan_mesh(tables, mesh);
        }
        self.faces += batch.orphan_faces;
        self.planar_faces += batch.orphan_planar;
    }

    /// Adds a mesh that belongs to no object (counted in geometry, not in the inventory).
    fn absorb_orphan_mesh(&mut self, tables: &Tables, geom: &MeshGeom) {
        let attrs = MeshAttrs {
            visible: geom.source_visible.unwrap_or(true),
            layer: geom
                .source_layer
                .as_deref()
                .and_then(|u| tables.layer_by_uuid.get(u).copied()),
            color: None,
            material: None,
            material_by_parent: false,
            site: false,
        };
        let mesh = mesh_data(geom, attrs);
        add_mesh(
            &mut self.acc,
            &mut self.inventory,
            &mut self.sink,
            tables,
            &mesh,
            &mesh.attrs,
            None,
            object_id(&[geom.key.as_bytes()]),
            object_id(&[geom.key.as_bytes()]),
        );
    }
}

/// The document tables of one cadmpeg decode.
pub(crate) fn document_from_decoded(decoded: &DecodeResult) -> Document {
    let ir = decoded.ir();
    let ns = ir.native.namespace("rhino");
    let attr = |key: &str| {
        ir.source
            .as_ref()?
            .attributes
            .iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v.as_str())
    };
    fn records<T: serde::de::DeserializeOwned>(
        ns: Option<&cadmpeg_ir::native::NativeNamespace>,
        name: &str,
        unparsed: &mut usize,
    ) -> Vec<T> {
        let a = arena(ns, name);
        *unparsed += a.unparsed;
        a.records.into_iter().map(|(_, r)| r).collect()
    }
    let mut unparsed = 0;
    let layers = records(ns, "layers", &mut unparsed);
    let materials = records(ns, "materials", &mut unparsed);
    let views = records(ns, "views", &mut unparsed);
    let groups = records(ns, "groups", &mut unparsed);
    let definitions = records(ns, "product_definitions", &mut unparsed);
    Document {
        archive_version: attr("archive_version").and_then(|v| v.parse::<u32>().ok()),
        application: attr("application_name").map(str::to_string),
        application_details: attr("application_details").map(str::to_string),
        unit_system: attr("unit_system").map(str::to_string),
        mm_per_unit: attr("millimeters_per_unit").and_then(|v| v.parse::<f64>().ok()),
        absolute_tolerance_mm: attr("absolute_tolerance_millimeters")
            .and_then(|v| v.parse::<f64>().ok()),
        layers,
        materials,
        views,
        groups,
        definitions,
        lights: ns
            .and_then(|ns| ns.arenas().get("lights"))
            .map_or(0, Vec::len),
        light_list: lights_from_native(
            ns,
            attr("millimeters_per_unit").and_then(|v| v.parse::<f64>().ok()),
        ),
        // cadmpeg skips the earth anchor's values.
        earth_anchor: None,
        unparsed,
    }
}

/// What one cadmpeg decode holds, as object views.
pub(crate) struct DecodedBatch {
    pub views: Vec<ObjectView>,
    /// Meshes that belong to no object.
    pub orphans: Vec<MeshGeom>,
    pub orphan_faces: usize,
    pub orphan_planar: usize,
    /// Occurrences whose instance object is missing (definition UUID when known).
    pub unmapped_occurrences: Vec<Option<String>>,
    /// Retained (undecoded) object records by class name.
    pub retained: Vec<(String, usize)>,
    pub lights: usize,
    pub unparsed: usize,
}

/// Converts one cadmpeg decode into object views.
pub(crate) fn views_from_decode(decoded: &DecodeResult) -> DecodedBatch {
    let ir = decoded.ir();
    let ns = ir.native.namespace("rhino");
    let mut batch = DecodedBatch {
        views: Vec::new(),
        orphans: Vec::new(),
        orphan_faces: 0,
        orphan_planar: 0,
        unmapped_occurrences: Vec::new(),
        retained: Vec::new(),
        lights: ns
            .and_then(|ns| ns.arenas().get("lights"))
            .map_or(0, Vec::len),
        unparsed: 0,
    };

    for loss in &decoded.report().losses {
        if let Some((n, class)) = retained_class(&loss.message) {
            if class == INSTANCE_REF_CLASS {
                continue; // instances are placed from their occurrences
            }
            let name = CLASS_NAMES
                .iter()
                .find(|(id, _)| *id == class)
                .map_or(class, |(_, name)| name);
            batch.retained.push((name.to_string(), n));
        }
    }

    // Object records tell what each object decoded into; instances and text dots link to
    // theirs. Their links also join meshes and surfaces to objects.
    let records = arena::<LinksRec>(ns, "unknowns");
    let occurrences = arena::<OccurrenceRec>(ns, "product_occurrences");
    let text_dots = arena::<LinksRec>(ns, "text_dots");
    let object_arena = arena::<ObjectRec>(ns, "object_presentation");
    batch.unparsed +=
        records.unparsed + occurrences.unparsed + text_dots.unparsed + object_arena.unparsed;
    let kinds: HashMap<&str, ObjectKind> = records
        .records
        .iter()
        .map(|(id, r)| (id.as_str(), classify(&r.links)))
        .collect();
    let mut entity_record: HashMap<&str, &str> = HashMap::new();
    for (id, r) in &records.records {
        for link in &r.links {
            entity_record.insert(link.as_str(), id.as_str());
        }
    }
    let instance_records: HashSet<&str> = occurrences
        .records
        .iter()
        .flat_map(|(_, o)| o.links.iter().map(String::as_str))
        .collect();
    let text_dot_records: HashSet<&str> = text_dots
        .records
        .iter()
        .flat_map(|(_, t)| t.links.iter().map(String::as_str))
        .collect();

    let mut views: Vec<ObjectView> = Vec::with_capacity(object_arena.records.len());
    let mut view_by_uuid: HashMap<&str, usize> = HashMap::new();
    let mut record_to_view: HashMap<&str, usize> = HashMap::new();
    for (i, (_, o)) in object_arena.records.iter().enumerate() {
        let record = o
            .links
            .iter()
            .find(|l| l.starts_with("rhino:object:record#"))
            .map(String::as_str);
        let kind = match record
            .and_then(|r| kinds.get(r))
            .copied()
            .unwrap_or(ObjectKind::Unlinked)
        {
            ObjectKind::Unlinked if record.is_some_and(|r| instance_records.contains(r)) => {
                Counted::BlockInstance
            }
            ObjectKind::Unlinked if record.is_some_and(|r| text_dot_records.contains(r)) => {
                Counted::Object(ObjectKind::Annotation)
            }
            kind => Counted::Object(kind),
        };
        if let Some(uuid) = o.source_uuid.as_deref() {
            view_by_uuid.insert(uuid, views.len());
        }
        if let Some(record) = record {
            record_to_view.insert(record, views.len());
        }
        views.push(ObjectView {
            uuid: o.source_uuid.clone(),
            fallback_key: format!("record {i}"),
            name: o.name.clone(),
            layer_index: o.layer_index,
            material_index: o.material_index,
            material_source: o.material_source,
            visible: o.visible,
            mode: o.object_mode,
            groups: o.group_indexes.clone(),
            user_strings: o
                .attribute_user_strings
                .iter()
                .flatten()
                .map(|u| {
                    (
                        u.key.clone().unwrap_or_default(),
                        u.value.clone().unwrap_or_default(),
                    )
                })
                .collect(),
            kind,
            meshes: Vec::new(),
            curves: Vec::new(),
            points: Vec::new(),
            instance: None,
            faces: 0,
            planar_faces: 0,
            generated_mesh: false,
            undisplayed_faces: 0,
            class: None,
            type_name: None,
        });
    }
    let view_of = |entity: &str, uuid: Option<&str>| -> Option<usize> {
        entity_record
            .get(entity)
            .and_then(|r| record_to_view.get(r))
            .or_else(|| uuid.and_then(|u| view_by_uuid.get(u)))
            .copied()
    };

    for (_, o) in &occurrences.records {
        let view = o
            .links
            .iter()
            .find(|l| l.starts_with("rhino:object:record#"))
            .and_then(|r| record_to_view.get(r.as_str()));
        match (view, &o.definition_uuid) {
            (Some(&v), Some(definition)) => {
                views[v].instance = Some(InstanceView {
                    definition: definition.clone(),
                    matrix: o.matrix_mm(),
                });
            }
            (_, definition) => batch.unmapped_occurrences.push(definition.clone()),
        }
    }

    let mut orphans = Vec::new();
    for t in &ir.model.tessellations {
        let source = t.source_object.as_ref();
        let mesh = MeshGeom {
            positions_mm: t
                .vertices()
                .into_iter()
                .map(|p| {
                    let p = p.get();
                    [p.x, p.y, p.z]
                })
                .collect(),
            triangles: t.triangles(),
            vertex_count: t.vertex_count(),
            triangle_count: t.triangle_count(),
            in_instance: source.is_some_and(|s| !s.instance_path.is_empty()),
            source_visible: source.and_then(|s| s.visible),
            source_layer: source.and_then(|s| s.layer.clone()),
            key: t.id.as_str().to_string(),
            color: None,
        };
        match view_of(t.id.as_str(), source.map(|s| s.object_id.as_str())) {
            Some(v) => views[v].meshes.push(mesh),
            None => orphans.push(mesh),
        }
    }

    // SubD objects: the control cage subdivided twice.
    for s in &ir.model.subds {
        let source = s.source_object.as_ref();
        let Some((positions_mm, triangles)) =
            crate::subdivide::mesh(&s.cage, crate::subdivide::LEVELS)
        else {
            continue;
        };
        let mesh = MeshGeom {
            vertex_count: positions_mm.len(),
            triangle_count: triangles.len(),
            positions_mm,
            triangles,
            in_instance: source.is_some_and(|s| !s.instance_path.is_empty()),
            source_visible: source.and_then(|s| s.visible),
            source_layer: source.and_then(|s| s.layer.clone()),
            key: s.id.as_str().to_string(),
            color: None,
        };
        match view_of(s.id.as_str(), source.map(|s| s.object_id.as_str())) {
            Some(v) => {
                views[v].meshes.push(mesh);
                views[v].generated_mesh = true;
            }
            None => orphans.push(mesh),
        }
    }

    // Planar faces: counted per object; faces of decoder-expanded blocks are left out.
    let model = &ir.model;
    let surfaces: HashMap<&str, (bool, bool, Option<usize>)> = model
        .surfaces
        .iter()
        .map(|s| {
            let source = s.source_object.as_ref();
            let planar = matches!(
                &s.geometry,
                SurfaceGeometry::Solved(SolvedSurfaceGeometry::Plane(_))
                    | SurfaceGeometry::Procedural {
                        cache: Some(SolvedSurfaceGeometry::Plane(_)),
                        ..
                    }
            );
            let in_instance = source.is_some_and(|o| !o.instance_path.is_empty());
            let view = view_of(s.id.as_str(), source.map(|o| o.object_id.as_str()));
            (s.id.as_str(), (planar, in_instance, view))
        })
        .collect();
    let (mut orphan_faces, mut orphan_planar) = (0, 0);
    for face in &model.faces {
        match surfaces.get(face.surface.as_str()) {
            None => orphan_faces += 1,
            Some(&(_, true, _)) => {}
            Some(&(planar, false, Some(v))) => {
                views[v].faces += 1;
                views[v].planar_faces += usize::from(planar);
            }
            Some(&(planar, false, None)) => {
                orphan_faces += 1;
                orphan_planar += usize::from(planar);
            }
        }
    }

    batch.views = views;
    batch.orphans = orphans;
    batch.orphan_faces = orphan_faces;
    batch.orphan_planar = orphan_planar;
    batch
}

/// Parses "retained N object record(s) for class UUID; ...".
fn retained_class(message: &str) -> Option<(usize, &str)> {
    let rest = message.strip_prefix("retained ")?;
    let (n, rest) = rest.split_once(" object record(s) for class ")?;
    let class = rest.split(';').next()?.trim();
    Some((n.parse().ok()?, class))
}

/// cadmpeg's light records (lengths in millimetres) as the document reader's lights (model
/// units), with their attributes' layer and visibility.
fn lights_from_native(
    ns: Option<&cadmpeg_ir::native::NativeNamespace>,
    mm_per_unit: Option<f64>,
) -> Vec<crate::reader::document::LightRec> {
    #[derive(serde::Deserialize)]
    struct Attributes {
        layer_index: i32,
        visible: bool,
    }
    #[derive(serde::Deserialize)]
    struct Light {
        name: String,
        enabled: bool,
        style: i32,
        intensity: f64,
        watts: f64,
        diffuse: String,
        direction: [f64; 3],
        location: [f64; 3],
        spot_angle_degrees: f64,
        length: [f64; 3],
        width: [f64; 3],
        hotspot: f64,
        attributes: Option<Attributes>,
    }
    let unit = mm_per_unit.unwrap_or(crate::extract::ASSUMED_MM_PER_UNIT);
    let Some(records) = ns.and_then(|ns| ns.arenas().get("lights")) else {
        return Vec::new();
    };
    records
        .iter()
        .filter_map(|r| serde_json::from_value::<Light>(serde_json::to_value(r).ok()?).ok())
        .map(|l| {
            let byte = |i: usize| {
                u8::from_str_radix(l.diffuse.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0)
            };
            let model = |v: [f64; 3]| v.map(|x| x / unit);
            crate::reader::document::LightRec {
                name: l.name,
                style: l.style,
                on: l.enabled,
                intensity: l.intensity,
                watts: l.watts,
                diffuse: [byte(0), byte(1), byte(2)],
                direction: l.direction,
                location: model(l.location),
                spot_angle_deg: l.spot_angle_degrees,
                length: model(l.length),
                width: model(l.width),
                hotspot: Some(l.hotspot).filter(|h| h.is_finite() && h.abs() < 1e300),
                layer: l.attributes.as_ref().map_or(-1, |a| a.layer_index),
                visible: l.attributes.as_ref().is_none_or(|a| a.visible),
            }
        })
        .collect()
}
