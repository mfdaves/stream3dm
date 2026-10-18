//! IFC (ISO 16739) models: what a viewer and the AI context need from the STEP text, read in
//! slices (header, spatial structure, classes, types, materials, property sets and quantities,
//! units), while the geometry comes product by product from a mesh engine (web-ifc in the
//! viewer). Both join the same report, sink and fingerprint as `.3dm` objects:
//!
//! - layers are `storey::class` (the storey a product stands on, through containment and
//!   aggregation; `No storey` otherwise), storeys ordered by elevation;
//! - materials are the IFC materials by name (the thickest layer of a layer set, the first of a
//!   list, constituent or profile set), coloured by the engine's colour of their first product;
//! - IFC types (`IfcWallType`, `IfcDoorStyle`, …) act as blocks: a product's pick names its type,
//!   the report lists each type with its product count;
//! - attributes, property sets and quantities (the type's first, the product's own replacing
//!   them) become the product's user strings;
//! - the report's storeys are the file's building storeys, not the mesh heuristic.
//!
//! Lengths come out in metres: the engine's meshes are in metres already, elevations and layer
//! thicknesses are scaled by the project's length unit.

pub(crate) mod step;

use crate::extract::{Facts, Mode, Tables, finalize};
use crate::native::{LayerRec, MaterialRec, ObjectKind};
use crate::object::{Counted, MeshGeom, ObjectView};
use crate::reader::document::Document;
use crate::report::{Block, Repetition, Report, Storeys};
use crate::{InspectError, MeshSink};
use std::collections::{BTreeMap, HashMap};
use step::{Splitter, Value};

/// User strings kept per product (attributes first, then property sets).
const MAX_PRODUCT_STRINGS: usize = 1000;
/// Nesting followed when looking for a product's storey.
const MAX_DEPTH: usize = 32;

/// Reads an IFC model: push the file's bytes in slices, [`prepare`](Self::prepare), add each
/// product's mesh, then [`finish`](Self::finish).
pub struct IfcInspector {
    splitter: Splitter,
    /// The first bytes, until the STEP signature is checked.
    head: Vec<u8>,
    checked: bool,
    model: Model,
    sink: Option<Box<dyn MeshSink>>,
    prepared: Option<Prepared>,
}

/// The entities read from the file.
#[derive(Default)]
struct Model {
    schema: Option<String>,
    application: Option<String>,
    /// Entities with a GlobalId (`IfcRoot`): class, GlobalId, Name, ObjectType.
    rooted: HashMap<u64, Rooted>,
    classes: Vec<String>,
    class_index: HashMap<String, u16>,
    /// Products with a representation, in file order.
    products: Vec<u64>,
    storey_elevation: HashMap<u64, f64>,
    /// Type object → its property sets (`HasPropertySets`).
    type_psets: HashMap<u64, Vec<u64>>,
    /// Element → spatial structure containing it.
    contained_in: HashMap<u64, u64>,
    /// Part → whole (`IfcRelAggregates`, `IfcRelNests`).
    part_of: HashMap<u64, u64>,
    /// Object → its property set definitions, as (relationship id, definition), in file order
    /// until read in relationship order (see [`Prepared::user_strings`]).
    psets_of: HashMap<u64, Vec<(u64, u64)>>,
    type_of: HashMap<u64, u64>,
    material_of: HashMap<u64, u64>,
    /// Property sets and element quantities: name and members.
    psets: HashMap<u64, (String, Vec<u64>)>,
    /// Properties and quantities: name and value as text.
    props: HashMap<u64, (String, String)>,
    materials: HashMap<u64, MaterialDef>,
    /// Units: SI length units and conversions (metres per unit).
    length_units: HashMap<u64, f64>,
    measures: HashMap<u64, (f64, u64)>,
    conversions: HashMap<u64, (bool, u64)>,
    unit_assignments: Vec<Vec<u64>>,
    unparsed: usize,
}

struct Rooted {
    class: u16,
    guid: String,
    name: Option<String>,
    object_type: Option<String>,
    tag: Option<String>,
}

/// The material definitions a product's material resolves through.
enum MaterialDef {
    Material(String),
    /// Layers: (material, thickness in file units).
    LayerSet(Option<String>, Vec<u64>),
    Layer(Option<u64>, f64),
    List(Vec<u64>),
    /// Constituent and profile sets: name, members.
    Set(Option<String>, Vec<u64>),
    /// Constituents and profiles: their material.
    Member(Option<u64>),
    /// Usages: the set they use.
    Usage(u64),
}

/// A product as the reader resolved it: what a viewer's object panel and the AI context get.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IfcProduct {
    /// STEP id (`#123`).
    pub id: u64,
    /// GlobalId as written (22 characters).
    pub guid: String,
    /// IFC class in its usual spelling (`IfcWall`, `IfcWindow`, …).
    pub class: String,
    /// The product's Name; `None` when unset or blank.
    pub name: Option<String>,
    /// Name of its type object (`IfcRelDefinesByType`).
    pub type_name: Option<String>,
    /// Name of its material, or its type's: the thickest layer of a layer set, the first of a
    /// list, constituent or profile set, else the set's name.
    pub material: Option<String>,
    /// Attributes (class, GlobalId, storey, type, ObjectType, tag, material), then property
    /// sets and quantities as `Set.Property`.
    pub user_strings: Vec<(String, String)>,
}

