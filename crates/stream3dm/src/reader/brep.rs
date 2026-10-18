//! Breps (`ON_Brep::Read`): face count, planar faces and the cached render meshes.
//!
//! Major 3 (archives 2+ from openNURBS-based writers): eight anonymous chunks (2D curves, 3D
//! curves, surfaces, vertices, edges, trims, loops, faces), the bounding box, then (minor 1+)
//! one chunk of render meshes and one of analysis meshes, one optional mesh per face. Only the
//! surface and face arrays are opened, unless the Brep has no cached render mesh: then the
//! whole topology is read and the faces are tessellated ([`super::tessellate`]).
//!
//! Major 2 (Rhino 2, archive 2): no chunks and no vertices: the 2D trim curves, the 3D edge
//! curves (polycurves without class wrappers), the NURBS surfaces, then per face its loops and
//! trims (41-byte records: index, twin, edge flag and index, reversed against the edge,
//! continuity, monotonicity, 3D and 2D tolerances), then the render meshes. Trim i uses 2D curve
//! i, face i surface i. Without render meshes the faces are tessellated like major 3's.

use super::classes::{self, ClassKind};
use super::cursor::Cursor;
use super::curve::{self, Curve};
use super::mesh::{self, Mesh};
use super::record::embedded;
use super::surface;
use super::{Context, ReadError, Result, tessellate, topology};

const TCODE_ANONYMOUS: u32 = 0x4000_8000;

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Brep {
    pub faces: usize,
    pub planar_faces: usize,
    /// Render meshes in face order (faces without one are skipped).
    pub meshes: Vec<Mesh>,
    /// The meshes were made by tessellating, not cached in the file.
    pub generated: bool,
    /// Faces with neither a cached nor a generated mesh.
    pub undisplayed_faces: usize,
}

pub(crate) fn read(data: &[u8], ctx: &Context) -> Result<Brep> {
    let mut c = Cursor::new(data);
    let version = c.u8()?;
    match version >> 4 {
        3 => current(&mut c, version & 0x0f, ctx),
        2 => legacy(&mut c, version & 0x0f, ctx),
        _ => Err(ReadError::Unsupported("Brep version")),
    }
}

fn current(c: &mut Cursor, minor: u8, ctx: &Context) -> Result<Brep> {
    let width = ctx.width;
    let curves_2d = c.chunk(width, TCODE_ANONYMOUS)?;
    let curves_3d = c.chunk(width, TCODE_ANONYMOUS)?;
    let surfaces = c.chunk(width, TCODE_ANONYMOUS)?;
    let vertices = c.chunk(width, TCODE_ANONYMOUS)?;
    let edges = c.chunk(width, TCODE_ANONYMOUS)?;
    let trims = c.chunk(width, TCODE_ANONYMOUS)?;
    let loops = c.chunk(width, TCODE_ANONYMOUS)?;
    let faces = c.chunk(width, TCODE_ANONYMOUS)?;
    let planar_slots = surface_slots(surfaces, ctx)?;
    let face_surfaces = face_surfaces(faces)?;
    c.skip(48)?; // bounding box
    let mut out = Brep {
        faces: face_surfaces.len(),
        planar_faces: face_surfaces
            .iter()
            .filter(|&&si| {
                usize::try_from(si)
                    .ok()
                    .and_then(|si| planar_slots.get(si))
                    .copied()
                    .unwrap_or(false)
            })
            .count(),
        meshes: Vec::new(),
        generated: false,
        undisplayed_faces: 0,
    };
    if minor >= 1 {
        let mut meshes = c.chunk(width, TCODE_ANONYMOUS)?;
        out.meshes = face_meshes(&mut meshes, face_surfaces.len(), ctx)?;
    }
    if out.meshes.is_empty() && out.faces > 0 {
        out.undisplayed_faces = out.faces;
        if ctx.tessellate
            && let Some(scale) = ctx.mm_per_unit
        {
            let arrays = topology::Arrays {
                curves_2d,
                curves_3d,
                surfaces,
                vertices,
                edges,
                trims,
                loops,
                faces,
            };
            // A Brep whose topology cannot be read stays undisplayed, as without tessellation.
            if let Ok(t) = topology::read(arrays, ctx) {
                let tess = tessellate::tessellate(&t, scale);
                out.generated = !tess.meshes.is_empty();
                out.undisplayed_faces = tess.skipped.len();
                out.meshes = tess.meshes;
            }
        }
    }
    Ok(out)
}

