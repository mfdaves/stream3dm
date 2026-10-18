//! Report assembly from decoded `.3dm` content, in three parts: document [`Tables`], object
//! [`Facts`] absorbed decode by decode, and [`finalize`].
//!
//! Positional record ids (`rhino:object:record#N`) are only joined within one decode; across
//! decodes objects are joined by UUID. Block members are placed when their meshes arrive, using
//! the placements gathered beforehand ([`Facts::prepare_placements`]), so no object geometry is
//! retained.

use crate::geometry::{Accumulator, Bounds, MeshInput, MeshStats, float, norm};
use crate::native::{
    DefinitionRec, GroupRec, LayerRec, MaterialRec, Matrix, ObjectKind, ViewRec, apply, hex_color,
    multiply, rgb_color,
};
use crate::object::{Counted, MeshGeom, ObjectView};
use crate::reader::document::Document;
use crate::report::*;
use crate::sink::{
    DocumentInfo, LayerInfo, MaterialInfo, MeshSink, Pick, PlacedMesh, PlacedWire, object_id,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[cfg(feature = "cadmpeg")]
mod decoded;
#[cfg(all(feature = "cadmpeg", test))]
pub(crate) use decoded::views_from_decode;

/// Version of the [`Report`] layout, in [`Report::schema_version`].
pub const SCHEMA_VERSION: u32 = 3;

/// Nested block placements deeper than this are ignored.
const MAX_BLOCK_DEPTH: usize = 16;
/// Archive versions the decoder rates as opened (L1); others are detected only.
const SUPPORTED_ARCHIVES: [u32; 7] = [2, 3, 4, 50, 60, 70, 80];
/// Distinct values kept per user-string key, and keys kept (the smallest, so the result does
/// not depend on object order).
const MAX_USER_STRING_VALUES: usize = 5;
const MAX_USER_STRING_KEYS: usize = 100;
/// Model-space block placements resolved at most (nested chains multiply quickly).
const MAX_EXPANDED_PLACEMENTS: usize = 1_000_000;
/// Floor-level candidates listed in the report.
const MAX_LEVELS: usize = 12;
/// `ON::object_material_source`: from layer 0, from object 1, from parent 3.
const MATERIAL_FROM_OBJECT: i64 = 1;
const MATERIAL_FROM_PARENT: i64 = 3;
/// `ON::object_mode` value of block definition members (low nibble).
const MODE_IDEF_OBJECT: i64 = 3;
/// Lower-case fragments of layer or material names that denote glazing.
const GLAZING_WORDS: [&str; 11] = [
    "glass", "glazing", "glazed", "window", "vetr", "finestr", "verre", "vidrio", "ventana",
    "glas", "fenster",
];
/// Lower-case fragments of layer paths that denote the site (terrain, landscape); their
/// horizontal area is left out of floor levels.
const SITE_WORDS: [&str; 10] = [
    "site",
    "sito",
    "terrain",
    "terreno",
    "topo",
    "ground",
    "landscape",
    "paesaggio",
    "gelände",
    "terrai",
];
/// `ON::LengthUnitSystem` codes 0..=25.
const UNIT_NAMES: [&str; 26] = [
    "none",
    "microns",
    "millimeters",
    "centimeters",
    "meters",
    "kilometers",
    "microinches",
    "mils",
    "inches",
    "feet",
    "miles",
    "custom",
    "angstroms",
    "nanometers",
    "decimeters",
    "dekameters",
    "hectometers",
    "megameters",
    "gigameters",
    "yards",
    "printer_points",
    "printer_picas",
    "nautical_miles",
    "astronomical_units",
    "light_years",
    "parsecs",
];

/// How a decode relates to the original file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The whole file: the decoder expanded block instances itself and never decodes block
    /// members at top level.
    #[cfg_attr(not(feature = "cadmpeg"), allow(dead_code))]
    Whole,
    /// A batch decoded without the instance-definition and light tables: members decode as
    /// ordinary objects and only light objects appear in the lights arena.
    Batch,
}

/// File-level attributes from the decoder's source metadata.
struct FileBase {
    archive_version: Option<u32>,
    application: Option<String>,
    application_details: Option<String>,
    unit_system: String,
    metres_per_unit: Option<f64>,
    /// As decoded, for converting geometry read by our own reader.
    mm_per_unit: Option<f64>,
    absolute_tolerance_m: Option<f64>,
}

/// Document tables: everything outside the object table that the report needs.
pub(crate) struct Tables {
    file: FileBase,
    /// Sorted by archive index.
    layers: Vec<LayerRec>,
    layer_by_index: HashMap<i64, usize>,
    #[cfg_attr(not(feature = "cadmpeg"), allow(dead_code))]
    layer_by_uuid: HashMap<String, usize>,
    layer_paths: Vec<String>,
    layer_material: Vec<Option<usize>>,
    /// Visible with every parent layer visible.
    layer_visible: Vec<bool>,
    glazing_layers: Vec<bool>,
    site_layers: Vec<bool>,
    /// Sorted by archive index.
    materials: Vec<MaterialRec>,
    material_by_index: HashMap<i64, usize>,
    glazing_materials: Vec<bool>,
    views: Vec<View>,
    /// Sorted by archive index (the report's order).
    groups: Vec<GroupRec>,
    group_by_index: HashMap<i64, usize>,
    definitions: Vec<DefinitionRec>,
    /// Block member UUID → UUIDs of the definitions listing it.
    member_definitions: HashMap<String, Vec<String>>,
    lights: usize,
    light_list: Vec<crate::report::Light>,
    earth_anchor: Option<crate::report::EarthAnchor>,
    unparsed: usize,
}

/// Millimetres per model unit assumed for geometry when the file has no unit system.
pub(crate) const ASSUMED_MM_PER_UNIT: f64 = 1.0;

impl Tables {
    /// Millimetres per model unit; `None` when the file has no unit system.
    #[cfg(all(test, feature = "cadmpeg"))]
    pub(crate) fn mm_per_unit(&self) -> Option<f64> {
        self.file.mm_per_unit
    }

    /// The scale geometry is read with: the file's, or [`ASSUMED_MM_PER_UNIT`] when it has no
    /// unit system (the report keeps the unit system as `none` and says so).
    pub(crate) fn geometry_mm_per_unit(&self) -> f64 {
        self.file.mm_per_unit.unwrap_or(ASSUMED_MM_PER_UNIT)
    }

