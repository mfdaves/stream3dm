//! The context: a compact summary of a [`Report`] (what the model is, at a glance), for tools
//! that describe or classify models. It is derived from the report alone. Names stay exactly as written in the file; no
//! layer, material or block name is translated or normalized.

use crate::report::{
    Element, Level, Projection, Report, Severity, Storeys, StyleSignals, ViewKind,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Version of the context layout.
pub const CONTEXT_VERSION: u32 = 1;

/// Object names listed at most.
const MAX_NAMES: usize = 50;
/// User-string values listed per key at most.
const MAX_VALUES: usize = 5;

/// A compact summary of a [`Report`]; made by [`context`]. Names stay as written in the
/// file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Context {
    /// [`CONTEXT_VERSION`].
    pub context_version: u32,
    /// Unit system of the file; every length below is in metres.
    pub units: String,
    /// Overall size of the visible geometry (x, y, z).
    pub size_m: Option<[f64; 3]>,
    /// Longer plan side over shorter plan side.
    pub plan_aspect: Option<f64>,
    /// As in [`Proportions::storeys`](crate::Proportions::storeys).
    pub storeys: Option<Storeys>,
    /// Floor-level candidates (upward-facing area by elevation), even without a storey verdict.
    pub levels: Vec<Level>,
    /// Layers that hold objects or visible area.
    pub layers: Vec<ContextLayer>,
    /// Per object when the model is small, otherwise grouped by layer, material and block.
    pub elements_per_object: bool,
    /// The report's elements, those that differ only in their count merged (counts added).
    pub elements: Vec<Element>,
    /// Materials in use, merged by name.
    pub materials: Vec<ContextMaterial>,
    /// Every block definition, sorted by name.
    pub blocks: Vec<ContextBlock>,
    /// Named views (or the saved viewports when the file has none).
    pub views: Vec<ContextView>,
    /// User-string keys, as in [`Structure::user_strings`](crate::Structure::user_strings).
    pub user_strings: Vec<ContextUserString>,
    /// Distinct object names, sorted, at most 50.
    pub object_names: Vec<String>,
    /// The report's style signals.
    pub style: StyleSignals,
    /// Warnings about what the model data leaves out.
    pub notes: Vec<String>,
}

/// A layer in the [`Context`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextLayer {
    /// Full path, `::`-separated.
    pub path: String,
    /// Model-space objects on it.
    pub objects: usize,
    /// Visible mesh area on it.
    pub area_m2: f64,
    /// Name of the layer's material.
    pub material: Option<String>,
    /// `#rrggbb`.
    pub color: Option<String>,
    /// Size of its visible meshes' bounding box (x, y, z); `None` when it has none.
    pub size_m: Option<[f64; 3]>,
    /// The layer's own visibility flag is off.
    pub hidden: bool,
}

/// The materials in use that share one name, merged: areas added, textures joined, the first
/// colour and PBR values found kept.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextMaterial {
    /// Name as saved.
    pub name: String,
    /// Materials in use with this name.
    pub variants: usize,
    /// `#rrggbb`.
    pub color: Option<String>,
    /// PBR metallic, 0 to 1.
    pub metallic: Option<f64>,
    /// PBR roughness, 0 to 1.
    pub roughness: Option<f64>,
    /// PBR opacity, 0 to 1.
    pub opacity: Option<f64>,
    /// Visible mesh area that uses them.
    pub area_m2: f64,
    /// Texture file names, without repeats.
    pub textures: Vec<String>,
}

/// A block definition in the [`Context`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextBlock {
    /// Definition name.
    pub name: String,
    /// As in [`Block::placements`](crate::Block::placements).
    pub placements: usize,
    /// Size of its displayable geometry, unplaced.
    pub size_m: Option<[f64; 3]>,
}

/// A view in the [`Context`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextView {
    /// Name as saved.
    pub name: String,
    /// Parallel or perspective.
    pub projection: Projection,
    /// 35 mm-equivalent lens length (perspective views).
    pub lens_35mm: Option<f64>,
    /// Camera location.
    pub camera: [f64; 3],
    /// The camera's target point, when the file stores one.
    pub target: Option<[f64; 3]>,
}