/// One part of a product's mesh, with its colour.
#[derive(Debug, Clone, Copy)]
pub struct ProductPart<'a> {
    /// Metres, z up, world coordinates: x, y, z per vertex.
    pub positions: &'a [f64],
    /// Three per triangle, into `positions`.
    pub indices: &'a [u32],
    /// The part's colour from the engine as `[r, g, b]`; the first one among a material's
    /// products also colours that material in the report.
    pub color: Option<[u8; 3]>,
}

/// The tables and facts once the file is read, and each product's resolved metadata.
struct Prepared {
    tables: Tables,
    facts: Facts,
    products: HashMap<u64, ProductMeta>,
    material_names: Vec<String>,
    /// Material index → colour of its first product with one.
    material_colors: HashMap<usize, [u8; 3]>,
    type_counts: BTreeMap<String, usize>,
    storeys: Vec<(String, Option<f64>)>,
    /// The layer of products the file does not list (`No storey::IfcProduct`).
    no_storey_product_layer: i64,
    size: u64,
    /// The entities read, for the products' property sets.
    model: Model,
}

impl Prepared {
    /// A product's attributes, then its type's property sets and its own (replacing the type's
    /// values), as `Set.Property` pairs. Its own sets are taken in the order of the relationships'
    /// ids, so where two sets give one property, the later relationship's value stands, as in
    /// IfcOpenShell, whatever the order of the records in the file.
    fn user_strings(&self, id: u64, meta: &ProductMeta) -> Vec<(String, String)> {
        let mut props = Vec::new();
        for pset in meta
            .type_id
            .and_then(|t| self.model.type_psets.get(&t))
            .into_iter()
            .flatten()
        {
            self.model.pset_strings(*pset, &mut props);
        }
        let mut own: Vec<(u64, u64)> = self.model.psets_of.get(&id).cloned().unwrap_or_default();
        own.sort_unstable();
        for (_, pset) in own {
            self.model.pset_strings(pset, &mut props);
        }
        let mut out = meta.attributes.clone();
        out.extend(props);
        out.truncate(MAX_PRODUCT_STRINGS);
        out
    }
}

struct ProductMeta {
    guid: String,
    class: String,
    name: Option<String>,
    type_id: Option<u64>,
    type_name: Option<String>,
    layer: i64,
    material: Option<usize>,
    /// Class, GlobalId, storey, type, ObjectType, tag, material: the property sets follow,
    /// read from the model when the product's strings are wanted.
    attributes: Vec<(String, String)>,
}

impl Default for IfcInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl IfcInspector {
    /// An inspector without a sink.
    pub fn new() -> Self {
        Self {
            splitter: Splitter::default(),
            head: Vec::new(),
            checked: false,
            model: Model::default(),
            sink: None,
            prepared: None,
        }
    }

    /// Hands every product's mesh to `sink` (with `MeshSink::pick` descriptions).
    pub fn with_sink(sink: Box<dyn MeshSink>) -> Self {
        Self {
            sink: Some(sink),
            ..Self::new()
        }
    }

    /// Reads the next slice of the file.
    pub fn push(&mut self, data: &[u8]) -> Result<(), InspectError> {
        if self.prepared.is_some() {
            return Err(InspectError::Decode("bytes pushed after prepare".into()));
        }
        if !self.checked {
            self.head.extend_from_slice(&data[..data.len().min(64)]);
            let text = String::from_utf8_lossy(&self.head);
            let text = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
            const SIGNATURE: &str = "ISO-10303-21";
            if text.len() >= SIGNATURE.len() {
                if !text.starts_with(SIGNATURE) {
                    return Err(InspectError::Decode("not an IFC (STEP) file".into()));
                }
                self.checked = true;
                self.head = Vec::new();
            }
        }
        let model = &mut self.model;
        self.splitter.push(data, |text| model.read(text));
        Ok(())
    }

    /// The products the file gives a representation, for the engine to mesh.
    pub fn product_count(&self) -> usize {
        self.prepared
            .as_ref()
            .map_or(self.model.products.len(), |p| p.products.len())
    }