    pub(crate) fn from_document(doc: Document) -> Self {
        let file = FileBase {
            archive_version: doc.archive_version,
            application: doc.application,
            application_details: doc.application_details,
            unit_system: unit_name(doc.unit_system.as_deref()),
            metres_per_unit: doc.mm_per_unit.map(|mm| mm / 1000.0),
            mm_per_unit: doc.mm_per_unit,
            absolute_tolerance_m: doc.absolute_tolerance_mm.map(|mm| mm / 1000.0),
        };
        let unparsed = doc.unparsed;

        // Layers and materials are referenced by archive index.
        let mut layers = doc.layers;
        layers.sort_by_key(|l| l.archive_index.unwrap_or(i64::MAX));
        let layer_by_index: HashMap<i64, usize> = layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| Some((l.archive_index?, i)))
            .collect();
        let layer_by_uuid: HashMap<String, usize> = layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| Some((l.source_uuid.clone()?, i)))
            .collect();

        let mut materials = doc.materials;
        materials.sort_by_key(|m| m.archive_index.unwrap_or(i64::MAX));
        let material_by_index: HashMap<i64, usize> = materials
            .iter()
            .enumerate()
            .filter_map(|(i, m)| Some((m.archive_index?, i)))
            .collect();
        let layer_material = layers
            .iter()
            .map(|l| {
                l.material_index
                    .filter(|i| *i >= 0)
                    .and_then(|i| material_by_index.get(&i).copied())
            })
            .collect();

        let layer_paths: Vec<String> = (0..layers.len())
            .map(|i| layer_path(&layers, &layer_by_uuid, i))
            .collect();
        let glazing_layers = layer_paths.iter().map(|p| is_glazing(p)).collect();
        let site_layers = layer_paths
            .iter()
            .map(|p| {
                let p = p.to_lowercase();
                SITE_WORDS.iter().any(|w| p.contains(w))
            })
            .collect();
        let layer_visible: Vec<bool> = (0..layers.len())
            .map(|i| layer_visible(&layers, &layer_by_uuid, i))
            .collect();
        let glazing_materials = materials
            .iter()
            .map(|m| m.name.as_deref().is_some_and(is_glazing))
            .collect();

        let mut views: Vec<(ViewKind, i64, View)> = doc.views.iter().filter_map(view).collect();
        views.sort_by_key(|(kind, index, _)| (*kind == ViewKind::Named, *index));

        let mut groups = doc.groups;
        groups.sort_by_key(|g| g.archive_index.unwrap_or(-1));
        let group_by_index = groups
            .iter()
            .enumerate()
            .filter_map(|(i, g)| Some((g.archive_index?, i)))
            .collect();

        let definitions = doc.definitions;
        let mut member_definitions: HashMap<String, Vec<String>> = HashMap::new();
        for d in &definitions {
            let Some(uuid) = &d.source_uuid else { continue };
            for member in &d.member_object_ids {
                member_definitions
                    .entry(member.clone())
                    .or_default()
                    .push(uuid.clone());
            }
        }

        // Lights in metres; on when switched on, not hidden and on a visible layer.
        let metres = doc.mm_per_unit.unwrap_or(ASSUMED_MM_PER_UNIT) / 1000.0;
        let light_list = doc
            .light_list
            .iter()
            .map(|l| {
                let layer = layer_by_index.get(&i64::from(l.layer)).copied();
                light_report(
                    l,
                    metres,
                    layer.map(|i| (layer_paths[i].clone(), layer_visible[i])),
                )
            })
            .collect();
        let earth_anchor = doc.earth_anchor.map(|a| crate::report::EarthAnchor {
            latitude: round(a.latitude, 6),
            longitude: round(a.longitude, 6),
            elevation_m: round(a.elevation_m, 3),
            north: a.north.map(|v| round(v, 6)),
            east: a.east.map(|v| round(v, 6)),
        });

        Self {
            file,
            layers,
            layer_by_index,
            layer_by_uuid,
            layer_paths,
            layer_material,
            layer_visible,
            glazing_layers,
            site_layers,
            materials,
            material_by_index,
            glazing_materials,
            views: views.into_iter().map(|(_, _, v)| v).collect(),
            groups,
            group_by_index,
            definitions,
            member_definitions,
            lights: doc.lights,
            light_list,
            earth_anchor,
            unparsed,
        }
    }

    /// Whether the file has block definitions (and so needs placements before member meshes).
    pub(crate) fn has_blocks(&self) -> bool {
        !self.definitions.is_empty()
    }

    pub(crate) fn document_info(&self) -> DocumentInfo {
        let parse = |hex: Option<String>| {
            let hex = hex?;
            let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).ok()?;
            Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
        };
        DocumentInfo {
            views: self.views.clone(),
            layers: self
                .layers
                .iter()
                .enumerate()
                .map(|(i, l)| LayerInfo {
                    path: self.layer_paths[i].clone(),
                    color: parse(l.color.as_deref().and_then(hex_color)),
                })
                .collect(),
            materials: self
                .materials
                .iter()
                .map(|m| MaterialInfo {
                    name: m.name.clone().unwrap_or_default(),
                    color: parse(
                        m.physically_based
                            .as_ref()
                            .and_then(|p| p.base_color.as_deref())
                            .and_then(rgb_color)
                            .or_else(|| m.diffuse.as_deref().and_then(hex_color)),
                    ),
                })
                .collect(),
        }
    }

    /// Report indices (`structure.groups`) of an object's groups, as the file lists them.
    fn group_positions(&self, groups: &[i64]) -> Vec<usize> {
        groups
            .iter()
            .filter_map(|g| self.group_by_index.get(g).copied())
            .collect()
    }

    fn definition_name(&self, uuid: &str) -> Option<String> {
        self.definitions
            .iter()
            .find(|d| d.source_uuid.as_deref() == Some(uuid))
            .and_then(|d| d.name.clone())
    }
}

/// Object attributes resolved against layers and materials.
struct Object {
    uuid: Option<String>,
    name: Option<String>,
    layer: Option<usize>,
    material: Option<usize>,
    /// Material comes from the block instance that places the object.
    material_by_parent: bool,
    visible: bool,
    member: bool,
}

/// One displayable mesh; positions in millimetres, unplaced.
struct MeshData<'a> {
    positions_mm: &'a [[f64; 3]],
    triangles: &'a [[u32; 3]],
    vertex_count: usize,
    triangle_count: usize,
    attrs: MeshAttrs,
}

/// Attributes of a mesh, resolved from its object.
#[derive(Clone, Copy)]
struct MeshAttrs {
    visible: bool,
    /// Display colour given with the geometry.
    color: Option<[u8; 3]>,
    layer: Option<usize>,
    material: Option<usize>,
    /// Material comes from the block instance that places the object.
    material_by_parent: bool,
    /// Placed by an instance on a site layer (the mesh's own layer is checked separately).
    site: bool,
}

/// A block instance as read from the file.
struct Instance {
    uuid: Option<String>,
    name: Option<String>,
    user_strings: Vec<(String, String)>,
    /// Report indices of its groups.
    groups: Vec<usize>,
    layer: Option<usize>,
    definition: String,
    matrix: Matrix,
    visible: bool,
    /// On a site layer: its contents count as site geometry.
    site: bool,
    material: Option<usize>,
    material_by_parent: bool,
    /// Set when the instance is itself a block member (a nested block).
    member_uuid: Option<String>,
}

/// One model-space placement of a definition, nested chains resolved.
#[derive(Clone, Copy)]
struct Placed {
    /// Index of the model-space instance it comes from (in `Facts::instances`).
    owner: usize,
    matrix: Matrix,
    visible: bool,
    site: bool,
    material: Option<usize>,
}

/// Object facts accumulated over one or more decodes. Memory is bounded by aggregates and the
/// number of block placements, not by the number or size of objects.
pub(crate) struct Facts {
    inventory: Inventory,
    undecoded_members: usize,
    rejected_records: usize,
    layer_objects: Vec<usize>,
    material_objects: Vec<usize>,
    group_objects: HashMap<i64, usize>,
    names: BTreeSet<String>,
    user_strings: BTreeMap<String, (usize, BTreeSet<String>)>,
    acc: Accumulator,
    faces: usize,
    planar_faces: usize,
    retained: BTreeMap<String, usize>,
    unparsed: usize,
    /// Layer index (or `None`) → Breps on it without a render mesh.
    breps_without_mesh: BTreeMap<Option<usize>, usize>,
    placements_per_definition: HashMap<String, usize>,
    occurrences: usize,
    instances: Vec<Instance>,
    /// Definition UUID → its model-space placements (filled by `prepare_placements`).
    placed: HashMap<String, Vec<Placed>>,
    /// Definition UUID → (enclosing definition, transform into it), for definition sizes.
    enclosing: HashMap<String, Vec<(String, Matrix)>>,
    /// Definition UUID → bounds of its geometry in definition space, metres.
    block_bounds: HashMap<String, Bounds>,
    /// Placements left out once `MAX_EXPANDED_PLACEMENTS` was reached.
    skipped_placements: usize,
    /// Elements grouped by layer, material and block definition.
    grouped: BTreeMap<GroupKey, ElementAcc>,
    /// Per-object elements, until there are more than `MAX_OBJECT_ELEMENTS`.
    listed: BTreeMap<ListedKey, Listed>,
    listed_overflow: bool,
    /// Receives visible meshes as they are added.
    sink: Option<Box<dyn MeshSink>>,
    /// Top-level instances (indices in `instances`) already described to the sink.
    picked: HashSet<usize>,
}