/// For each slot of the surface array, whether it holds an analytic plane.
fn surface_slots(mut c: Cursor, ctx: &Context) -> Result<Vec<bool>> {
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported("surface array version"));
    }
    let n = c.i32()?.max(0) as usize;
    let mut out = Vec::with_capacity(n.min(c.remaining() / 4));
    for _ in 0..n {
        if c.i32()? != 1 {
            out.push(false);
            continue;
        }
        let planar = embedded(&mut c, ctx.width)?
            .is_some_and(|w| classes::kind(&w.class) == Some(ClassKind::PlaneSurface));
        out.push(planar);
    }
    Ok(out)
}

/// The surface slot of every face.
fn face_surfaces(mut c: Cursor) -> Result<Vec<i32>> {
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported("face array version"));
    }
    let n = c.count(20)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        c.i32()?; // face index
        c.skip_array(4)?; // loops
        out.push(c.i32()?);
        c.skip(8)?; // reversed flag, material channel
    }
    Ok(out)
}

/// One flag per face, each followed by an embedded mesh when set.
fn face_meshes(c: &mut Cursor, faces: usize, ctx: &Context) -> Result<Vec<Mesh>> {
    let mut out = Vec::new();
    for _ in 0..faces {
        if c.u8()? == 0 {
            continue;
        }
        if let Some(w) = embedded(c, ctx.width)? {
            if classes::kind(&w.class) != Some(ClassKind::Mesh) {
                return Err(ReadError::Invalid {
                    at: c.position(),
                    what: "render mesh slot is not a mesh",
                });
            }
            out.push(mesh::read(&w, ctx)?);
        }
    }
    Ok(out)
}

fn legacy(c: &mut Cursor, minor: u8, ctx: &Context) -> Result<Brep> {
    let topology = legacy_topology(c, ctx)?;
    let faces = topology.faces.len();
    let mut out = Brep {
        faces,
        // Planar faces are faces on ON_PlaneSurface; Rhino 2 Breps hold NURBS surfaces only.
        planar_faces: 0,
        meshes: Vec::new(),
        generated: false,
        undisplayed_faces: 0,
    };
    // openNURBS ignores failures from here on; so do we, keeping what was read.
    if let Ok(meshes) = face_meshes(c, faces, ctx) {
        out.meshes = meshes;
    }
    if out.meshes.is_empty() {
        out.undisplayed_faces = faces;
        if ctx.tessellate
            && let Some(scale) = ctx.mm_per_unit
        {
            let tess = tessellate::tessellate(&topology, scale);
            out.generated = !tess.meshes.is_empty();
            out.undisplayed_faces = tess.skipped.len();
            out.meshes = tess.meshes;
        }
    }
    let _ = minor;
    Ok(out)
}