    /// Each product the file gives a representation, with what the reader resolved for it
    /// (after [`prepare`](Self::prepare); empty before), in STEP id order.
    pub fn products(&self) -> Vec<IfcProduct> {
        let Some(p) = &self.prepared else {
            return Vec::new();
        };
        let mut out: Vec<IfcProduct> = p
            .products
            .iter()
            .map(|(id, m)| IfcProduct {
                id: *id,
                guid: m.guid.clone(),
                class: m.class.clone(),
                name: m.name.clone(),
                type_name: m.type_name.clone(),
                material: m.material.map(|i| p.material_names[i].clone()),
                user_strings: p.user_strings(*id, m),
            })
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    /// Resolves storeys, types, materials and property sets once every byte is pushed. The
    /// products' meshes can be added from then on.
    pub fn prepare(&mut self) -> Result<(), InspectError> {
        if self.prepared.is_some() {
            return Ok(());
        }
        let size = self.splitter.offset;
        if size == 0 {
            return Err(InspectError::Truncated { offset: 0 });
        }
        let model = std::mem::take(&mut self.model);
        let mut prepared = model.prepare(size);
        if let Some(sink) = self.sink.take() {
            prepared.facts.set_sink(&prepared.tables, sink);
        }
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Adds product `id` (its STEP id) with the parts of its mesh as the engine gives them
    /// (each with its colour, say a window's frame and its glass): positions in metres, z up,
    /// world coordinates (three per vertex), indices into the part's own positions (three per
    /// triangle). Products the file does not list are counted as objects of class `IfcProduct`
    /// on no storey.
    pub fn add_product(&mut self, id: u64, parts: &[ProductPart<'_>]) -> Result<(), InspectError> {
        let Some(p) = &mut self.prepared else {
            return Err(InspectError::Decode("add_product before prepare".into()));
        };
        let meta = p.products.get(&id);
        let key = format!("#{id}");
        let meshes: Vec<MeshGeom> = parts
            .iter()
            .enumerate()
            .map(|(k, part)| {
                let vertex_count = part.positions.len() / 3;
                let triangles: Vec<[u32; 3]> = part
                    .indices
                    .chunks_exact(3)
                    .filter(|t| t.iter().all(|&i| (i as usize) < vertex_count))
                    .map(|t| [t[0], t[1], t[2]])
                    .collect();
                MeshGeom {
                    positions_mm: part
                        .positions
                        .chunks_exact(3)
                        .map(|c| [c[0] * 1000.0, c[1] * 1000.0, c[2] * 1000.0])
                        .collect(),
                    vertex_count,
                    triangle_count: triangles.len(),
                    triangles,
                    in_instance: false,
                    source_visible: None,
                    source_layer: None,
                    key: format!("{key}.{k}"),
                    color: part.color,
                }
            })
            .collect();
        if let Some(m) = meta.and_then(|m| m.material)
            && let Some(c) = parts.iter().find_map(|part| part.color)
        {
            p.material_colors.entry(m).or_insert(c);
        }
        let view = ObjectView {
            uuid: meta.map(|m| m.guid.clone()).or_else(|| Some(key.clone())),
            fallback_key: key.clone(),
            name: meta.and_then(|m| m.name.clone()),
            layer_index: Some(meta.map_or(p.no_storey_product_layer, |m| m.layer)),
            material_index: meta.and_then(|m| m.material).map(|m| m as i64),
            material_source: Some(1),
            visible: Some(true),
            mode: None,
            groups: Vec::new(),
            user_strings: meta.map(|m| p.user_strings(id, m)).unwrap_or_default(),
            kind: Counted::Object(ObjectKind::Mesh),
            meshes,
            curves: Vec::new(),
            points: Vec::new(),
            instance: None,
            faces: 0,
            planar_faces: 0,
            generated_mesh: false,
            undisplayed_faces: 0,
            class: Some(meta.map_or_else(|| "IfcProduct".to_string(), |m| m.class.clone())),
            type_name: meta.and_then(|m| m.type_name.clone()),
        };
        if let Some(t) = meta.and_then(|m| m.type_name.clone()) {
            *p.type_counts.entry(t).or_default() += 1;
        }
        p.facts.absorb_object(&p.tables, &view, Mode::Batch);
        Ok(())
    }

    /// The report: the products added, with the file's storeys and types.
    pub fn finish(mut self) -> Result<Report, InspectError> {
        self.prepare()?;
        let p = self.prepared.take().expect("prepared");
        if self.splitter.pending() > 0 {
            return Err(InspectError::Truncated { offset: p.size });
        }
        let mut report = finalize(&p.tables, p.facts, p.size);
        for (i, m) in report.materials.iter_mut().enumerate() {
            if let Some(c) = p.material_colors.get(&i) {
                m.color = Some(format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]));
            }
        }
        report.blocks = p
            .type_counts
            .iter()
            .map(|(name, n)| Block {
                name: name.clone(),
                placements: *n,
                members: 0,
                size: None,
            })
            .collect();
        // The repetition figures follow the types, which stand in for blocks.
        report.style.repetition = Repetition {
            block_definitions: report.blocks.len(),
            block_placements: report.blocks.iter().map(|b| b.placements).sum(),
            max_placements_per_block: report
                .blocks
                .iter()
                .map(|b| b.placements)
                .max()
                .unwrap_or(0),
        };
        let levels: Vec<f64> = p
            .storeys
            .iter()
            .filter_map(|(_, z)| *z)
            .map(|z| (z * 1e6).round() / 1e6)
            .collect();
        if let Some(proportions) = &mut report.proportions
            && !levels.is_empty()
        {
            let mut gaps: Vec<f64> = levels
                .windows(2)
                .map(|w| w[1] - w[0])
                .filter(|g| *g > 0.5)
                .collect();
            gaps.sort_by(f64::total_cmp);
            let typical = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
            proportions.storeys = Some(Storeys {
                count: levels.len(),
                typical_height: (typical * 1e6).round() / 1e6,
                levels,
            });
        }
        Ok(report)
    }
}

impl Model {
    fn class_id(&mut self, name: &str) -> u16 {
        if let Some(&i) = self.class_index.get(name) {
            return i;
        }
        let i = self.classes.len() as u16;
        self.classes.push(name.to_string());
        self.class_index.insert(name.to_string(), i);
        i
    }