/// A user-string key in the [`Context`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextUserString {
    /// The key, as saved.
    pub key: String,
    /// Model-space objects with this key.
    pub objects: usize,
    /// Distinct values, sorted, at most five.
    pub values: Vec<String>,
}

/// The [`Context`] of `report`.
pub fn context(report: &Report) -> Context {
    let proportions = report.proportions.as_ref();
    let layers = report
        .structure
        .layers
        .iter()
        .filter(|l| l.objects > 0 || l.mesh_area_m2 > 0.0)
        .map(|l| ContextLayer {
            path: l.path.clone(),
            objects: l.objects,
            area_m2: l.mesh_area_m2,
            material: l.material.clone(),
            color: l.color.clone(),
            size_m: l
                .bbox
                .as_ref()
                .map(|b| [0, 1, 2].map(|i| round6(b.max[i] - b.min[i]))),
            hidden: !l.visible,
        })
        .collect();

    let mut merged: BTreeMap<&str, ContextMaterial> = BTreeMap::new();
    for m in report
        .materials
        .iter()
        .filter(|m| m.objects > 0 || m.layers > 0 || m.mesh_area_m2 > 0.0)
    {
        let entry = merged.entry(&m.name).or_insert_with(|| ContextMaterial {
            name: m.name.clone(),
            variants: 0,
            color: None,
            metallic: None,
            roughness: None,
            opacity: None,
            area_m2: 0.0,
            textures: Vec::new(),
        });
        entry.variants += 1;
        entry.area_m2 = round6(entry.area_m2 + m.mesh_area_m2);
        entry.color = entry.color.take().or_else(|| m.color.clone());
        entry.metallic = entry.metallic.or(m.metallic);
        entry.roughness = entry.roughness.or(m.roughness);
        entry.opacity = entry.opacity.or(m.opacity);
        for t in &m.textures {
            if !entry.textures.contains(t) {
                entry.textures.push(t.clone());
            }
        }
    }
    let mut materials: Vec<ContextMaterial> = merged.into_values().collect();
    materials.sort_by(|a, b| b.area_m2.total_cmp(&a.area_m2).then(a.name.cmp(&b.name)));

    let named = report.views.iter().any(|v| v.kind == ViewKind::Named);
    let views = report
        .views
        .iter()
        .filter(|v| !named || v.kind == ViewKind::Named)
        .map(|v| ContextView {
            name: v.name.clone(),
            projection: v.projection,
            lens_35mm: v.lens_35mm,
            camera: v.camera,
            target: v.target,
        })
        .collect();

    Context {
        context_version: CONTEXT_VERSION,
        units: report.file.unit_system.clone(),
        size_m: proportions.map(|p| p.size),
        plan_aspect: proportions.and_then(|p| p.plan_aspect),
        storeys: proportions.and_then(|p| p.storeys.clone()),
        levels: proportions.map(|p| p.levels.clone()).unwrap_or_default(),
        layers,
        elements_per_object: report.elements.per_object,
        elements: merge_identical(&report.elements.items),
        materials,
        blocks: report
            .blocks
            .iter()
            .map(|b| ContextBlock {
                name: b.name.clone(),
                placements: b.placements,
                size_m: b.size,
            })
            .collect(),
        views,
        user_strings: report
            .structure
            .user_strings
            .iter()
            .map(|u| ContextUserString {
                key: u.key.clone(),
                objects: u.objects,
                values: u.values.iter().take(MAX_VALUES).cloned().collect(),
            })
            .collect(),
        object_names: report
            .structure
            .object_names
            .iter()
            .take(MAX_NAMES)
            .cloned()
            .collect(),
        style: report.style.clone(),
        notes: report
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .map(|d| d.message.clone())
            .collect(),
    }
}

/// Elements that differ only in their count (typically placements of one block at the same
/// height) become one element with the counts added.
fn merge_identical(items: &[Element]) -> Vec<Element> {
    let mut out: Vec<Element> = Vec::with_capacity(items.len());
    for item in items {
        let same = |e: &&mut Element| {
            Element {
                count: item.count,
                ..(**e).clone()
            } == *item
        };
        match out.iter_mut().find(same) {
            Some(e) => e.count += item.count,
            None => out.push(item.clone()),
        }
    }
    out
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}