/// The topology of a major-2 Brep, the cursor left at its render meshes.
fn legacy_topology(c: &mut Cursor, ctx: &Context) -> Result<topology::Topology> {
    let at = c.position();
    let counts = [c.i32()?, c.i32()?, c.i32()?, c.i32()?];
    if counts.iter().any(|&n| n < 1) {
        return Err(ReadError::Invalid {
            at,
            what: "legacy Brep counts",
        });
    }
    let [faces, edges, _loops, trims] = counts.map(|n| n as usize);
    c.i32()?; // outer flag
    c.skip(48)?; // bounding box
    // Trim i runs along 2D curve i; edge i is 3D curve i; face i lies on surface i.
    let curves_2d = (0..trims)
        .map(|_| raw_polycurve(c, ctx))
        .collect::<Result<Vec<_>>>()?;
    let curves = (0..edges)
        .map(|_| raw_polycurve(c, ctx))
        .collect::<Result<Vec<_>>>()?;
    let surfaces = (0..faces)
        .map(|_| {
            // Decoded from a copy of the cursor; the walk below steps over it either way.
            let mut probe = *c;
            let surface = surface::read_nurbs(&mut probe).ok();
            raw_nurbs_surface(c)?;
            Ok(surface)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut topology = topology::Topology {
        edges: curves
            .iter()
            .enumerate()
            .map(|(i, curve)| topology::Edge {
                curve: i as i32,
                reversed: false,
                proxy: curve.as_ref().map_or([0.0, 0.0], Curve::domain),
                tolerance: 0.0,
            })
            .collect(),
        curves_2d,
        curves,
        surfaces,
        vertices: Vec::new(),
        trims: Vec::with_capacity(trims),
        loops: Vec::new(),
        faces: Vec::with_capacity(faces),
    };
    for face in 0..faces {
        c.i32()?; // index
        c.i32()?; // material
        let reversed = c.i32()? != 0;
        c.i32()?; // type
        c.skip(48)?; // bounding box
        let loops = c.count(4)?;
        let mut face_loops = Vec::with_capacity(loops);
        for _ in 0..loops {
            c.i32()?; // index
            let kind = match c.i32()? {
                0 => topology::LoopKind::Outer,
                1 => topology::LoopKind::Inner,
                _ => topology::LoopKind::Other, // −1: a slit
            };
            c.skip(32)?; // 2D bounding box
            let n = c.count(41)?;
            let mut loop_trims = Vec::with_capacity(n);
            for _ in 0..n {
                c.i32()?; // index (the trim's place in the file's order)
                c.i32()?; // twin
                c.u8()?; // has an edge (the index below says so too)
                let edge = c.i32()?;
                let reversed = c.i32()? != 0;
                c.skip(4 + 4)?; // continuity, monotonicity
                let tolerance_3d = c.f64()?;
                c.f64()?; // 2D tolerance
                let edge = if usize::try_from(edge).is_ok_and(|e| e < edges) {
                    edge
                } else {
                    -1
                };
                if let Some(e) = usize::try_from(edge)
                    .ok()
                    .and_then(|e| topology.edges.get_mut(e))
                    && tolerance_3d.is_finite()
                    && tolerance_3d > e.tolerance
                {
                    e.tolerance = tolerance_3d;
                }
                let curve_2d = topology.trims.len() as i32;
                topology.trims.push(topology::Trim {
                    curve_2d,
                    proxy: topology
                        .curves_2d
                        .get(curve_2d as usize)
                        .and_then(Option::as_ref)
                        .map_or([0.0, 0.0], Curve::domain),
                    proxy_reversed: false,
                    edge,
                    vertices: [-1, -1],
                    reversed,
                });
                loop_trims.push(curve_2d);
            }
            face_loops.push(topology.loops.len() as i32);
            topology.loops.push(topology::Loop {
                trims: loop_trims,
                kind,
            });
        }
        topology.faces.push(topology::Face {
            loops: face_loops,
            surface: face as i32,
            reversed,
        });
    }
    Ok(topology)
}

/// `ON_PolyCurve::Read` without a class wrapper (Rhino 2 Brep curves); the curve when every
/// segment decodes.
fn raw_polycurve(c: &mut Cursor, ctx: &Context) -> Result<Option<Curve>> {
    c.u8()?;
    let n = c.count(1)?;
    c.skip(8 + 48)?; // reserved, bounding box
    let m = c.count(8)?;
    let params = (0..m).map(|_| c.f64()).collect::<Result<Vec<_>>>()?;
    let mut segments = Vec::with_capacity(n);
    for _ in 0..n {
        let segment = embedded(c, ctx.width)?.and_then(|w| curve::read(&w, ctx.width).ok());
        segments.push(segment);
    }
    let segments: Option<Vec<Curve>> = segments.into_iter().collect();
    Ok(segments
        .filter(|s| !s.is_empty() && params.len() == s.len() + 1)
        .map(|segments| Curve::Poly { segments, params }))
}

/// `ON_NurbsSurface::Read` without a class wrapper, skipped.
fn raw_nurbs_surface(c: &mut Cursor) -> Result<()> {
    c.u8()?;
    let at = c.position();
    let dim = c.i32()?;
    let rational = c.i32()?;
    c.skip(4 * 6)?; // orders, control point counts, reserved
    c.skip(48)?; // bounding box
    c.skip_array(8)?; // knots in u
    c.skip_array(8)?; // knots in v
    let stride = usize::try_from(dim)
        .ok()
        .filter(|&d| d <= 64)
        .ok_or(ReadError::Invalid {
            at,
            what: "surface dimension",
        })?
        + usize::from(rational != 0);
    c.skip_array(8 * stride)
}

/// One face seen both ways, for checking the tessellator against Rhino.
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) struct FaceCheck {
    pub cached: Option<Mesh>,
    pub ours: Option<Mesh>,
    /// Largest tolerance among the face's edges (model units).
    pub edge_tolerance: f64,
    /// The face lies in a plane.
    pub flat: bool,
}

/// For every face of a major-3 Brep: Rhino's cached render mesh and our tessellation (in
/// model units), side by side.
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) fn faces_both_ways(data: &[u8], ctx: &Context) -> Result<Vec<FaceCheck>> {
    let mut c = Cursor::new(data);
    let version = c.u8()?;
    if version >> 4 != 3 || version & 0x0f < 1 {
        return Err(ReadError::Unsupported("Brep version"));
    }
    let width = ctx.width;
    let arrays = topology::Arrays {
        curves_2d: c.chunk(width, TCODE_ANONYMOUS)?,
        curves_3d: c.chunk(width, TCODE_ANONYMOUS)?,
        surfaces: c.chunk(width, TCODE_ANONYMOUS)?,
        vertices: c.chunk(width, TCODE_ANONYMOUS)?,
        edges: c.chunk(width, TCODE_ANONYMOUS)?,
        trims: c.chunk(width, TCODE_ANONYMOUS)?,
        loops: c.chunk(width, TCODE_ANONYMOUS)?,
        faces: c.chunk(width, TCODE_ANONYMOUS)?,
    };
    c.skip(48)?;
    let t = topology::read(arrays, ctx)?;
    let mut meshes = c.chunk(width, TCODE_ANONYMOUS)?;
    let unit = Context {
        mm_per_unit: Some(1.0),
        ..*ctx
    };
    let mut earcut = earcut::Earcut::new();
    let mut out = Vec::with_capacity(t.faces.len());
    for face in &t.faces {
        let cached = if meshes.u8()? != 0 {
            embedded(&mut meshes, width)?
                .map(|w| mesh::read(&w, &unit))
                .transpose()?
        } else {
            None
        };
        let edge_tolerance = face
            .loops
            .iter()
            .filter_map(|&l| t.loops.get(usize::try_from(l).ok()?))
            .flat_map(|l| l.trims.iter())
            .filter_map(|&ti| t.trims.get(usize::try_from(ti).ok()?))
            .filter_map(|trim| t.edges.get(usize::try_from(trim.edge).ok()?))
            .map(|e| e.tolerance)
            .fold(0.0, f64::max);
        let flat = usize::try_from(face.surface)
            .ok()
            .and_then(|i| t.surfaces.get(i))
            .and_then(Option::as_ref)
            .is_some_and(|s| s.frame(tessellate::FLAT_TOLERANCE).is_some());
        out.push(FaceCheck {
            cached,
            ours: tessellate::face_mesh(&t, face, &mut earcut).ok(),
            edge_tolerance,
            flat,
        });
    }
    Ok(out)
}

