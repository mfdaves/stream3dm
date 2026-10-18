//! Our own decoder for `.3dm` object records: it works directly on the bytes of one record,
//! without building a document, so it has no per-document limits, no grouped rejections and no
//! amplification beyond the decoded geometry itself.
//!
//! Format references: the openNURBS source (McNeel's reference implementation) and the
//! format notes and source of `cadmpeg-codec-rhino` (Apache-2.0). The code here is our own.
//!
//! [`read_object`] turns one object record into an [`ObjectView`], the same description the
//! cadmpeg path produces. Classes not handled here return [`ReadError::Unsupported`] and are
//! left to cadmpeg.

pub(crate) mod attributes;
pub(crate) mod brep;
pub(crate) mod buffer;
pub(crate) mod classes;
pub(crate) mod cursor;
pub(crate) mod curve;
pub(crate) mod document;
pub(crate) mod extrusion;
pub(crate) mod idef;
pub(crate) mod instance;
pub(crate) mod material;
pub(crate) mod mesh;
pub(crate) mod point;
pub(crate) mod rdk;
pub(crate) mod record;
pub(crate) mod subd;
pub(crate) mod surface;
pub(crate) mod tessellate;
pub(crate) mod topology;
pub(crate) mod v1;
pub(crate) mod view;

#[cfg(all(test, feature = "cadmpeg"))]
mod tests;

use crate::native::ObjectKind;
use crate::object::{Counted, InstanceView, MeshGeom, ObjectView};
use crate::stream::frame::Width;
use classes::ClassKind;

/// A decoding failure inside one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// The data ended before a value.
    Eof { at: usize },
    /// A value has an impossible size or layout.
    Invalid { at: usize, what: &'static str },
    /// The class, or this version of it, is not decoded here.
    Unsupported(&'static str),
}

pub(crate) type Result<T> = std::result::Result<T, ReadError>;

/// What decoding an object record needs from the rest of the file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Context {
    pub width: Width,
    pub archive: u32,
    /// openNURBS version that wrote the file (properties chunk `0xa0000026`); several layouts
    /// depend on it. Compared as an unsigned number.
    pub stamp: u32,
    /// Millimetres per model unit; `None` when the file has no unit system.
    pub mm_per_unit: Option<f64>,
    /// Tessellate Breps that have no cached render mesh.
    pub tessellate: bool,
}