/// Layer, material and block definition of a grouped element.
type GroupKey = (Option<usize>, Option<usize>, Option<String>);

/// Identity of a listed element: a model-space object (by UUID, or record position when it
/// has none) or a model-space block instance (by its index in `Facts::instances`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ListedKey {
    Object(String),
    Placement(usize),
}

#[derive(Default)]
struct ElementAcc {
    count: usize,
    stats: MeshStats,
    bounds: Option<Bounds>,
}

impl ElementAcc {
    fn add(&mut self, stats: MeshStats, bounds: Bounds) {
        self.stats.add(stats);
        match &mut self.bounds {
            Some(b) => {
                b.extend(bounds.min);
                b.extend(bounds.max);
            }
            None => self.bounds = Some(bounds),
        }
    }
}

struct Listed {
    name: Option<String>,
    layer: Option<usize>,
    material: Option<usize>,
    block: Option<String>,
    acc: ElementAcc,
}

impl Facts {
    pub(crate) fn new(tables: &Tables) -> Self {
        Self {
            inventory: Inventory {
                lights: tables.lights,
                ..Default::default()
            },
            undecoded_members: 0,
            rejected_records: 0,
            layer_objects: vec![0; tables.layers.len()],
            material_objects: vec![0; tables.materials.len()],
            group_objects: HashMap::new(),
            names: BTreeSet::new(),
            user_strings: BTreeMap::new(),
            acc: Accumulator::default(),
            faces: 0,
            planar_faces: 0,
            retained: BTreeMap::new(),
            unparsed: 0,
            breps_without_mesh: BTreeMap::new(),
            placements_per_definition: HashMap::new(),
            occurrences: 0,
            instances: Vec::new(),
            placed: HashMap::new(),
            enclosing: HashMap::new(),
            block_bounds: HashMap::new(),
            skipped_placements: 0,
            grouped: BTreeMap::new(),
            listed: BTreeMap::new(),
            listed_overflow: false,
            sink: None,
            picked: HashSet::new(),
        }
    }

    /// Hands visible meshes to `sink` from now on, after telling it about the document.
    pub(crate) fn set_sink(&mut self, tables: &Tables, mut sink: Box<dyn MeshSink>) {
        sink.document(&tables.document_info());
        self.sink = Some(sink);
    }

    /// Adds per-object figures to a listed element, giving up the listing past the limit.
    fn list(
        &mut self,
        key: ListedKey,
        make: impl FnOnce() -> Listed,
        stats: MeshStats,
        bounds: Bounds,
    ) {
        if self.listed_overflow {
            return;
        }
        if !self.listed.contains_key(&key) && self.listed.len() >= MAX_OBJECT_ELEMENTS {
            self.listed_overflow = true;
            self.listed.clear();
            return;
        }
        let entry = self.listed.entry(key).or_insert_with(make);
        entry.acc.add(stats, bounds);
    }

    pub(crate) fn add_rejected_records(&mut self, n: usize) {
        self.rejected_records += n;
    }

    /// Counts one object record left undecoded, by class name.
    #[cfg_attr(feature = "cadmpeg", allow(dead_code))]
    pub(crate) fn add_retained(&mut self, class: &str) {
        *self.retained.entry(class.to_string()).or_default() += 1;
    }

    fn count_occurrence(&mut self, definition: Option<&str>) {
        self.occurrences += 1;
        if let Some(uuid) = definition {
            *self
                .placements_per_definition
                .entry(uuid.to_string())
                .or_default() += 1;
        }
    }

    /// Adds one object, whichever decoder read it.
    pub(crate) fn absorb_object(&mut self, tables: &Tables, view: &ObjectView, mode: Mode) {
        let layer = view
            .layer_index
            .and_then(|i| tables.layer_by_index.get(&i).copied());
        let material_by_parent = view.material_source == Some(MATERIAL_FROM_PARENT);
        let material = match view.material_source {
            Some(MATERIAL_FROM_OBJECT) => view
                .material_index
                .filter(|i| *i >= 0)
                .and_then(|i| tables.material_by_index.get(&i).copied()),
            Some(MATERIAL_FROM_PARENT) => None,
            _ => layer.and_then(|l| tables.layer_material[l]),
        };
        let layer_visible = layer.is_none_or(|l| tables.layer_visible[l]);
        let name = view
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let object = Object {
            uuid: view.uuid.clone(),
            name: name.map(str::to_string),
            layer,
            material,
            material_by_parent,
            visible: view.visible.unwrap_or(true) && layer_visible,
            member: view.mode.is_some_and(|m| m & 0x0f == MODE_IDEF_OBJECT)
                || view
                    .uuid
                    .as_ref()
                    .is_some_and(|u| tables.member_definitions.contains_key(u)),
        };

        if object.member {
            self.inventory.block_members += 1;
            // The whole-file decode never decodes members at top level.
            if mode == Mode::Batch && view.kind == Counted::Object(ObjectKind::Unlinked) {
                self.undecoded_members += 1;
            }
        } else {
            count(&mut self.inventory, view.kind);
            if view.kind == Counted::Object(ObjectKind::Brep { render_mesh: false }) {
                self.inventory.breps_tessellated += usize::from(view.generated_mesh);
                if view.meshes.is_empty() || view.undisplayed_faces > 0 {
                    *self.breps_without_mesh.entry(layer).or_default() += 1;
                }
            }
            if let Some(l) = layer {
                self.layer_objects[l] += 1;
            }
            if let Some(m) = material {
                self.material_objects[m] += 1;
            }
            for g in &view.groups {
                *self.group_objects.entry(*g).or_default() += 1;
            }
            if let Some(name) = name {
                insert_bounded(&mut self.names, name, MAX_LISTED);
            }
            for (key, value) in &view.user_strings {
                if key.is_empty() {
                    continue;
                }
                if !self.user_strings.contains_key(key) {
                    if self.user_strings.len() >= MAX_USER_STRING_KEYS {
                        if self
                            .user_strings
                            .last_key_value()
                            .is_some_and(|(last, _)| key >= last)
                        {
                            continue;
                        }
                        self.user_strings.pop_last();
                    }
                    self.user_strings.insert(key.clone(), Default::default());
                }
                let entry = self.user_strings.get_mut(key).expect("present");
                entry.0 += 1;
                if !value.is_empty() {
                    insert_bounded(&mut entry.1, value, MAX_USER_STRING_VALUES);
                }
            }
            self.faces += view.faces;
            self.planar_faces += view.planar_faces;
        }

        if let Some(instance) = &view.instance {
            self.count_occurrence(Some(&instance.definition));
            if let Some(matrix) = instance.matrix {
                self.instances.push(Instance {
                    uuid: object.uuid.clone(),
                    name: object.name.clone(),
                    user_strings: view.user_strings.clone(),
                    groups: tables.group_positions(&view.groups),
                    layer: object.layer,
                    definition: instance.definition.clone(),
                    matrix,
                    visible: object.visible,
                    site: object.layer.is_some_and(|l| tables.site_layers[l]),
                    material: object.material,
                    material_by_parent: object.material_by_parent,
                    member_uuid: object.member.then(|| object.uuid.clone()).flatten(),
                });
            }
        }

        // Displayable meshes: model-space objects directly, block members once per placement.
        let attrs = MeshAttrs {
            visible: object.visible,
            color: None,
            layer: object.layer,
            material: object.material,
            material_by_parent: object.material_by_parent,
            site: false,
        };
        let id = object_id(&[object
            .uuid
            .as_deref()
            .unwrap_or(&view.fallback_key)
            .as_bytes()]);
        let mut element = ElementAcc::default();
        for geom in &view.meshes {
            let mesh = mesh_data(geom, attrs);
            if object.member && !geom.in_instance {
                if let Some(uuid) = &object.uuid {
                    self.place_member(tables, uuid, &mesh);
                }
                continue;
            }
            if let Some((stats, bounds)) = add_mesh(
                &mut self.acc,
                &mut self.inventory,
                &mut self.sink,
                tables,
                &mesh,
                &mesh.attrs,
                None,
                id,
                id,
            ) {
                element.add(stats, bounds);
            }
        }
        // Curves and points are drawn; the report's areas and sizes stay those of the meshes.
        let mut wired = false;
        if !view.curves.is_empty() || !view.points.is_empty() {
            if object.member {
                if let Some(uuid) = &object.uuid {
                    self.place_member_wires(tables, uuid, &view.curves, &view.points, attrs);
                }
            } else if object.visible
                && let Some(sink) = &mut self.sink
            {
                let metres =
                    |ps: &[[f64; 3]]| ps.iter().map(|p| p.map(|c| c / 1000.0)).collect::<Vec<_>>();
                for (i, ps) in view.curves.iter().chain([&view.points]).enumerate() {
                    if ps.is_empty() {
                        continue;
                    }
                    let positions = metres(ps);
                    let wire = PlacedWire {
                        positions: &positions,
                        object: id,
                        pick: id,
                        layer: object.layer,
                        material: object.material,
                    };
                    if i < view.curves.len() {
                        sink.curve(&wire);
                    } else {
                        sink.points(&wire);
                    }
                }
                wired = true;
            }
        }
        if (element.bounds.is_some() || wired)
            && let Some(sink) = &mut self.sink
        {
            sink.pick(&Pick {
                pick: id,
                uuid: object.uuid.as_deref(),
                name: object.name.as_deref(),
                kind: pick_kind(view.kind),
                layer: object.layer,
                material: object.material,
                block: view.type_name.as_deref(),
                class: view.class.as_deref(),
                groups: &tables.group_positions(&view.groups),
                user_strings: &view.user_strings,
            });
        }
        if let Some(bounds) = element.bounds {
            let key = (object.layer, object.material, None);
            let group = self.grouped.entry(key.clone()).or_default();
            group.count += 1;
            group.add(element.stats, bounds);
            let listed_key = ListedKey::Object(
                object
                    .uuid
                    .clone()
                    .unwrap_or_else(|| view.fallback_key.clone()),
            );
            self.list(
                listed_key,
                || Listed {
                    name: object.name.clone(),
                    layer: key.0,
                    material: key.1,
                    block: None,
                    acc: ElementAcc::default(),
                },
                element.stats,
                bounds,
            );
        }
    }

