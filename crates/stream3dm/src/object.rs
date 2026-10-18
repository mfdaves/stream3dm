//! A decoder-neutral description of one object record. Either decoder (cadmpeg, or our own
//! reader) produces these; [`crate::extract::Facts::absorb_object`] turns them into report
//! facts, so the report's meaning does not depend on which decoder read the file.

use crate::native::{Matrix, ObjectKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    Object(ObjectKind),
    BlockInstance,
}

/// One object record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObjectView {
    pub uuid: Option<String>,
    /// Identity used when the object has no UUID.
    pub fallback_key: String,
    pub name: Option<String>,
    pub layer_index: Option<i64>,
    pub material_index: Option<i64>,
    pub material_source: Option<i64>,
    pub visible: Option<bool>,
    pub mode: Option<i64>,
    pub groups: Vec<i64>,
    pub user_strings: Vec<(String, String)>,
    pub kind: Counted,
    pub meshes: Vec<MeshGeom>,
    /// Curves of the object, each sampled into a polyline (millimetres, unplaced).
    pub curves: Vec<Vec<[f64; 3]>>,
    /// Points of the object: a point, or a point cloud (millimetres, unplaced).
    pub points: Vec<[f64; 3]>,
    pub instance: Option<InstanceView>,
    /// Brep faces of the object, and how many lie on analytic planes.
    pub faces: usize,
    pub planar_faces: usize,
    /// The meshes were tessellated by us (the file had no cached render mesh).
    pub generated_mesh: bool,
    /// Brep faces with no mesh at all, cached or generated.
    pub undisplayed_faces: usize,
    /// IFC class (`IfcWall`, …), for IFC products.
    pub class: Option<String>,
    /// IFC type name, for IFC products that have a type (shown like a block).
    pub type_name: Option<String>,
}

/// A block placement read from an instance reference.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InstanceView {
    pub definition: String,
    /// Placement with translation in millimetres; `None` when it could not be read.
    pub matrix: Option<Matrix>,
}

/// One displayable mesh of an object, positions in millimetres, unplaced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MeshGeom {
    pub positions_mm: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
    pub vertex_count: usize,
    pub triangle_count: usize,
    /// Already placed by the decoder's own block expansion (whole-file decodes only).
    pub in_instance: bool,
    /// Visibility and layer from the mesh itself, used when it has no object.
    pub source_visible: Option<bool>,
    pub source_layer: Option<String>,
    /// Identity of the mesh, for objects without a UUID.
    pub key: String,
    /// Display colour given with the geometry (IFC: the engine's colour of the part).
    pub color: Option<[u8; 3]>,
}

/// Whether two triangle lists differ only in quads split along the other diagonal where both
/// diagonals have the same length to within rounding.
#[cfg(all(test, feature = "cadmpeg"))]
fn same_up_to_diagonals(e: &MeshGeom, a: &MeshGeom) -> bool {
    if e.triangles.len() != a.triangles.len() {
        return false;
    }
    let p = &e.positions_mm;
    let length = |i: u32, j: u32| {
        let (u, v) = (p[i as usize], p[j as usize]);
        ((u[0] - v[0]).powi(2) + (u[1] - v[1]).powi(2) + (u[2] - v[2]).powi(2)).sqrt()
    };
    let mut k = 0;
    while k < e.triangles.len() {
        if e.triangles[k] == a.triangles[k] {
            k += 1;
            continue;
        }
        let (Some(e2), Some(a2)) = (e.triangles.get(k + 1), a.triangles.get(k + 1)) else {
            return false;
        };
        let mut quad_e: Vec<u32> = e.triangles[k].iter().chain(e2).copied().collect();
        let mut quad_a: Vec<u32> = a.triangles[k].iter().chain(a2).copied().collect();
        quad_e.sort_unstable();
        quad_e.dedup();
        quad_a.sort_unstable();
        quad_a.dedup();
        // A split quad (a, b, c, d) is (a, b, c) + (a, c, d) or (a, b, d) + (b, c, d).
        let corners = |t: &[u32; 3], u: &[u32; 3]| -> Option<[u32; 4]> {
            if t[0] == u[0] && t[2] == u[1] {
                Some([t[0], t[1], t[2], u[2]])
            } else if t[1] == u[0] && t[2] == u[2] {
                Some([t[0], t[1], u[1], t[2]])
            } else {
                None
            }
        };
        let Some(q) = corners(&e.triangles[k], e2) else {
            return false;
        };
        let (d02, d13) = (length(q[0], q[2]), length(q[1], q[3]));
        if quad_e != quad_a || quad_e.len() != 4 || (d02 - d13).abs() > 1e-9 * d02.max(d13) {
            return false;
        }
        k += 2;
    }
    true
}