/// Why each face of a major-3 Brep is not tessellated (`None` for faces that are).
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) fn skip_reasons(
    data: &[u8],
    ctx: &Context,
) -> Result<Vec<(Option<tessellate::Skip>, String)>> {
    let mut c = Cursor::new(data);
    let t = match c.u8()? >> 4 {
        2 => legacy_topology(&mut c, ctx)?,
        3 => {
            let width = ctx.width;
            let arrays = topology::Arrays {
                curves_2d: c.chunk(width, TCODE_ANONYMOUS)?,
                curves_3d: c.chunk(width, TCODE_ANONYMOUS)?,
                surfaces: c.chunk(width, TCODE_ANONYMOUS)?,
                vertices: c.chunk(width, TCODE_ANONYMOUS)?,
                edges: c.chunk(width, TCODE_ANONYMOUS)?,
                trims: c.chunk(width, TCODE_ANONYMOUS)?,
                loops: c.chunk(width, TCODE_ANONYMOUS)?,
                faces: c.chunk(width, TCODE_ANONYMOUS)?,
            };
            topology::read(arrays, ctx)?
        }
        _ => return Err(ReadError::Unsupported("Brep version")),
    };
    let mut earcut = earcut::Earcut::new();
    Ok(t.faces
        .iter()
        .map(|f| {
            let surface = usize::try_from(f.surface)
                .ok()
                .and_then(|i| t.surfaces.get(i))
                .and_then(Option::as_ref)
                .map_or_else(|| "none".to_string(), |s| s.describe());
            (tessellate::face_mesh(&t, f, &mut earcut).err(), surface)
        })
        .collect())
}

/// Diagnostics for face `index` of a major-3 Brep.
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) fn describe_face(data: &[u8], ctx: &Context, index: usize) -> Result<String> {
    let mut c = Cursor::new(data);
    c.u8()?;
    let width = ctx.width;
    let arrays = topology::Arrays {
        curves_2d: c.chunk(width, TCODE_ANONYMOUS)?,
        curves_3d: c.chunk(width, TCODE_ANONYMOUS)?,
        surfaces: c.chunk(width, TCODE_ANONYMOUS)?,
        vertices: c.chunk(width, TCODE_ANONYMOUS)?,
        edges: c.chunk(width, TCODE_ANONYMOUS)?,
        trims: c.chunk(width, TCODE_ANONYMOUS)?,
        loops: c.chunk(width, TCODE_ANONYMOUS)?,
        faces: c.chunk(width, TCODE_ANONYMOUS)?,
    };
    let t = topology::read(arrays, ctx)?;
    Ok(t.faces.get(index).map_or_else(
        || "no face".to_string(),
        |f| tessellate::describe_face(&t, f),
    ))
}