    /// Resolves every definition's model-space placements (nested blocks included) from the
    /// instances absorbed so far. Called once all instances are known, before member meshes.
    pub(crate) fn prepare_placements(&mut self, tables: &Tables) {
        // Instances arrive in an order that depends on batching; sort for a stable result.
        self.instances.sort_by(|a, b| {
            (&a.member_uuid, &a.uuid, &a.definition).cmp(&(&b.member_uuid, &b.uuid, &b.definition))
        });
        let members: HashMap<&str, &[String]> = tables
            .definitions
            .iter()
            .filter_map(|d| Some((d.source_uuid.as_deref()?, d.member_object_ids.as_slice())))
            .collect();
        let nested: HashMap<&str, &Instance> = self
            .instances
            .iter()
            .filter_map(|i| Some((i.member_uuid.as_deref()?, i)))
            .collect();

        let mut placed: HashMap<String, Vec<Placed>> = HashMap::new();
        let mut total = 0usize;
        for (owner, top) in self
            .instances
            .iter()
            .enumerate()
            .filter(|(_, i)| i.member_uuid.is_none())
        {
            let mut stack = vec![(
                top.definition.as_str(),
                top.matrix,
                top.visible,
                top.site,
                top.material,
                0,
            )];
            while let Some((definition, matrix, visible, site, material, depth)) = stack.pop() {
                if total >= MAX_EXPANDED_PLACEMENTS {
                    self.skipped_placements += 1;
                    continue;
                }
                total += 1;
                placed
                    .entry(definition.to_string())
                    .or_default()
                    .push(Placed {
                        owner,
                        matrix,
                        visible,
                        site,
                        material,
                    });
                if depth >= MAX_BLOCK_DEPTH {
                    continue;
                }
                for member in members.get(definition).copied().unwrap_or_default() {
                    if let Some(n) = nested.get(member.as_str()) {
                        let material = if n.material_by_parent {
                            material
                        } else {
                            n.material
                        };
                        stack.push((
                            n.definition.as_str(),
                            multiply(&matrix, &n.matrix),
                            visible && n.visible,
                            site || n.site,
                            material,
                            depth + 1,
                        ));
                    }
                }
            }
        }

        // Nested definitions inside each definition, under the same global cap as the
        // placements (definitions that contain themselves would otherwise multiply).
        let mut enclosing: HashMap<String, Vec<(String, Matrix)>> = HashMap::new();
        let mut nestings = 0usize;
        for d in &tables.definitions {
            let Some(outer) = d.source_uuid.as_deref() else {
                continue;
            };
            let mut stack: Vec<(&str, Matrix, usize)> = vec![(outer, crate::native::IDENTITY, 0)];
            while let Some((definition, matrix, depth)) = stack.pop() {
                if depth >= MAX_BLOCK_DEPTH || nestings >= MAX_EXPANDED_PLACEMENTS {
                    continue;
                }
                for member in members.get(definition).copied().unwrap_or_default() {
                    if let Some(n) = nested.get(member.as_str()) {
                        let m = multiply(&matrix, &n.matrix);
                        nestings += 1;
                        enclosing
                            .entry(n.definition.clone())
                            .or_default()
                            .push((outer.to_string(), m));
                        stack.push((n.definition.as_str(), m, depth + 1));
                    }
                }
            }
        }
        self.placed = placed;
        self.enclosing = enclosing;
    }

