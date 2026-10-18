//! Typed views of the decoder's native Rhino records (`CadIr::native["rhino"]`).
//!
//! Only the fields the report needs are declared; every field is optional so that records from
//! any archive version deserialize. Records that still fail are counted, not dropped silently.

use serde::Deserialize;
#[cfg(feature = "cadmpeg")]
use {cadmpeg_ir::native::NativeNamespace, serde::de::DeserializeOwned, serde_json::Value};

/// Records of one native arena, paired with their record id.
#[cfg(feature = "cadmpeg")]
pub struct Arena<T> {
    pub records: Vec<(String, T)>,
    pub unparsed: usize,
}

#[cfg(feature = "cadmpeg")]
pub fn arena<T: DeserializeOwned>(ns: Option<&NativeNamespace>, name: &str) -> Arena<T> {
    let mut out = Arena {
        records: Vec::new(),
        unparsed: 0,
    };
    let Some(records) = ns.and_then(|ns| ns.arenas().get(name)) else {
        return out;
    };
    for record in records {
        match serde_json::from_value::<T>(Value::Object(record.fields().clone())) {
            Ok(value) => out.records.push((record.id().to_string(), value)),
            Err(_) => out.unparsed += 1,
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct LayerRec {
    pub archive_index: Option<i64>,
    pub name: Option<String>,
    pub color: Option<String>,
    pub visible: Option<bool>,
    pub locked: Option<bool>,
    pub material_index: Option<i64>,
    pub parent_uuid: Option<String>,
    pub source_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MaterialRec {
    pub archive_index: Option<i64>,
    pub name: Option<String>,
    pub diffuse: Option<String>,
    pub physically_based: Option<PbrRec>,
    #[serde(default)]
    pub textures: Vec<TextureRec>,
}

#[derive(Debug, Deserialize)]
pub struct PbrRec {
    pub base_color: Option<Vec<f64>>,
    pub metallic: Option<f64>,
    pub roughness: Option<f64>,
    pub opacity: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct TextureRec {
    pub file_reference: Option<FileRefRec>,
}

#[derive(Debug, Deserialize)]
pub struct FileRefRec {
    pub full_path: Option<String>,
    pub relative_path: Option<String>,
}

/// Object attributes (`object_presentation`).
#[cfg(feature = "cadmpeg")]
#[derive(Debug, Deserialize)]
pub struct ObjectRec {
    pub source_uuid: Option<String>,
    pub name: Option<String>,
    pub layer_index: Option<i64>,
    pub material_index: Option<i64>,
    pub material_source: Option<i64>,
    pub visible: Option<bool>,
    pub object_mode: Option<i64>,
    #[serde(default)]
    pub group_indexes: Vec<i64>,
    #[serde(default)]
    pub links: Vec<String>,
    pub attribute_user_strings: Option<Vec<UserStringRec>>,
}

#[cfg(feature = "cadmpeg")]
#[derive(Debug, Deserialize)]
pub struct UserStringRec {
    pub key: Option<String>,
    pub value: Option<String>,
}

/// Object records (`unknowns`): the links tell which decoded entities an object produced.
#[cfg(feature = "cadmpeg")]
#[derive(Debug, Deserialize)]
pub struct LinksRec {
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GroupRec {
    pub archive_index: Option<i64>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DefinitionRec {
    pub name: Option<String>,
    pub source_uuid: Option<String>,
    #[serde(default)]
    pub member_object_ids: Vec<String>,
}

#[cfg(feature = "cadmpeg")]
#[derive(Debug, Deserialize)]
pub struct OccurrenceRec {
    pub definition_uuid: Option<String>,
    #[serde(default)]
    pub links: Vec<String>,
    /// Row-major 4×4 placement; translation in `transform_units`.
    pub transform: Option<Vec<Vec<f64>>>,
    pub transform_units: Option<String>,
}

pub type Matrix = [[f64; 4]; 4];

pub const IDENTITY: Matrix = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

#[cfg(feature = "cadmpeg")]
impl OccurrenceRec {
    /// Placement with translation in millimetres (the unit of decoded geometry).
    pub fn matrix_mm(&self) -> Option<Matrix> {
        let rows = self.transform.as_ref()?;
        let mut m = IDENTITY;
        for (r, row) in rows.iter().take(4).enumerate() {
            for (c, v) in row.iter().take(4).enumerate() {
                m[r][c] = *v;
            }
        }
        let scale = match self.transform_units.as_deref() {
            None | Some("millimeter") => 1.0,
            Some("meter") => 1000.0,
            Some(_) => return None,
        };
        for row in m.iter_mut().take(3) {
            row[3] *= scale;
        }
        Some(m)
    }
}

pub fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.0; 4]; 4];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn apply(m: &Matrix, p: [f64; 3]) -> [f64; 3] {
    let w = m[3][0] * p[0] + m[3][1] * p[1] + m[3][2] * p[2] + m[3][3];
    let w = if w.abs() > f64::EPSILON { w } else { 1.0 };
    [0, 1, 2].map(|r| (m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2] + m[r][3]) / w)
}

#[derive(Debug, Deserialize)]
pub struct ViewRec {
    pub name: Option<String>,
    pub list_kind: Option<String>,
    pub list_index: Option<i64>,
    pub viewport: Option<ViewportRec>,
    pub target_millimeters: Option<Vec<f64>>,
    pub attributes: Option<ViewAttributesRec>,
}

#[derive(Debug, Deserialize)]
pub struct ViewAttributesRec {
    #[serde(default)]
    pub clipping_planes: Vec<ClippingPlaneRec>,
}

/// A clipping plane saved with a view: `a·x + b·y + c·z + d = 0` in millimetres.
#[derive(Debug, Deserialize)]
pub struct ClippingPlaneRec {
    pub enabled: Option<bool>,
    pub equation_mm: Option<Vec<f64>>,
}

#[derive(Debug, Deserialize)]
pub struct ViewportRec {
    pub projection: Option<i64>,
    pub camera_location_mm: Option<Vec<f64>>,
    pub camera_direction: Option<Vec<f64>>,
    pub camera_up: Option<Vec<f64>>,
    /// left, right, bottom, top, near, far.
    pub frustum_mm: Option<Vec<f64>>,
    pub target_millimeters: Option<Vec<f64>>,
}

/// What an object record decoded into, read from its link kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Brep {
        render_mesh: bool,
    },
    Mesh,
    #[cfg_attr(not(feature = "cadmpeg"), allow(dead_code))]
    Subd,
    Curve,
    Point,
    Annotation,
    Hatch,
    Other,
    /// No links: geometry retained undecoded (block instances and text dots are resolved later).
    Unlinked,
}

#[cfg(feature = "cadmpeg")]
pub fn classify(links: &[String]) -> ObjectKind {
    let has = |kind: &str| links.iter().any(|l| l.starts_with(kind));
    let body = has("rhino:object:body#");
    let surface = has("rhino:object:surface#");
    let mesh = has("rhino:object:tessellation#");
    if links.is_empty() {
        ObjectKind::Unlinked
    } else if has("rhino:object:subd#") {
        ObjectKind::Subd
    } else if surface {
        ObjectKind::Brep { render_mesh: mesh }
    } else if mesh {
        ObjectKind::Mesh
    } else if body {
        ObjectKind::Point
    } else if has("rhino:dimension:") {
        ObjectKind::Annotation
    } else if has("rhino:hatch:") {
        ObjectKind::Hatch
    } else if has("rhino:object:curve#") {
        ObjectKind::Curve
    } else {
        ObjectKind::Other
    }
}

/// `"6e63b500"` (RRGGBB + alpha byte) to `"#6e63b5"`.
pub fn hex_color(raw: &str) -> Option<String> {
    let rgb = raw.get(0..6)?;
    rgb.chars()
        .all(|c| c.is_ascii_hexdigit())
        .then(|| format!("#{}", rgb.to_ascii_lowercase()))
}

/// Linear RGB(A) in 0..1 to `"#rrggbb"`.
pub fn rgb_color(components: &[f64]) -> Option<String> {
    let [r, g, b] = [components.first()?, components.get(1)?, components.get(2)?]
        .map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}
