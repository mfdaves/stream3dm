//! Brep topology (major 3): vertices, edges, trims, loops and faces, with the 3D edge curves
//! and the surfaces they refer to. Read only for Breps that need tessellating; also made for
//! Rhino 2 Breps ([`super::brep`]) and for surface objects ([`untrimmed`]).
//!
//! Each array is an anonymous chunk: version byte, count, records. Edge records carry an edge
//! domain, and trim records a trim domain instead of two legacy points, when the file was
//! written by openNURBS from 2002-06-18 on (archive 3+).

use super::cursor::Cursor;
use super::curve::{self, Curve};
use super::record::embedded;
use super::surface::{self, Surface};
use super::{Context, Result};

/// Writer stamp from which edges and trims carry their own domains.
const DOMAIN_STAMP: u32 = 200_206_180;

pub(crate) struct Topology {
    /// 2D trim curves, in the surfaces' parameter spaces.
    pub curves_2d: Vec<Option<Curve>>,
    pub curves: Vec<Option<Curve>>,
    pub surfaces: Vec<Option<Surface>>,
    pub vertices: Vec<[f64; 3]>,
    pub edges: Vec<Edge>,
    pub trims: Vec<Trim>,
    pub loops: Vec<Loop>,
    pub faces: Vec<Face>,
}

pub(crate) struct Edge {
    pub curve: i32,
    /// The edge runs against its curve.
    pub reversed: bool,
    /// Part of the curve's domain the edge uses.
    pub proxy: [f64; 2],
    /// How far the trims of the faces may stray from the edge (0 when unset).
    #[cfg_attr(not(all(test, feature = "cadmpeg")), allow(dead_code))]
    // read by the checks against Rhino's meshes
    pub tolerance: f64,
}

pub(crate) struct Trim {
    /// Its 2D curve in the face's surface parameters.
    pub curve_2d: i32,
    /// Part of the 2D curve's domain the trim uses.
    pub proxy: [f64; 2],
    /// The trim runs against its 2D curve.
    pub proxy_reversed: bool,
    /// −1 for a singular trim (a point in 3D, such as a sphere's pole).
    pub edge: i32,
    pub vertices: [i32; 2],
    /// The trim runs against its edge.
    pub reversed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopKind {
    Outer,
    Inner,
    Other,
}

pub(crate) struct Loop {
    pub trims: Vec<i32>,
    pub kind: LoopKind,
}

pub(crate) struct Face {
    pub loops: Vec<i32>,
    pub surface: i32,
    pub reversed: bool,
}

/// The eight array chunks of a major-3 Brep, in file order.
pub(crate) struct Arrays<'a> {
    pub curves_2d: Cursor<'a>,
    pub curves_3d: Cursor<'a>,
    pub surfaces: Cursor<'a>,
    pub vertices: Cursor<'a>,
    pub edges: Cursor<'a>,
    pub trims: Cursor<'a>,
    pub loops: Cursor<'a>,
    pub faces: Cursor<'a>,
}

pub(crate) fn read(a: Arrays, ctx: &Context) -> Result<Topology> {
    let width = ctx.width;
    let domains = ctx.archive >= 3 && ctx.stamp >= DOMAIN_STAMP;
    let curves_2d = objects(a.curves_2d, |w| curve::read(w, width).ok(), ctx)?;
    let curves = objects(a.curves_3d, |w| curve::read(w, width).ok(), ctx)?;
    let surfaces = objects(a.surfaces, |w| surface::read(w, width).ok(), ctx)?;
    let vertices = records(a.vertices, |c| {
        c.i32()?;
        let p = c.point3()?;
        c.skip_array(4)?;
        c.f64()?;
        Ok(p)
    })?;
    let edges = records(a.edges, |c| {
        c.i32()?;
        let curve = c.i32()?;
        let reversed = c.i32()? != 0;
        let proxy = [c.f64()?, c.f64()?];
        c.skip(8)?; // vertices
        c.skip_array(4)?;
        let tolerance = c.f64()?;
        let tolerance = if tolerance.is_finite() && tolerance > 0.0 {
            tolerance
        } else {
            0.0
        };
        if domains {
            c.skip(16)?;
        }
        Ok(Edge {
            curve,
            reversed,
            proxy,
            tolerance,
        })
    })?;
    let trims = records(a.trims, |c| {
        c.i32()?; // index
        let curve_2d = c.i32()?;
        let proxy = [c.f64()?, c.f64()?];
        let edge = c.i32()?;
        let vertices = [c.i32()?, c.i32()?];
        let reversed = c.i32()? != 0;
        c.skip(4 + 4 + 4 + 16)?; // type, iso, loop, tolerances
        let proxy_reversed = if domains {
            c.skip(16)?; // trim domain
            let flag = c.u8()? == 1;
            c.skip(7 + 24)?;
            flag
        } else {
            c.skip(48)?; // legacy points
            false
        };
        c.skip(16)?; // legacy tolerances
        Ok(Trim {
            curve_2d,
            proxy,
            proxy_reversed,
            edge,
            vertices,
            reversed,
        })
    })?;
    let loops = records(a.loops, |c| {
        c.i32()?;
        let trims = c.i32_array()?;
        let kind = match c.i32()? {
            1 => LoopKind::Outer,
            2 => LoopKind::Inner,
            _ => LoopKind::Other,
        };
        c.i32()?;
        Ok(Loop { trims, kind })
    })?;
    let faces = records(a.faces, |c| {
        c.i32()?;
        let loops = c.i32_array()?;
        let surface = c.i32()?;
        let reversed = c.i32()? != 0;
        c.i32()?;
        Ok(Face {
            loops,
            surface,
            reversed,
        })
    })?;
    Ok(Topology {
        curves_2d,
        curves,
        surfaces,
        vertices,
        edges,
        trims,
        loops,
        faces,
    })
}