    /// Adds a block member's mesh once per placement of each definition listing it, and to the
    /// definitions' sizes.
    fn place_member(&mut self, tables: &Tables, uuid: &str, mesh: &MeshData<'_>) {
        for definition in tables.member_definitions.get(uuid).into_iter().flatten() {
            for p in mesh.positions_mm {
                let p = p.map(|c| c / 1000.0);
                extend_bounds(&mut self.block_bounds, definition, p);
            }
            for (outer, m) in self.enclosing.get(definition).into_iter().flatten() {
                for p in mesh.positions_mm {
                    extend_bounds(
                        &mut self.block_bounds,
                        outer,
                        apply(m, *p).map(|c| c / 1000.0),
                    );
                }
            }
            let block_name = tables.definition_name(definition);
            let placements = self.placed.get(definition).map_or(0, Vec::len);
            // How many placements of this definition each top-level instance has had so far:
            // with the instance's UUID it identifies the placement stably across saves (the
            // instance's position in the file is not stable).
            let mut occurrences: HashMap<usize, u32> = HashMap::new();
            for k in 0..placements {
                let placement = &self.placed[definition][k];
                let occurrence = {
                    let n = occurrences.entry(placement.owner).or_insert(0);
                    *n += 1;
                    *n - 1
                };
                let attrs = MeshAttrs {
                    visible: mesh.attrs.visible && placement.visible,
                    site: mesh.attrs.site || placement.site,
                    material: if mesh.attrs.material_by_parent {
                        placement.material
                    } else {
                        mesh.attrs.material
                    },
                    ..mesh.attrs
                };
                let (owner, matrix) = (placement.owner, placement.matrix);
                let (id, pick) = self.member_ids(owner, definition, occurrence, uuid);
                let Some((stats, bounds)) = add_mesh(
                    &mut self.acc,
                    &mut self.inventory,
                    &mut self.sink,
                    tables,
                    mesh,
                    &attrs,
                    Some(&matrix),
                    id,
                    pick,
                ) else {
                    continue;
                };
                self.pick_instance(tables, owner, pick);
                self.grouped
                    .entry((attrs.layer, attrs.material, block_name.clone()))
                    .or_default()
                    .add(stats, bounds);
                let instance = &self.instances[owner];
                let make = || Listed {
                    name: None,
                    layer: instance.layer,
                    material: instance.material,
                    block: tables.definition_name(&instance.definition),
                    acc: ElementAcc::default(),
                };
                let listed = make();
                self.list(ListedKey::Placement(owner), || listed, stats, bounds);
            }
        }
    }
}

impl Facts {
    /// Describes the top-level block instance `owner` to the sink, once.
    fn pick_instance(&mut self, tables: &Tables, owner: usize, pick: u64) {
        if let Some(sink) = &mut self.sink
            && self.picked.insert(owner)
        {
            let instance = &self.instances[owner];
            let block = tables.definition_name(&instance.definition);
            sink.pick(&Pick {
                pick,
                uuid: instance.uuid.as_deref(),
                name: instance.name.as_deref(),
                kind: "block",
                layer: instance.layer,
                material: instance.material,
                block: block.as_deref(),
                class: None,
                groups: &instance.groups,
                user_strings: &instance.user_strings,
            });
        }
    }

    /// The object and pick ids of a block member's `occurrence`-th placement by instance
    /// `owner` of `definition`: the same for its meshes, curves and points.
    fn member_ids(
        &self,
        owner: usize,
        definition: &str,
        occurrence: u32,
        uuid: &str,
    ) -> (u64, u64) {
        let owner_key = match &self.instances[owner].uuid {
            Some(owner_uuid) => owner_uuid.as_bytes().to_vec(),
            None => format!("instance {owner}").into_bytes(),
        };
        let id = object_id(&[
            &owner_key,
            definition.as_bytes(),
            &occurrence.to_le_bytes(),
            uuid.as_bytes(),
        ]);
        (id, object_id(&[&owner_key]))
    }

    /// Draws a block member's curves and points once per placement of each definition listing
    /// it.
    fn place_member_wires(
        &mut self,
        tables: &Tables,
        uuid: &str,
        curves: &[Vec<[f64; 3]>],
        points: &[[f64; 3]],
        attrs: MeshAttrs,
    ) {
        if self.sink.is_none() {
            return;
        }
        for definition in tables.member_definitions.get(uuid).into_iter().flatten() {
            let placements: Vec<Placed> = self.placed.get(definition).cloned().unwrap_or_default();
            let mut occurrences: HashMap<usize, u32> = HashMap::new();
            for placement in placements {
                let occurrence = {
                    let n = occurrences.entry(placement.owner).or_insert(0);
                    *n += 1;
                    *n - 1
                };
                if !(attrs.visible && placement.visible) {
                    continue;
                }
                let (id, pick) = self.member_ids(placement.owner, definition, occurrence, uuid);
                let material = if attrs.material_by_parent {
                    placement.material
                } else {
                    attrs.material
                };
                let place = |ps: &[[f64; 3]]| {
                    ps.iter()
                        .map(|p| apply(&placement.matrix, *p).map(|c| c / 1000.0))
                        .collect::<Vec<_>>()
                };
                let sink = self.sink.as_mut().expect("checked above");
                for (i, ps) in curves.iter().map(Vec::as_slice).chain([points]).enumerate() {
                    if ps.is_empty() {
                        continue;
                    }
                    let positions = place(ps);
                    let wire = PlacedWire {
                        positions: &positions,
                        object: id,
                        pick,
                        layer: attrs.layer,
                        material,
                    };
                    if i < curves.len() {
                        sink.curve(&wire);
                    } else {
                        sink.points(&wire);
                    }
                }
                self.pick_instance(tables, placement.owner, pick);
            }
        }
    }
}

/// The kind of a selectable object, as [`Pick::kind`] names it.
fn pick_kind(kind: Counted) -> &'static str {
    match kind {
        Counted::Object(ObjectKind::Brep { .. }) => "brep",
        Counted::Object(ObjectKind::Mesh) => "mesh",
        Counted::Object(ObjectKind::Subd) => "subd",
        Counted::BlockInstance => "block",
        Counted::Object(_) => "other",
    }
}

fn extend_bounds(map: &mut HashMap<String, Bounds>, key: &str, p: [f64; 3]) {
    match map.get_mut(key) {
        Some(b) => b.extend(p),
        None => {
            map.insert(key.to_string(), Bounds::point(p));
        }
    }
}

/// Rhino's sun and ground plane as report values (the ground's height in metres).
pub(crate) fn render_report(
    tables: &Tables,
    sun: Option<crate::reader::rdk::SunRec>,
    ground: Option<crate::reader::rdk::GroundRec>,
) -> Option<crate::report::RenderSettings> {
    if sun.is_none() && ground.is_none() {
        return None;
    }
    let metres = tables
        .file
        .metres_per_unit
        .unwrap_or(ASSUMED_MM_PER_UNIT / 1000.0);
    Some(crate::report::RenderSettings {
        sun: sun.map(|s| crate::report::Sun {
            enabled: s.enabled,
            manual: s.manual,
            azimuth: round(s.azimuth, 3),
            altitude: round(s.altitude, 3),
            date: format!("{:04}-{:02}-{:02}", s.year, s.month, s.day),
            hours: round(s.hours, 6),
            utc_offset: round(s.timezone, 6),
            daylight_saving_minutes: s.daylight_saving,
            latitude: s.latitude.map(|v| round(v, 6)),
            longitude: s.longitude.map(|v| round(v, 6)),
            north: s.north.map(|v| round(v, 6)),
            intensity: round(s.intensity, 6),
            skylight: s.skylight,
        }),
        ground_plane: ground.map(|g| crate::report::GroundPlane {
            enabled: g.enabled,
            altitude_m: round(g.altitude * metres, 6),
            auto_altitude: g.auto_altitude,
            shadow_only: g.shadow_only,
            show_underside: g.show_underside,
        }),
    })
}