/// Differences between two views of the same record, for parity tests. Positions and matrices
/// are compared with a relative tolerance; everything else exactly.
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) fn differences(expected: &ObjectView, actual: &ObjectView) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! same {
        ($field:ident) => {
            if expected.$field != actual.$field {
                out.push(format!(
                    "{}: {:?} vs {:?}",
                    stringify!($field),
                    expected.$field,
                    actual.$field
                ));
            }
        };
    }
    same!(uuid);
    same!(name);
    same!(layer_index);
    same!(material_index);
    same!(material_source);
    same!(visible);
    same!(groups);
    same!(user_strings);
    same!(kind);
    same!(faces);
    same!(planar_faces);
    if expected.mode.map(|m| m & 0x0f) != actual.mode.map(|m| m & 0x0f) {
        out.push(format!("mode: {:?} vs {:?}", expected.mode, actual.mode));
    }
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
    match (&expected.instance, &actual.instance) {
        (None, None) => {}
        (Some(e), Some(a)) => {
            if e.definition != a.definition {
                out.push(format!(
                    "instance definition: {} vs {}",
                    e.definition, a.definition
                ));
            }
            let matrices_match = match (e.matrix, a.matrix) {
                (Some(m), Some(n)) => (0..4).all(|r| (0..4).all(|c| close(m[r][c], n[r][c]))),
                (None, None) => true,
                _ => false,
            };
            if !matrices_match {
                out.push(format!("instance matrix: {:?} vs {:?}", e.matrix, a.matrix));
            }
        }
        (e, a) => out.push(format!("instance: {e:?} vs {a:?}")),
    }
    if expected.meshes.len() != actual.meshes.len() {
        out.push(format!(
            "meshes: {} vs {}",
            expected.meshes.len(),
            actual.meshes.len()
        ));
    } else {
        // Mesh order within an object carries no meaning (cadmpeg sorts them by id text).
        let key = |m: &MeshGeom| {
            let positions: Vec<[i64; 3]> = m
                .positions_mm
                .iter()
                .map(|p| p.map(|v| (v * 1e3).round() as i64))
                .collect();
            (m.vertex_count, m.triangle_count, positions)
        };
        let mut expected_meshes: Vec<&MeshGeom> = expected.meshes.iter().collect();
        let mut actual_meshes: Vec<&MeshGeom> = actual.meshes.iter().collect();
        expected_meshes.sort_by_cached_key(|m| key(m));
        actual_meshes.sort_by_cached_key(|m| key(m));
        for (i, (e, a)) in expected_meshes.into_iter().zip(actual_meshes).enumerate() {
            if (e.vertex_count, e.triangle_count) != (a.vertex_count, a.triangle_count) {
                out.push(format!(
                    "mesh {i}: {}v/{}t vs {}v/{}t",
                    e.vertex_count, e.triangle_count, a.vertex_count, a.triangle_count
                ));
                continue;
            }
            if e.triangles != a.triangles && !same_up_to_diagonals(e, a) {
                out.push(format!("mesh {i}: triangles differ"));
            }
            if let Some(k) = (0..e.positions_mm.len().min(a.positions_mm.len()))
                .find(|&k| (0..3).any(|j| !close(e.positions_mm[k][j], a.positions_mm[k][j])))
            {
                out.push(format!(
                    "mesh {i}: vertex {k} {:?} vs {:?}",
                    e.positions_mm[k], a.positions_mm[k]
                ));
            }
        }
    }
    out
}