/// A one-face topology for a surface object, which has no trims of its own: the outer loop is
/// its domain's boundary, each side sampled at the surface's grid parameters, as a 2D curve and
/// as the 3D curve of its edge, so the face tessellates like a Brep face.
pub(crate) fn untrimmed(surface: Surface) -> Topology {
    let [du, dv] = surface.domain();
    let us = surface.grid_parameters(0, du);
    let vs = surface.grid_parameters(1, dv);
    // Counter-clockwise in parameter space.
    let sides: [Vec<[f64; 2]>; 4] = [
        us.iter().map(|&u| [u, dv[0]]).collect(),
        vs.iter().map(|&v| [du[1], v]).collect(),
        us.iter().rev().map(|&u| [u, dv[1]]).collect(),
        vs.iter().rev().map(|&v| [du[0], v]).collect(),
    ];
    let mut t = Topology {
        curves_2d: Vec::new(),
        curves: Vec::new(),
        surfaces: Vec::new(),
        vertices: Vec::new(),
        edges: Vec::new(),
        trims: Vec::new(),
        loops: Vec::new(),
        faces: Vec::new(),
    };
    for (i, side) in sides.iter().enumerate() {
        let params: Vec<f64> = (0..side.len()).map(|k| k as f64).collect();
        let proxy = [0.0, (side.len() - 1) as f64];
        t.curves_2d.push(Some(Curve::Polyline {
            points: side.iter().map(|p| [p[0], p[1], 0.0]).collect(),
            params: params.clone(),
        }));
        t.curves.push(Some(Curve::Polyline {
            points: side.iter().map(|p| surface.point_at(p[0], p[1])).collect(),
            params,
        }));
        t.edges.push(Edge {
            curve: i as i32,
            reversed: false,
            proxy,
            tolerance: 0.0,
        });
        t.trims.push(Trim {
            curve_2d: i as i32,
            proxy,
            proxy_reversed: false,
            edge: i as i32,
            vertices: [-1, -1],
            reversed: false,
        });
    }
    t.loops.push(Loop {
        trims: vec![0, 1, 2, 3],
        kind: LoopKind::Outer,
    });
    t.faces.push(Face {
        loops: vec![0],
        surface: 0,
        reversed: false,
    });
    t.surfaces.push(Some(surface));
    t
}

/// An array of fixed-layout records: version byte, count, records.
fn records<T>(mut c: Cursor, mut item: impl FnMut(&mut Cursor) -> Result<T>) -> Result<Vec<T>> {
    c.u8()?;
    let n = c.count(4)?;
    (0..n).map(|_| item(&mut c)).collect()
}

/// An array of embedded objects: version byte, count, then per slot a flag (1 = object).
fn objects<T>(
    mut c: Cursor,
    decode: impl Fn(&super::record::Wrapper) -> Option<T>,
    ctx: &Context,
) -> Result<Vec<Option<T>>> {
    c.u8()?;
    let n = c.count(4)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if c.i32()? != 1 {
            out.push(None);
            continue;
        }
        out.push(embedded(&mut c, ctx.width)?.and_then(|w| decode(&w)));
    }
    Ok(out)
}