/// Merges tables and facts into the report.
pub(crate) fn finalize(tables: &Tables, facts: Facts, size_bytes: u64) -> Report {
    let acc = &facts.acc;
    let file = FileInfo {
        size_bytes,
        archive_version: tables.file.archive_version,
        application: tables.file.application.clone(),
        application_details: tables.file.application_details.clone(),
        unit_system: tables.file.unit_system.clone(),
        metres_per_unit: tables.file.metres_per_unit,
        absolute_tolerance_m: tables.file.absolute_tolerance_m,
        earth_anchor: tables.earth_anchor.clone(),
    };

    let layers = tables
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| Layer {
            index: l.archive_index.unwrap_or(-1),
            name: l.name.clone().unwrap_or_default(),
            path: tables.layer_paths[i].clone(),
            color: l.color.as_deref().and_then(hex_color),
            visible: l.visible.unwrap_or(true),
            locked: l.locked.unwrap_or(false),
            material: tables.layer_material[i].and_then(|m| tables.materials[m].name.clone()),
            objects: facts.layer_objects[i],
            mesh_area_m2: round(acc.layer_area(i), 6),
            bbox: acc.layer_bounds.get(&i).map(bbox),
        })
        .collect();

    let groups = tables
        .groups
        .iter()
        .map(|g| Group {
            name: g.name.clone().unwrap_or_default(),
            objects: facts
                .group_objects
                .get(&g.archive_index.unwrap_or(-1))
                .copied()
                .unwrap_or(0),
        })
        .collect();

    let structure = Structure {
        layers,
        groups,
        object_names: facts.names.iter().cloned().collect(),
        user_strings: facts
            .user_strings
            .iter()
            .map(|(key, (objects, values))| UserStringKey {
                key: key.clone(),
                objects: *objects,
                values: values.iter().cloned().collect(),
            })
            .collect(),
    };

    let mut blocks: Vec<Block> = tables
        .definitions
        .iter()
        .map(|d| {
            let uuid = d.source_uuid.as_deref().unwrap_or_default();
            Block {
                name: d.name.clone().unwrap_or_default(),
                placements: facts
                    .placements_per_definition
                    .get(uuid)
                    .copied()
                    .unwrap_or(0),
                members: d.member_object_ids.len(),
                size: facts
                    .block_bounds
                    .get(uuid)
                    .map(|b| b.size().map(|v| round(v, 6))),
            }
        })
        .collect();
    blocks.sort_by(|a, b| a.name.cmp(&b.name));

    let mut layer_material_counts = vec![0usize; tables.materials.len()];
    for &m in tables.layer_material.iter().flatten() {
        layer_material_counts[m] += 1;
    }
    let materials = tables
        .materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let pbr = m.physically_based.as_ref();
            Material {
                index: m.archive_index.unwrap_or(-1),
                name: m.name.clone().unwrap_or_default(),
                color: pbr
                    .and_then(|p| p.base_color.as_deref())
                    .and_then(rgb_color)
                    .or_else(|| m.diffuse.as_deref().and_then(hex_color)),
                physically_based: pbr.is_some(),
                metallic: pbr.and_then(|p| p.metallic).map(|v| round(v, 4)),
                roughness: pbr.and_then(|p| p.roughness).map(|v| round(v, 4)),
                opacity: pbr.and_then(|p| p.opacity).map(|v| round(v, 4)),
                textures: m
                    .textures
                    .iter()
                    .filter_map(|t| {
                        let f = t.file_reference.as_ref()?;
                        let path = f
                            .full_path
                            .as_deref()
                            .filter(|p| !p.is_empty())
                            .or(f.relative_path.as_deref())?;
                        path.rsplit(['/', '\\'])
                            .next()
                            .filter(|n| !n.is_empty())
                            .map(str::to_string)
                    })
                    .collect(),
                layers: layer_material_counts[i],
                objects: facts.material_objects[i],
                mesh_area_m2: round(acc.material_area(i), 6),
            }
        })
        .collect();

    let proportions = acc.bounds.map(|b| {
        let size = b.size();
        let (long, short) = (size[0].max(size[1]), size[0].min(size[1]));
        Proportions {
            bbox: bbox(&b),
            size: size.map(|v| round(v, 6)),
            plan_aspect: (short > 0.0).then(|| round(long / short, 4)),
            height_to_plan: (long > 0.0).then(|| round(size[2] / long, 4)),
            storeys: acc.storeys().map(|(count, height, levels)| Storeys {
                count,
                typical_height: round(height, 4),
                levels: levels.into_iter().map(|z| round(z, 4)).collect(),
            }),
            levels: acc
                .levels(MAX_LEVELS)
                .into_iter()
                .map(|(z, area)| Level {
                    z: round(z, 4),
                    area_m2: round(area, 6),
                })
                .collect(),
        }
    });
    let total_area = acc.total_area();
    let share = |part: f64| (total_area > 0.0).then(|| round(part / total_area, 4));
    let style = StyleSignals {
        orthogonality: acc.orthogonality().map(|v| round(v, 4)),
        horizontal_area_share: share(acc.horizontal_area()),
        vertical_area_share: share(acc.vertical_area()),
        planar_face_share: (facts.faces > 0)
            .then(|| round(facts.planar_faces as f64 / facts.faces as f64, 4)),
        glazing_area_share: share(acc.glazing_area()),
        repetition: Repetition {
            block_definitions: blocks.len(),
            block_placements: facts.occurrences,
            max_placements_per_block: blocks.iter().map(|b| b.placements).max().unwrap_or(0),
        },
    };

    let elements = elements(tables, &facts);
    let diagnostics = diagnostics(
        tables,
        &file,
        &facts,
        total_area,
        tables.unparsed + facts.unparsed,
    );

    Report {
        schema_version: SCHEMA_VERSION,
        file,
        structure,
        blocks,
        materials,
        views: tables.views.clone(),
        lights: tables.light_list.clone(),
        render: None,
        inventory: facts.inventory.clone(),
        proportions,
        style,
        elements,
        diagnostics,
    }
}

/// The per-object listing when it stayed within its limit, the grouped one otherwise.
fn elements(tables: &Tables, facts: &Facts) -> Elements {
    let layer = |l: Option<usize>| l.map(|l| tables.layer_paths[l].clone());
    let material = |m: Option<usize>| m.and_then(|m| tables.materials[m].name.clone());
    let element = |name, layer, material, block, count, acc: &ElementAcc| {
        let bounds = acc.bounds.expect("elements have geometry");
        let area = float(acc.stats.area);
        let share = |part: i128| {
            if area > 0.0 {
                round(float(part) / area, 4)
            } else {
                0.0
            }
        };
        let (horizontal, vertical) = (share(acc.stats.horizontal), share(acc.stats.vertical));
        Element {
            name,
            layer,
            material,
            block,
            count,
            area_m2: round(area, 6),
            size: bounds.size().map(|v| round(v, 6)),
            base_z: round(bounds.min[2], 4),
            horizontal_share: horizontal,
            vertical_share: vertical,
            sloped_share: round((1.0 - horizontal - vertical).max(0.0), 4),
        }
    };
    let by_reading_order = |a: &Element, b: &Element| {
        (&a.layer, &a.block, &a.name)
            .cmp(&(&b.layer, &b.block, &b.name))
            .then(a.base_z.total_cmp(&b.base_z))
            .then(b.area_m2.total_cmp(&a.area_m2))
            .then(
                a.size
                    .partial_cmp(&b.size)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    };
    if !facts.listed_overflow {
        let mut items: Vec<Element> = facts
            .listed
            .values()
            .map(|l| {
                element(
                    l.name.clone(),
                    layer(l.layer),
                    material(l.material),
                    l.block.clone(),
                    1,
                    &l.acc,
                )
            })
            .collect();
        items.sort_by(by_reading_order);
        return Elements {
            per_object: true,
            items,
        };
    }
    let mut items: Vec<Element> = facts
        .grouped
        .iter()
        .map(|((l, m, block), acc)| {
            let count = match block {
                Some(name) => tables
                    .definitions
                    .iter()
                    .filter(|d| d.name.as_deref() == Some(name.as_str()))
                    .filter_map(|d| d.source_uuid.as_deref())
                    .map(|u| facts.placements_per_definition.get(u).copied().unwrap_or(0))
                    .sum(),
                None => acc.count,
            };
            element(None, layer(*l), material(*m), block.clone(), count, acc)
        })
        .collect();
    items.sort_by(|a, b| {
        b.area_m2
            .total_cmp(&a.area_m2)
            .then_with(|| by_reading_order(a, b))
    });
    Elements {
        per_object: false,
        items,
    }
}

/// A mesh with the given attributes, copied from its description.
fn mesh_data(geom: &MeshGeom, attrs: MeshAttrs) -> MeshData<'_> {
    MeshData {
        positions_mm: &geom.positions_mm,
        triangles: &geom.triangles,
        vertex_count: geom.vertex_count,
        triangle_count: geom.triangle_count,
        attrs: MeshAttrs {
            color: geom.color.or(attrs.color),
            ..attrs
        },
    }
}