/// Formats a UUID stored as `u32`, `u16`, `u16` (little-endian) and eight bytes, the way the
/// rest of the report spells it (lower-case, hyphenated).
pub(crate) fn uuid_string(b: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Decodes one object record (the whole `0x20008070` chunk). `ordinal` is the record's
/// position in the object table, which names objects that have no UUID.
pub(crate) fn read_object(record: &[u8], ordinal: usize, ctx: &Context) -> Result<ObjectView> {
    let scale = ctx
        .mm_per_unit
        .ok_or(ReadError::Unsupported("unit system none"))?;
    let parts = record::parts(record, ctx.width)?;
    let class = classes::kind(&parts.object.class).ok_or(ReadError::Unsupported("class"))?;
    let mut view = base_view(&parts, ordinal, ctx)?;
    let data = parts.object.data;
    let mut meshes = Vec::new();
    view.kind = match class {
        ClassKind::Mesh => {
            meshes.push(mesh::read(&parts.object, ctx)?);
            Counted::Object(ObjectKind::Mesh)
        }
        ClassKind::Brep => {
            let b = brep::read(data, ctx)?;
            view.faces = b.faces;
            view.planar_faces = b.planar_faces;
            view.generated_mesh = b.generated;
            view.undisplayed_faces = b.undisplayed_faces;
            meshes = b.meshes;
            Counted::Object(ObjectKind::Brep {
                render_mesh: !meshes.is_empty() && !b.generated,
            })
        }
        ClassKind::Extrusion => {
            let e = extrusion::read(&parts.object, ctx)?;
            // Each cap is a planar face; the sides are counted as surfaces only.
            view.faces = e.caps;
            view.planar_faces = e.caps;
            view.generated_mesh = e.generated;
            meshes.extend(e.mesh);
            Counted::Object(ObjectKind::Brep {
                render_mesh: !meshes.is_empty() && !e.generated,
            })
        }
        // A surface object has no face of its own (as openNURBS counts faces), but is drawn:
        // tessellated over its whole domain like an untrimmed face.
        ClassKind::PlaneSurface | ClassKind::Surface => {
            if ctx.tessellate
                && let Ok(s) = surface::read(&parts.object, ctx.width)
            {
                let tess = tessellate::tessellate(&topology::untrimmed(s), scale);
                view.generated_mesh = !tess.meshes.is_empty();
                view.undisplayed_faces = tess.skipped.len();
                meshes = tess.meshes;
            }
            Counted::Object(ObjectKind::Brep { render_mesh: false })
        }
        ClassKind::InstanceRef => {
            let r = instance::read(data)?;
            view.instance = Some(InstanceView {
                definition: uuid_string(&r.definition),
                matrix: r.matrix.map(|mut m| {
                    for row in m.iter_mut().take(3) {
                        row[3] *= scale;
                    }
                    m
                }),
            });
            Counted::BlockInstance
        }
        ClassKind::Curve => {
            // A curve that cannot be decoded is still counted, just not drawn.
            if let Ok(curve) = curve::read(&parts.object, ctx.width) {
                let [t0, t1] = curve.domain();
                let points = curve.sample(t0, t1);
                if points.len() >= 2 && points.iter().flatten().all(|v| v.is_finite()) {
                    view.curves
                        .push(points.into_iter().map(|p| p.map(|v| v * scale)).collect());
                }
            }
            Counted::Object(ObjectKind::Curve)
        }
        ClassKind::Point | ClassKind::PointCloud => {
            let points = if class == ClassKind::Point {
                point::read_point(data).map(|p| vec![p])
            } else {
                point::read_cloud(data)
            };
            if let Ok(points) = points {
                view.points = points
                    .into_iter()
                    .filter(|p| p.iter().all(|v| v.is_finite()))
                    .map(|p| p.map(|v| v * scale))
                    .collect();
            }
            Counted::Object(ObjectKind::Point)
        }
        // The control cage, subdivided for display.
        ClassKind::SubD => {
            if let Some(cage) = subd::read(data, ctx, scale)? {
                let (positions_mm, triangles) =
                    crate::subdivide::triangulate(cage, crate::subdivide::LEVELS);
                meshes.push(mesh::Mesh {
                    positions_mm,
                    triangles,
                    dropped_faces: 0,
                });
                view.generated_mesh = true;
            }
            Counted::Object(ObjectKind::Subd)
        }
        ClassKind::TextDot | ClassKind::Annotation => Counted::Object(ObjectKind::Annotation),
        ClassKind::Hatch => Counted::Object(ObjectKind::Hatch),
        ClassKind::Other => Counted::Object(ObjectKind::Other),
    };
    view.meshes = meshes
        .into_iter()
        .enumerate()
        .map(|(i, m)| MeshGeom {
            vertex_count: m.positions_mm.len(),
            triangle_count: m.triangles.len(),
            positions_mm: m.positions_mm,
            triangles: m.triangles,
            in_instance: false,
            source_visible: view.visible,
            source_layer: None,
            key: format!("{}.mesh{i}", view.fallback_key),
            color: None,
        })
        .collect();
    Ok(view)
}

/// The object's attributes and identity, before its geometry is read.
fn base_view(parts: &record::RecordParts, ordinal: usize, ctx: &Context) -> Result<ObjectView> {
    let attrs = match parts.attributes {
        Some(body) => attributes::read(body, ctx.width)?,
        None => attributes::Attributes::default(),
    };
    let user_strings = match parts.attribute_userdata {
        Some(body) => attributes::user_strings(body, ctx.width)?,
        None => Vec::new(),
    };
    Ok(ObjectView {
        uuid: Some(uuid_string(&attrs.uuid)),
        fallback_key: format!("record {ordinal}"),
        name: Some(attrs.name),
        layer_index: Some(i64::from(attrs.layer)),
        material_index: Some(i64::from(attrs.material_index)),
        material_source: Some(i64::from(attrs.material_source)),
        visible: Some(attrs.visible),
        mode: Some(i64::from(attrs.mode)),
        groups: attrs.groups.into_iter().map(i64::from).collect(),
        user_strings,
        kind: Counted::Object(ObjectKind::Other),
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
    })
}

/// A record whose geometry is not decoded here, as an undecoded object with its attributes,
/// and the name of its class for the report.
#[cfg_attr(feature = "cadmpeg", allow(dead_code))]
pub(crate) fn read_undecoded(
    record: &[u8],
    ordinal: usize,
    ctx: &Context,
) -> Result<(ObjectView, String)> {
    let parts = record::parts(record, ctx.width)?;
    let class = match classes::kind(&parts.object.class) {
        Some(kind) => format!("{kind:?}"),
        None => uuid_string(&parts.object.class),
    };
    let mut view = base_view(&parts, ordinal, ctx)?;
    view.kind = Counted::Object(ObjectKind::Unlinked);
    Ok((view, class))
}

/// The writer stamp (`0xa0000026`) from the properties table among the prefix chunks, or 0.
pub(crate) fn writer_stamp(prefix: &crate::stream::frame::Prefix) -> u32 {
    const TCODE_PROPERTIES_TABLE: u32 = 0x1000_0014;
    const TCODE_OPENNURBS_VERSION: u32 = 0xa000_0026;
    fn find(c: &mut cursor::Cursor, width: Width, depth: usize) -> Option<u32> {
        while !c.is_empty() {
            let at = c.position();
            let (typecode, value) = c.chunk_header(width).ok()?;
            if typecode == TCODE_OPENNURBS_VERSION {
                return Some(value as u32);
            }
            if typecode & crate::stream::frame::TCODE_SHORT != 0 {
                continue;
            }
            let mut body = c.long_body(at, typecode, value).ok()?;
            if depth > 0
                && let Some(stamp) = find(&mut body, width, depth - 1)
            {
                return Some(stamp);
            }
        }
        None
    }
    prefix
        .spans
        .iter()
        .filter(|s| s.typecode == TCODE_PROPERTIES_TABLE)
        .find_map(|s| {
            let mut c = cursor::Cursor::new(&prefix.bytes[s.start..s.end]);
            let at = c.position();
            let (typecode, value) = c.chunk_header(prefix.width).ok()?;
            let mut body = c.long_body(at, typecode, value).ok()?;
            find(&mut body, prefix.width, 2)
        })
        .unwrap_or(0)
}

/// The archive version from the 32-byte header (right-justified decimal).
pub(crate) fn archive_version(header: &[u8]) -> u32 {
    header
        .get(24..32)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