    fn read(&mut self, text: &[u8]) {
        let Some(r) = step::record(text) else {
            return;
        };
        let Some(id) = r.id else {
            match r.name {
                "FILE_SCHEMA" => {
                    let a = step::args(r.args);
                    self.schema = a
                        .first()
                        .and_then(|v| v.list().first())
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                "FILE_NAME" => {
                    let a = step::args(r.args);
                    // Originating system, else preprocessor version.
                    self.application = a
                        .get(5)
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| {
                            a.get(4)
                                .and_then(Value::as_str)
                                .filter(|s| !s.trim().is_empty())
                        })
                        .map(str::to_string);
                }
                _ => {}
            }
            return;
        };
        let name = r.name;
        match name {
            "IFCRELCONTAINEDINSPATIALSTRUCTURE" => {
                let a = step::args(r.args);
                if let Some(structure) = a.get(5).and_then(Value::as_ref) {
                    for e in a.get(4).map(Value::refs).unwrap_or_default() {
                        self.contained_in.insert(e, structure);
                    }
                }
            }
            // Openings belong to the element they cut, fillings (doors, windows) and projections
            // to their opening or host: a product with no containment of its own stands on
            // their storey.
            "IFCRELVOIDSELEMENT" | "IFCRELPROJECTSELEMENT" | "IFCRELFILLSELEMENT" => {
                // Relating (the host, or the opening a filling fills) at 4, related at 5.
                let a = step::args(r.args);
                if let (Some(w), Some(p)) = (
                    a.get(4).and_then(Value::as_ref),
                    a.get(5).and_then(Value::as_ref),
                ) {
                    self.part_of.entry(p).or_insert(w);
                }
            }
            "IFCRELAGGREGATES" | "IFCRELNESTS" => {
                let a = step::args(r.args);
                if let Some(whole) = a.get(4).and_then(Value::as_ref) {
                    for part in a.get(5).map(Value::refs).unwrap_or_default() {
                        self.part_of.entry(part).or_insert(whole);
                    }
                }
            }
            "IFCRELDEFINESBYPROPERTIES" => {
                let a = step::args(r.args);
                if let Some(pset) = a.get(5).and_then(Value::as_ref) {
                    for o in a.get(4).map(Value::refs).unwrap_or_default() {
                        self.psets_of.entry(o).or_default().push((id, pset));
                    }
                }
            }
            "IFCRELDEFINESBYTYPE" => {
                let a = step::args(r.args);
                if let Some(t) = a.get(5).and_then(Value::as_ref) {
                    for o in a.get(4).map(Value::refs).unwrap_or_default() {
                        self.type_of.insert(o, t);
                    }
                }
            }
            "IFCRELASSOCIATESMATERIAL" => {
                let a = step::args(r.args);
                if let Some(m) = a.get(5).and_then(Value::as_ref) {
                    for o in a.get(4).map(Value::refs).unwrap_or_default() {
                        self.material_of.entry(o).or_insert(m);
                    }
                }
            }
            // Door and window panel and lining properties: property set definitions whose
            // properties are the entity's attributes.
            "IFCDOORPANELPROPERTIES"
            | "IFCDOORLININGPROPERTIES"
            | "IFCWINDOWPANELPROPERTIES"
            | "IFCWINDOWLININGPROPERTIES" => {
                let a = step::args(r.args);
                let attributes: &[(usize, &str)] = match name {
                    "IFCDOORPANELPROPERTIES" => &[
                        (4, "PanelDepth"),
                        (5, "PanelOperation"),
                        (6, "PanelWidth"),
                        (7, "PanelPosition"),
                    ],
                    "IFCWINDOWPANELPROPERTIES" => &[
                        (4, "OperationType"),
                        (5, "PanelPosition"),
                        (6, "FrameDepth"),
                        (7, "FrameThickness"),
                    ],
                    "IFCDOORLININGPROPERTIES" => &[
                        (4, "LiningDepth"),
                        (5, "LiningThickness"),
                        (6, "ThresholdDepth"),
                        (7, "ThresholdThickness"),
                        (8, "TransomThickness"),
                        (9, "TransomOffset"),
                        (10, "LiningOffset"),
                        (11, "ThresholdOffset"),
                        (12, "CasingThickness"),
                        (13, "CasingDepth"),
                        (15, "LiningToPanelOffsetX"),
                        (16, "LiningToPanelOffsetY"),
                    ],
                    _ => &[
                        (4, "LiningDepth"),
                        (5, "LiningThickness"),
                        (6, "TransomThickness"),
                        (7, "MullionThickness"),
                        (8, "FirstTransomOffset"),
                        (9, "SecondTransomOffset"),
                        (10, "FirstMullionOffset"),
                        (11, "SecondMullionOffset"),
                        (13, "LiningOffset"),
                        (14, "LiningToPanelOffsetX"),
                        (15, "LiningToPanelOffsetY"),
                    ],
                };
                let mut members = Vec::new();
                for (k, (index, prop)) in attributes.iter().enumerate() {
                    if let Some(v) = a.get(*index).and_then(Value::display) {
                        // Ids of their own, apart from the file's (below 2^56).
                        let pid = (1 << 62) | (id << 4) | k as u64;
                        self.props.insert(pid, ((*prop).to_string(), v));
                        members.push(pid);
                    }
                }
                let pset_name = a.get(2).and_then(Value::as_str).unwrap_or("").to_string();
                self.psets.insert(id, (pset_name, members));
            }
            "IFCPROPERTYSET" => {
                let a = step::args(r.args);
                let pset_name = a.get(2).and_then(Value::as_str).unwrap_or("").to_string();
                self.psets.insert(
                    id,
                    (pset_name, a.get(4).map(Value::refs).unwrap_or_default()),
                );
            }
            "IFCELEMENTQUANTITY" => {
                let a = step::args(r.args);
                let pset_name = a.get(2).and_then(Value::as_str).unwrap_or("").to_string();
                self.psets.insert(
                    id,
                    (pset_name, a.get(5).map(Value::refs).unwrap_or_default()),
                );
            }
            "IFCPROPERTYSINGLEVALUE" | "IFCPROPERTYENUMERATEDVALUE" | "IFCPROPERTYLISTVALUE" => {
                let a = step::args(r.args);
                if let (Some(n), Some(v)) = (
                    a.first().and_then(Value::as_str),
                    a.get(2).and_then(Value::display),
                ) {
                    self.props.insert(id, (n.to_string(), v));
                }
            }
            n if n.starts_with("IFCQUANTITY") => {
                let a = step::args(r.args);
                if let (Some(q), Some(v)) = (
                    a.first().and_then(Value::as_str),
                    a.get(3).and_then(Value::display),
                ) {
                    self.props.insert(id, (q.to_string(), v));
                }
            }
            "IFCMATERIAL" => {
                let a = step::args(r.args);
                if let Some(n) = a.first().and_then(Value::as_str) {
                    self.materials
                        .insert(id, MaterialDef::Material(n.to_string()));
                }
            }
            "IFCMATERIALLAYERSET" => {
                let a = step::args(r.args);
                let set_name = a.get(1).and_then(Value::as_str).map(str::to_string);
                self.materials.insert(
                    id,
                    MaterialDef::LayerSet(set_name, a.first().map(Value::refs).unwrap_or_default()),
                );
            }
            "IFCMATERIALLAYER" | "IFCMATERIALLAYERWITHOFFSETS" => {
                let a = step::args(r.args);
                let thickness = a.get(1).and_then(Value::as_f64).unwrap_or(0.0);
                self.materials.insert(
                    id,
                    MaterialDef::Layer(a.first().and_then(Value::as_ref), thickness),
                );
            }
            "IFCMATERIALLIST" => {
                let a = step::args(r.args);
                self.materials.insert(
                    id,
                    MaterialDef::List(a.first().map(Value::refs).unwrap_or_default()),
                );
            }
            "IFCMATERIALCONSTITUENTSET" | "IFCMATERIALPROFILESET" => {
                let a = step::args(r.args);
                let set_name = a.first().and_then(Value::as_str).map(str::to_string);
                self.materials.insert(
                    id,
                    MaterialDef::Set(set_name, a.get(2).map(Value::refs).unwrap_or_default()),
                );
            }
            "IFCMATERIALCONSTITUENT" | "IFCMATERIALPROFILE" | "IFCMATERIALPROFILEWITHOFFSETS" => {
                let a = step::args(r.args);
                self.materials
                    .insert(id, MaterialDef::Member(a.get(2).and_then(Value::as_ref)));
            }
            "IFCMATERIALLAYERSETUSAGE"
            | "IFCMATERIALPROFILESETUSAGE"
            | "IFCMATERIALPROFILESETUSAGETAPERING" => {
                let a = step::args(r.args);
                if let Some(set) = a.first().and_then(Value::as_ref) {
                    self.materials.insert(id, MaterialDef::Usage(set));
                }
            }
            "IFCSIUNIT" => {
                let a = step::args(r.args);
                if matches!(a.get(1), Some(Value::Enum(u)) if u == "LENGTHUNIT") {
                    let prefix = match a.get(2) {
                        Some(Value::Enum(p)) => si_prefix(p),
                        _ => 1.0,
                    };
                    self.length_units.insert(id, prefix);
                }
            }
            "IFCMEASUREWITHUNIT" => {
                let a = step::args(r.args);
                if let (Some(v), Some(u)) = (
                    a.first().and_then(Value::as_f64),
                    a.get(1).and_then(Value::as_ref),
                ) {
                    self.measures.insert(id, (v, u));
                }
            }
            "IFCCONVERSIONBASEDUNIT" | "IFCCONVERSIONBASEDUNITWITHOFFSET" => {
                let a = step::args(r.args);
                let length = matches!(a.get(1), Some(Value::Enum(u)) if u == "LENGTHUNIT");
                if let Some(m) = a.get(3).and_then(Value::as_ref) {
                    self.conversions.insert(id, (length, m));
                }
            }
            "IFCUNITASSIGNMENT" => {
                let a = step::args(r.args);
                self.unit_assignments
                    .push(a.first().map(Value::refs).unwrap_or_default());
            }
            _ => self.read_rooted(id, name, r.args),
        }
    }