/// Inserts into a set that keeps only its `max` smallest values (the ones a sorted listing
/// truncated to `max` would show).
fn insert_bounded(set: &mut BTreeSet<String>, value: &str, max: usize) {
    if set.len() >= max && set.last().is_some_and(|last| value >= last.as_str()) {
        return;
    }
    set.insert(value.to_string());
    if set.len() > max {
        set.pop_last();
    }
}

fn is_glazing(name: &str) -> bool {
    let name = name.to_lowercase();
    GLAZING_WORDS.iter().any(|w| name.contains(w))
}

/// Adds one mesh with the given attributes, optionally placed by a block transform
/// (millimetres), to the totals. Hidden meshes count in the inventory but not in areas.
/// Returns the mesh's figures and bounds (metres) when it is visible.
#[allow(clippy::too_many_arguments)]
fn add_mesh(
    acc: &mut Accumulator,
    inventory: &mut Inventory,
    sink: &mut Option<Box<dyn MeshSink>>,
    tables: &Tables,
    geometry: &MeshData<'_>,
    attrs: &MeshAttrs,
    placement: Option<&Matrix>,
    object: u64,
    pick: u64,
) -> Option<(MeshStats, Bounds)> {
    inventory.mesh_vertices += geometry.vertex_count;
    inventory.mesh_triangles += geometry.triangle_count;
    if !attrs.visible {
        return None;
    }
    let positions: Vec<[f64; 3]> = geometry
        .positions_mm
        .iter()
        .map(|&p| placement.map_or(p, |m| apply(m, p)).map(|c| c / 1000.0))
        .collect();
    // A mirroring placement reverses winding; flip it back so up stays up.
    let flipped: Vec<[u32; 3]>;
    let triangles = if placement.is_some_and(|m| determinant3(m) < 0.0) {
        flipped = geometry
            .triangles
            .iter()
            .map(|&[a, b, c]| [a, c, b])
            .collect();
        &flipped[..]
    } else {
        geometry.triangles
    };
    let glazing = match attrs.material {
        Some(m) => tables.glazing_materials[m],
        None => attrs.layer.is_some_and(|l| tables.glazing_layers[l]),
    };
    let mut bounds: Option<Bounds> = None;
    for p in positions.iter().filter(|p| p.iter().all(|c| c.is_finite())) {
        match &mut bounds {
            Some(b) => b.extend(*p),
            None => bounds = Some(Bounds::point(*p)),
        }
    }
    let stats = acc.add(&MeshInput {
        positions: &positions,
        triangles,
        layer: attrs.layer,
        material: attrs.material,
        glazing,
        site: attrs.site || attrs.layer.is_some_and(|l| tables.site_layers[l]),
    });
    if let Some(sink) = sink {
        sink.mesh(&PlacedMesh {
            positions: &positions,
            triangles,
            object,
            pick,
            layer: attrs.layer,
            material: attrs.material,
            color: attrs.color,
        });
    }
    Some((stats, bounds?))
}

fn determinant3(m: &Matrix) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

fn count(inventory: &mut Inventory, kind: Counted) {
    inventory.objects += 1;
    match kind {
        Counted::BlockInstance => inventory.block_instances += 1,
        Counted::Object(kind) => match kind {
            ObjectKind::Brep { render_mesh } => {
                inventory.breps += 1;
                inventory.breps_with_render_mesh += usize::from(render_mesh);
            }
            ObjectKind::Mesh => inventory.meshes += 1,
            ObjectKind::Subd => inventory.subds += 1,
            ObjectKind::Curve => inventory.curves += 1,
            ObjectKind::Point => inventory.points += 1,
            ObjectKind::Annotation => inventory.annotations += 1,
            ObjectKind::Hatch => inventory.hatches += 1,
            ObjectKind::Other => inventory.other += 1,
            ObjectKind::Unlinked => inventory.undecoded += 1,
        },
    }
}

fn unit_name(raw: Option<&str>) -> String {
    match raw {
        None | Some("") => "unset".to_string(),
        Some(raw) => raw
            .strip_prefix("standard:")
            .and_then(|code| code.parse::<usize>().ok())
            .and_then(|code| UNIT_NAMES.get(code))
            .map_or_else(|| raw.to_string(), |name| (*name).to_string()),
    }
}

/// A layer is shown only when it and all its parents are visible.
fn layer_visible(layers: &[LayerRec], by_uuid: &HashMap<String, usize>, index: usize) -> bool {
    let mut current = index;
    for _ in 0..64 {
        if !layers[current].visible.unwrap_or(true) {
            return false;
        }
        match layers[current]
            .parent_uuid
            .as_deref()
            .and_then(|u| by_uuid.get(u))
        {
            Some(&parent) => current = parent,
            None => break,
        }
    }
    true
}

fn layer_path(layers: &[LayerRec], by_uuid: &HashMap<String, usize>, index: usize) -> String {
    let mut parts = vec![layers[index].name.clone().unwrap_or_default()];
    let mut current = index;
    for _ in 0..64 {
        let Some(parent) = layers[current]
            .parent_uuid
            .as_deref()
            .and_then(|u| by_uuid.get(u))
            .copied()
        else {
            break;
        };
        parts.push(layers[parent].name.clone().unwrap_or_default());
        current = parent;
    }
    parts.reverse();
    parts.join("::")
}

fn view(v: &ViewRec) -> Option<(ViewKind, i64, View)> {
    let vp = v.viewport.as_ref()?;
    let camera = vec3(vp.camera_location_mm.as_deref())?;
    let direction = unit(vec3(vp.camera_direction.as_deref())?)?;
    let up = unit(vec3(vp.camera_up.as_deref())?)?;
    let kind = if v.list_kind.as_deref() == Some("named") {
        ViewKind::Named
    } else {
        ViewKind::Viewport
    };
    let projection = match vp.projection {
        Some(1) => Projection::Parallel,
        Some(2) => Projection::Perspective,
        _ => Projection::Other,
    };
    let target =
        vec3(vp.target_millimeters.as_deref()).or_else(|| vec3(v.target_millimeters.as_deref()));
    let (fov_deg, lens_35mm) = match (projection, vp.frustum_mm.as_deref()) {
        (Projection::Perspective, Some(&[left, right, bottom, top, near, _far])) if near > 0.0 => {
            let (half_w, half_h) = ((right - left) / 2.0, (top - bottom) / 2.0);
            let fov = FieldOfView {
                horizontal: round(2.0 * (half_w / near).atan().to_degrees(), 3),
                vertical: round(2.0 * (half_h / near).atan().to_degrees(), 3),
            };
            let lens =
                (half_w.min(half_h) > 0.0).then(|| round(12.0 * near / half_w.min(half_h), 3));
            (Some(fov), lens)
        }
        _ => (None, None),
    };
    let view = View {
        name: v.name.clone().unwrap_or_default(),
        kind,
        projection,
        camera: camera.map(|c| round(c / 1000.0, 6)),
        direction: direction.map(|c| round(c, 6)),
        up: up.map(|c| round(c, 6)),
        target: target.map(|t| t.map(|c| round(c / 1000.0, 6))),
        fov_deg,
        lens_35mm,
        frustum: match vp.frustum_mm.as_deref() {
            Some(&[l, r, b, t, n, f]) if r > l && t > b => {
                Some([l, r, b, t, n, f].map(|c| round(c / 1000.0, 9)))
            }
            _ => None,
        },
        clipping_planes: v
            .attributes
            .iter()
            .flat_map(|a| &a.clipping_planes)
            .filter(|c| c.enabled.unwrap_or(false))
            .filter_map(|c| match c.equation_mm.as_deref() {
                Some(&[a, b, c, d]) => {
                    let len = (a * a + b * b + c * c).sqrt();
                    (len > 0.0 && d.is_finite())
                        .then(|| [a / len, b / len, c / len, d / len / 1000.0].map(|v| round(v, 9)))
                }
                _ => None,
            })
            .collect(),
    };
    Some((kind, v.list_index.unwrap_or(i64::MAX), view))
}

fn diagnostics(
    tables: &Tables,
    file: &FileInfo,
    facts: &Facts,
    mesh_area: f64,
    unparsed: usize,
) -> Vec<Diagnostic> {
    let inventory = &facts.inventory;
    let mut out = Vec::new();
    let mut push = |code, severity, message: String, count: Option<usize>| {
        out.push(Diagnostic {
            code,
            severity,
            message,
            count,
        })
    };
    if matches!(file.unit_system.as_str(), "none" | "unset") {
        push(
            DiagnosticCode::UnknownUnits,
            Severity::Warning,
            "The file has no unit system; lengths assume 1 unit = 1 mm.".to_string(),
            None,
        );
    }
    if file.archive_version == Some(1) {
        push(
            DiagnosticCode::UnsupportedVersion,
            Severity::Info,
            "Rhino 1 file: its geometry, layers and units are read; its saved views are not."
                .to_string(),
            None,
        );
    } else if let Some(version) = file
        .archive_version
        .filter(|v| !SUPPORTED_ARCHIVES.contains(v))
    {
        push(
            DiagnosticCode::UnsupportedVersion,
            Severity::Warning,
            format!("Archive version {version} is only partly supported by the decoder."),
            None,
        );
    }
    let without_mesh: usize = facts.breps_without_mesh.values().sum();
    if without_mesh > 0 {
        let layers: Vec<String> = facts
            .breps_without_mesh
            .iter()
            .map(|(layer, n)| {
                let name = layer.map_or("(no layer)", |l| tables.layer_paths[l].as_str());
                format!("{name} ×{n}")
            })
            .collect();
        push(
            DiagnosticCode::BrepsWithoutRenderMesh,
            Severity::Warning,
            format!(
                "Breps not fully displayed: faces with no cached render mesh could not be tessellated ({}); saving the file from a shaded view, without Save Small, stores Rhino's own meshes.",
                layers.join(", ")
            ),
            Some(without_mesh),
        );
    }
    if inventory.subds > 0 {
        push(
            DiagnosticCode::SubdCageOnly,
            Severity::Info,
            "SubD objects are shown by subdividing their control cage twice (Catmull-Clark), \
             close to the surface Rhino draws."
                .to_string(),
            Some(inventory.subds),
        );
    }
    let undecoded = inventory.undecoded + facts.undecoded_members;
    if undecoded > 0 {
        let detail: Vec<String> = facts
            .retained
            .iter()
            .map(|(name, n)| format!("{name} ×{n}"))
            .collect();
        push(
            DiagnosticCode::UndecodedObjects,
            Severity::Warning,
            format!(
                "Objects retained without decoded geometry (block members included): {}.",
                if detail.is_empty() {
                    "unknown classes".to_string()
                } else {
                    detail.join(", ")
                }
            ),
            Some(undecoded),
        );
    }
    if facts.skipped_placements > 0 {
        push(
            DiagnosticCode::BlockExpansionLimited,
            Severity::Warning,
            format!(
                "Block placements beyond {MAX_EXPANDED_PLACEMENTS} (nested blocks included) were left out; geometry totals are partial."
            ),
            Some(facts.skipped_placements),
        );
    }
    if facts.rejected_records > 0 {
        push(
            DiagnosticCode::RejectedRecords,
            Severity::Warning,
            "Object records the decoder rejected were skipped.".to_string(),
            Some(facts.rejected_records),
        );
    }
    if inventory.objects > 0 && mesh_area <= 0.0 {
        push(
            DiagnosticCode::NoDisplayableGeometry,
            Severity::Warning,
            "No visible object has a displayable mesh.".to_string(),
            None,
        );
    }
    if unparsed > 0 {
        push(
            DiagnosticCode::UnparsedRecords,
            Severity::Warning,
            "Some native records had an unexpected shape and were skipped.".to_string(),
            Some(unparsed),
        );
    }
    out
}

fn vec3(values: Option<&[f64]>) -> Option<[f64; 3]> {
    match values? {
        &[x, y, z, ..] => Some([x, y, z]),
        _ => None,
    }
}

fn unit(v: [f64; 3]) -> Option<[f64; 3]> {
    let len = norm(v);
    (len > 0.0).then(|| v.map(|c| c / len))
}

fn bbox(b: &Bounds) -> BBox {
    BBox {
        min: b.min.map(|v| round(v, 6)),
        max: b.max.map(|v| round(v, 6)),
    }
}

/// A light of the light table as the report gives it (metres, unit direction).
fn light_report(
    l: &crate::reader::document::LightRec,
    metres: f64,
    layer: Option<(String, bool)>,
) -> crate::report::Light {
    let (kind, camera_relative) = match l.style {
        4 => ("directional", true),
        5 => ("point", true),
        6 => ("spot", true),
        7 => ("directional", false),
        8 => ("point", false),
        9 => ("spot", false),
        10 => ("ambient", false),
        11 => ("linear", false),
        12 => ("rectangular", false),
        _ => ("unknown", false),
    };
    let m = |v: [f64; 3]| v.map(|x| round(x * metres, 6));
    let len = (l.direction[0].powi(2) + l.direction[1].powi(2) + l.direction[2].powi(2)).sqrt();
    let direction = if matches!(kind, "point" | "ambient") || len.is_nan() || len <= 1e-12 {
        [0.0; 3]
    } else {
        l.direction.map(|v| round(v / len, 6))
    };
    let extended = matches!(kind, "linear" | "rectangular");
    crate::report::Light {
        name: l.name.clone(),
        kind: kind.to_string(),
        camera_relative,
        on: l.on && l.visible && layer.as_ref().is_none_or(|(_, visible)| *visible),
        layer: layer.map(|(path, _)| path),
        intensity: round(l.intensity, 6),
        watts: round(l.watts, 3),
        color: l.diffuse,
        location: m(l.location),
        direction,
        spot_angle_deg: (kind == "spot").then(|| round(l.spot_angle_deg, 3)),
        hotspot: if kind == "spot" {
            l.hotspot.map(|h| round(h, 4))
        } else {
            None
        },
        length: extended.then(|| m(l.length)),
        width: extended.then(|| m(l.width)),
    }
}

/// Rounds for stable output across platforms (native and WebAssembly math libraries differ in the
/// last bits of transcendental functions).
fn round(value: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 { 0.0 } else { rounded }
}