    /// Entities with a GlobalId: products (with a representation), spatial elements, types.
    fn read_rooted(&mut self, id: u64, name: &str, args: &[u8]) {
        if !name.starts_with("IFC") || name.starts_with("IFCREL") {
            return;
        }
        // Cheap test before parsing: IfcRoot entities start with the 22-character GlobalId.
        let head = args
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map(|i| &args[i..]);
        if !head.is_some_and(|h| h.len() > 24 && h[0] == b'\'' && h[23] == b'\'') {
            return;
        }
        let a = step::args(args);
        let Some(guid) = a.first().and_then(Value::as_str) else {
            self.unparsed += 1;
            return;
        };
        let text = |i: usize| {
            a.get(i)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let class = self.class_id(name);
        // IfcProduct: ObjectPlacement (5) and Representation (6) are references or unset.
        let product = a.len() >= 7
            && matches!(a.get(5), Some(Value::Ref(_) | Value::Null))
            && matches!(a.get(6), Some(Value::Ref(_) | Value::Null));
        // IfcTypeObject: HasPropertySets (5) is a list.
        if let Some(Value::List(items)) = a.get(5)
            && !product
        {
            let psets: Vec<u64> = items.iter().filter_map(Value::as_ref).collect();
            if !psets.is_empty() {
                self.type_psets.insert(id, psets);
            }
        }
        if product && matches!(a.get(6), Some(Value::Ref(_))) {
            self.products.push(id);
        }
        if name == "IFCBUILDINGSTOREY"
            && let Some(z) = a.get(9).and_then(Value::as_f64)
        {
            self.storey_elevation.insert(id, z);
        }
        self.rooted.insert(
            id,
            Rooted {
                class,
                guid: guid.to_string(),
                name: text(2),
                object_type: if product { text(4) } else { None },
                tag: if product { text(7) } else { None },
            },
        );
    }

    /// Metres per file length unit (1 when the file does not say).
    fn metres_per_unit(&self) -> f64 {
        for units in &self.unit_assignments {
            for u in units {
                if let Some(f) = self.length_units.get(u) {
                    return *f;
                }
                if let Some((true, m)) = self.conversions.get(u)
                    && let Some((value, base)) = self.measures.get(m)
                {
                    return value * self.length_units.get(base).copied().unwrap_or(1.0);
                }
            }
        }
        1.0
    }

    fn class_name(&self, id: u64) -> Option<&str> {
        self.rooted
            .get(&id)
            .map(|r| self.classes[r.class as usize].as_str())
    }

    /// The storey a product stands on: through containment, or through the element or space
    /// it is part of.
    fn storey_of(&self, id: u64) -> Option<u64> {
        let mut current = id;
        for _ in 0..MAX_DEPTH {
            if self.class_name(current) == Some("IFCBUILDINGSTOREY") {
                return Some(current);
            }
            current = match (self.contained_in.get(&current), self.part_of.get(&current)) {
                (Some(&s), _) => s,
                (None, Some(&w)) => w,
                (None, None) => return None,
            };
        }
        None
    }

    /// A material definition's main material name, and a description of its layers.
    fn material_name(
        &self,
        id: u64,
        depth: usize,
        metres: f64,
    ) -> (Option<String>, Option<String>) {
        if depth > 8 {
            return (None, None);
        }
        let named = |m: Option<u64>| m.and_then(|m| self.material_name(m, depth + 1, metres).0);
        match self.materials.get(&id) {
            Some(MaterialDef::Material(n)) => (Some(n.clone()), None),
            Some(MaterialDef::Usage(set)) => self.material_name(*set, depth + 1, metres),
            Some(MaterialDef::Layer(m, _)) | Some(MaterialDef::Member(m)) => (named(*m), None),
            Some(MaterialDef::LayerSet(set_name, layers)) => {
                let mut thickest: Option<(f64, String)> = None;
                let mut parts = Vec::new();
                for l in layers {
                    if let Some(MaterialDef::Layer(m, t)) = self.materials.get(l) {
                        let n = named(*m).unwrap_or_else(|| "unnamed".to_string());
                        parts.push(format!("{} {} mm", n, (t * metres * 1000.0).round()));
                        if thickest.as_ref().is_none_or(|(best, _)| t > best) {
                            thickest = Some((*t, n));
                        }
                    }
                }
                let main = thickest.map(|(_, n)| n).or_else(|| set_name.clone());
                (main, (!parts.is_empty()).then(|| parts.join(", ")))
            }
            Some(MaterialDef::List(members)) | Some(MaterialDef::Set(_, members)) => {
                let names: Vec<String> = members
                    .iter()
                    .filter_map(|m| self.material_name(*m, depth + 1, metres).0)
                    .collect();
                let set_name = match self.materials.get(&id) {
                    Some(MaterialDef::Set(n, _)) => n.clone(),
                    _ => None,
                };
                (
                    names.first().cloned().or(set_name),
                    (names.len() > 1).then(|| names.join(", ")),
                )
            }
            None => (None, None),
        }
    }

    fn pset_strings(&self, pset: u64, out: &mut Vec<(String, String)>) {
        if let Some((name, members)) = self.psets.get(&pset) {
            let name = if name.trim().is_empty() {
                "Properties"
            } else {
                name.as_str()
            };
            for m in members {
                if let Some((p, v)) = self.props.get(m) {
                    let key = format!("{name}.{p}");
                    match out.iter_mut().find(|(k, _)| *k == key) {
                        Some(entry) => entry.1 = v.clone(),
                        None => out.push((key, v.clone())),
                    }
                }
            }
        }
    }

    fn prepare(self, size: u64) -> Prepared {
        let metres = self.metres_per_unit();
        // Storeys by elevation (unknown elevations last, in file order).
        let mut storeys: Vec<u64> = self
            .rooted
            .iter()
            .filter(|(_, r)| self.classes[r.class as usize] == "IFCBUILDINGSTOREY")
            .map(|(id, _)| *id)
            .collect();
        storeys.sort_by(|a, b| {
            let za = self
                .storey_elevation
                .get(a)
                .copied()
                .unwrap_or(f64::INFINITY);
            let zb = self
                .storey_elevation
                .get(b)
                .copied()
                .unwrap_or(f64::INFINITY);
            za.total_cmp(&zb).then(a.cmp(b))
        });
        let storey_label = |s: u64| {
            let r = &self.rooted[&s];
            r.name.clone().unwrap_or_else(|| format!("Storey #{s}"))
        };

        // Layers: one per storey (then "No storey"), with one child per class found on it.
        let mut layers: Vec<LayerRec> = Vec::new();
        let mut storey_layer: HashMap<Option<u64>, usize> = HashMap::new();
        let mut class_layer: HashMap<(Option<u64>, String), i64> = HashMap::new();
        let product_storey: HashMap<u64, Option<u64>> = self
            .products
            .iter()
            .map(|&p| (p, self.storey_of(p)))
            .collect();
        let mut add_parent = |key: Option<u64>, name: String, layers: &mut Vec<LayerRec>| {
            let index = layers.len();
            layers.push(LayerRec {
                archive_index: Some(index as i64),
                name: Some(name),
                color: None,
                visible: Some(true),
                locked: None,
                material_index: None,
                parent_uuid: None,
                source_uuid: Some(format!(
                    "storey:{}",
                    key.map_or("none".to_string(), |k| k.to_string())
                )),
            });
            storey_layer.insert(key, index);
        };
        for &s in &storeys {
            add_parent(Some(s), storey_label(s), &mut layers);
        }
        add_parent(None, "No storey".to_string(), &mut layers);
        let mut classes_on: Vec<(Option<u64>, String)> = product_storey
            .iter()
            .map(|(p, s)| {
                (
                    *s,
                    pretty_class(self.class_name(*p).unwrap_or("IFCPRODUCT")),
                )
            })
            .collect();
        classes_on.push((None, "IfcProduct".to_string()));
        classes_on.sort();
        classes_on.dedup();
        for (s, class) in classes_on {
            let parent = storey_layer[&s];
            let index = layers.len();
            layers.push(LayerRec {
                archive_index: Some(index as i64),
                name: Some(class.clone()),
                color: None,
                visible: Some(true),
                locked: None,
                material_index: None,
                parent_uuid: layers[parent].source_uuid.clone(),
                source_uuid: Some(format!("class:{index}")),
            });
            class_layer.insert((s, class), index as i64);
        }

        // Products: storey, class, type, material, strings.
        let mut material_names: Vec<String> = Vec::new();
        let mut material_index: HashMap<String, usize> = HashMap::new();
        let mut products = HashMap::new();
        for &p in &self.products {
            let r = &self.rooted[&p];
            let class = pretty_class(&self.classes[r.class as usize]);
            let storey = product_storey[&p];
            let layer = class_layer[&(storey, class.clone())];
            let type_id = self.type_of.get(&p).copied();
            let type_name = type_id
                .and_then(|t| self.rooted.get(&t))
                .and_then(|t| t.name.clone());
            let material_def = self
                .material_of
                .get(&p)
                .or_else(|| type_id.and_then(|t| self.material_of.get(&t)));
            let (material_name, layers_text) =
                material_def.map_or((None, None), |m| self.material_name(*m, 0, metres));
            let material = material_name.as_ref().map(|n| {
                *material_index.entry(n.clone()).or_insert_with(|| {
                    material_names.push(n.clone());
                    material_names.len() - 1
                })
            });
            let mut strings: Vec<(String, String)> = vec![
                ("IFC class".into(), class.clone()),
                ("GlobalId".into(), r.guid.clone()),
            ];
            if let Some(s) = storey {
                strings.push(("Storey".into(), storey_label(s)));
            }
            if let Some(t) = &type_name {
                strings.push(("Type".into(), t.clone()));
            }
            if let Some(o) = &r.object_type {
                strings.push(("ObjectType".into(), o.clone()));
            }
            if let Some(t) = &r.tag {
                strings.push(("Tag".into(), t.clone()));
            }
            if let Some(m) = &material_name {
                strings.push(("Material".into(), m.clone()));
            }
            if let Some(l) = layers_text {
                strings.push(("Material layers".into(), l));
            }
            products.insert(
                p,
                ProductMeta {
                    guid: r.guid.clone(),
                    class,
                    name: r.name.clone(),
                    type_id,
                    type_name,
                    layer,
                    material,
                    attributes: strings,
                },
            );
        }

        let doc = Document {
            archive_version: None,
            application: self.application.clone(),
            application_details: self.schema.clone(),
            unit_system: Some("standard:4".to_string()),
            mm_per_unit: Some(1000.0),
            absolute_tolerance_mm: None,
            layers,
            materials: material_names
                .iter()
                .enumerate()
                .map(|(i, n)| MaterialRec {
                    archive_index: Some(i as i64),
                    name: Some(n.clone()),
                    diffuse: None,
                    physically_based: None,
                    textures: Vec::new(),
                })
                .collect(),
            views: Vec::new(),
            groups: Vec::new(),
            definitions: Vec::new(),
            lights: 0,
            light_list: Vec::new(),
            earth_anchor: None,
            unparsed: self.unparsed,
        };
        let storeys = storeys
            .iter()
            .map(|s| {
                (
                    storey_label(*s),
                    self.storey_elevation.get(s).map(|z| z * metres),
                )
            })
            .collect();
        let no_storey_product_layer = class_layer[&(None, "IfcProduct".to_string())];
        let tables = Tables::from_document(doc);
        let facts = Facts::new(&tables);
        Prepared {
            tables,
            facts,
            products,
            material_names,
            material_colors: HashMap::new(),
            type_counts: BTreeMap::new(),
            storeys,
            no_storey_product_layer,
            size,
            model: self,
        }
    }
}

/// An SI prefix as a factor.
fn si_prefix(prefix: &str) -> f64 {
    match prefix {
        "MILLI" => 1e-3,
        "CENTI" => 1e-2,
        "DECI" => 1e-1,
        "KILO" => 1e3,
        "MICRO" => 1e-6,
        _ => 1.0,
    }
}

/// `IFCWALLSTANDARDCASE` → `IfcWallStandardCase`, through the schema's spelling of the classes a
/// viewer meets most; others as `Ifc` + the rest in lower case.
pub(crate) fn pretty_class(upper: &str) -> String {
    const KNOWN: &[&str] = &[
        "IfcActuator",
        "IfcAirTerminal",
        "IfcBeam",
        "IfcBuilding",
        "IfcBuildingElementPart",
        "IfcBuildingElementProxy",
        "IfcBuildingStorey",
        "IfcCableCarrierSegment",
        "IfcCableSegment",
        "IfcChimney",
        "IfcColumn",
        "IfcColumnStandardCase",
        "IfcCovering",
        "IfcCurtainWall",
        "IfcDiscreteAccessory",
        "IfcDistributionControlElement",
        "IfcDistributionElement",
        "IfcDistributionFlowElement",
        "IfcDoor",
        "IfcDoorStandardCase",
        "IfcDuctFitting",
        "IfcDuctSegment",
        "IfcElementAssembly",
        "IfcElectricAppliance",
        "IfcFastener",
        "IfcFlowController",
        "IfcFlowFitting",
        "IfcFlowMovingDevice",
        "IfcFlowSegment",
        "IfcFlowStorageDevice",
        "IfcFlowTerminal",
        "IfcFlowTreatmentDevice",
        "IfcFooting",
        "IfcFurnishingElement",
        "IfcFurniture",
        "IfcGeographicElement",
        "IfcLightFixture",
        "IfcMechanicalFastener",
        "IfcMember",
        "IfcMemberStandardCase",
        "IfcOpeningElement",
        "IfcPile",
        "IfcPipeFitting",
        "IfcPipeSegment",
        "IfcPlate",
        "IfcPlateStandardCase",
        "IfcProxy",
        "IfcRailing",
        "IfcRamp",
        "IfcRampFlight",
        "IfcReinforcingBar",
        "IfcReinforcingMesh",
        "IfcRoof",
        "IfcSanitaryTerminal",
        "IfcShadingDevice",
        "IfcSite",
        "IfcSlab",
        "IfcSlabElementedCase",
        "IfcSlabStandardCase",
        "IfcSpace",
        "IfcStair",
        "IfcStairFlight",
        "IfcSystemFurnitureElement",
        "IfcTransportElement",
        "IfcVirtualElement",
        "IfcWall",
        "IfcWallElementedCase",
        "IfcWallStandardCase",
        "IfcWindow",
        "IfcWindowStandardCase",
        "IfcBridge",
        "IfcBridgePart",
        "IfcRoad",
        "IfcRoadPart",
        "IfcCourse",
        "IfcEarthworksFill",
        "IfcKerb",
        "IfcPavement",
        "IfcSign",
        "IfcSignal",
        "IfcTendon",
        "IfcBearing",
        "IfcDeepFoundation",
        "IfcFacility",
        "IfcFacilityPart",
        "IfcAnnotation",
        "IfcGrid",
        "IfcProduct",
    ];
    KNOWN
        .iter()
        .find(|k| k.eq_ignore_ascii_case(upper))
        .map_or_else(
            || {
                let rest = upper.get(3..).unwrap_or("").to_lowercase();
                let mut chars = rest.chars();
                let first = chars
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string())
                    .unwrap_or_default();
                format!("Ifc{first}{}", chars.as_str())
            },
            |k| (*k).to_string(),
        )
}

#[cfg(test)]
mod tests;
