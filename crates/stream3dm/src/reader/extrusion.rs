//! Extrusions (`ON_Extrusion::Read`): the cached render mesh, or one made here from the profile
//! and path when the file has none.
//!
//! The class data is one versioned anonymous chunk: the profile (an embedded curve), the path
//! (a line), the part of it used, the up vector, two miter flags and normals, the path domain,
//! a transposed flag, the profile count (minor 1+), two cap flags (minor 2+; before, both ends
//! are capped when the profiles are closed), then (minor 3+, archives 60+) a mesh cache: items
//! keyed by mesh UUID, each holding an embedded mesh. Archive-50 files keep the meshes instead
//! in `ON_V5ExtrusionDisplayMeshCache` userdata on the object: render, analysis, null.
//!
//! Geometry (openNURBS): the profile lies in the XY plane of a frame at the path's start, with
//! Z along the path, Y the up vector and X = Y × Z; the same profile at the path's end closes
//! the sides. Several profiles are the segments of a polycurve, the outer one first
//! (counter-clockwise), then the holes (clockwise). A miter tilts an end: its normal, given in
//! the frame, is used when its Z exceeds 1/64, and the profile is projected along the path onto
//! that plane.

use super::cursor::Cursor;
use super::curve::{self, Curve};
use super::mesh::{self, Mesh};
use super::record::{Wrapper, embedded};
use super::{Context, ReadError, Result};

const TCODE_ANONYMOUS: u32 = 0x4000_8000;
const TCODE_USERDATA_HEADER: u32 = 0x0002_fff9;
/// Miter normals with a smaller Z component are ignored, as openNURBS does.
const MITER_Z_MINIMUM: f64 = 1.0 / 64.0;
/// A profile is closed when its ends lie within this share of its size of each other.
const CLOSED_TOLERANCE: f64 = 1e-9;

/// `ON_MeshCache` render mesh id (`f5e3baa9-a7a2-49fd-b8a1-66eb274a5f91`) as stored.
const RENDER_MESH_ID: [u8; 16] = [
    0xa9, 0xba, 0xe3, 0xf5, 0xa2, 0xa7, 0xfd, 0x49, 0xb8, 0xa1, 0x66, 0xeb, 0x27, 0x4a, 0x5f, 0x91,
];
/// `ON_V5ExtrusionDisplayMeshCache` userdata class id (`a8130a3e-e4f3-4cb0-bb8a-f10a473912d0`).
const V5_MESH_CACHE: [u8; 16] = [
    0x3e, 0x0a, 0x13, 0xa8, 0xf3, 0xe4, 0xb0, 0x4c, 0xbb, 0x8a, 0xf1, 0x0a, 0x47, 0x39, 0x12, 0xd0,
];

pub(crate) struct Extrusion {
    /// Number of end caps.
    pub caps: usize,
    /// The cached render mesh, or the mesh made here.
    pub mesh: Option<Mesh>,
    /// The mesh was made here (the file had none).
    pub generated: bool,
}

/// What defines an extrusion's shape, in model units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shape {
    pub profile: Curve,
    pub profile_count: usize,
    pub from: [f64; 3],
    pub to: [f64; 3],
    /// Part of the path used, as fractions of the line.
    pub t: [f64; 2],
    pub up: [f64; 3],
    /// Miter normals in the profile frame, when present.
    pub miters: [Option<[f64; 3]>; 2],
    pub transposed: bool,
    /// Caps at the start and at the end; `None` when not stored (then: closed profiles).
    pub caps: Option<[bool; 2]>,
}

pub(crate) fn read(w: &Wrapper, ctx: &Context) -> Result<Extrusion> {
    let width = ctx.width;
    let mut outer = Cursor::new(w.data);
    let mut c = outer.chunk(width, TCODE_ANONYMOUS)?;
    if c.i32()? != 1 {
        return Err(ReadError::Unsupported("extrusion version"));
    }
    let minor = c.i32()?;
    let profile = embedded(&mut c, width)?;
    let from = c.point3()?;
    let to = c.point3()?;
    let t = [c.f64()?, c.f64()?];
    let up = c.point3()?;
    let have = [c.bool8()?, c.bool8()?];
    let normals = [c.point3()?, c.point3()?];
    c.skip(16)?; // path domain
    let transposed = c.bool8()?;
    let profile_count = if minor >= 1 { c.i32()? } else { 1 };
    let stored_caps = if minor >= 2 {
        Some([c.bool8()?, c.bool8()?])
    } else {
        None
    };
    let cached = if minor >= 3 {
        cache(c.chunk(width, TCODE_ANONYMOUS)?, ctx)?
    } else {
        v5_cache(w, ctx)?
    };
    // A profile that cannot be read leaves the extrusion without a generated mesh.
    let shape = profile
        .and_then(|p| curve::read(&p, width).ok())
        .map(|profile| Shape {
            profile,
            profile_count: usize::try_from(profile_count).unwrap_or(0),
            from,
            to,
            t,
            up,
            miters: [0, 1].map(|i| have[i].then_some(normals[i])),
            transposed,
            caps: stored_caps,
        });
    let caps = match (stored_caps, &shape) {
        (Some([a, b]), _) => usize::from(a) + usize::from(b),
        (None, Some(s)) if s.closed() => 2,
        (None, _) => 0,
    };
    let mut out = Extrusion {
        caps,
        mesh: cached,
        generated: false,
    };
    if out.mesh.is_none()
        && ctx.tessellate
        && let (Some(scale), Some(shape)) = (ctx.mm_per_unit, &shape)
        && let Some(mut m) = shape.mesh()
    {
        for p in &mut m.positions_mm {
            *p = p.map(|v| v * scale);
        }
        out.mesh = Some(m);
        out.generated = true;
    }
    Ok(out)
}

impl Shape {
    /// The profiles: the profile itself, or the segments of a polycurve holding several.
    fn profiles(&self) -> Option<Vec<&Curve>> {
        match (&self.profile, self.profile_count) {
            (p, 1) => Some(vec![p]),
            (Curve::Poly { segments, .. }, n) if n > 1 && segments.len() == n => {
                Some(segments.iter().collect())
            }
            _ => None,
        }
    }

    /// Whether every profile is closed.
    fn closed(&self) -> bool {
        self.profiles()
            .is_some_and(|ps| ps.iter().all(|p| ring(p).1))
    }

    /// The mesh of the sides and caps, in model units; `None` when the shape is degenerate.
    pub(crate) fn mesh(&self) -> Option<Mesh> {
        let profiles = self.profiles()?;
        let path = sub(self.to, self.from);
        let z = unit(path)?;
        let y = unit(self.up)?;
        if dot(y, z).abs() > 1e-6 {
            return None;
        }
        let x = unit(cross(y, z))?;
        let origins = self.t.map(|t| add(self.from, scale(path, t)));
        let miters = self
            .miters
            .map(|n| n.and_then(unit).filter(|n| n[2] > MITER_Z_MINIMUM));
        // A profile point (local x, y) at end `e`, projected along the path onto its miter.
        let place = |p: [f64; 2], e: usize| {
            let lz = miters[e].map_or(0.0, |n| -(n[0] * p[0] + n[1] * p[1]) / n[2]);
            add(
                origins[e],
                add(scale(x, p[0]), add(scale(y, p[1]), scale(z, lz))),
            )
        };

        let rings: Vec<(Vec<[f64; 2]>, bool)> = profiles.iter().map(|p| ring(p)).collect();
        if rings
            .iter()
            .any(|(r, closed)| r.len() < if *closed { 3 } else { 2 })
        {
            return None;
        }
        let all_closed = rings.iter().all(|(_, closed)| *closed);
        let caps = self.caps.unwrap_or([all_closed; 2]);
        let capped = all_closed && (caps[0] || caps[1]);
        let mut positions = Vec::new();
        let mut triangles = Vec::new();

        // Sides: one quad per profile segment. They face along the surface's natural normal
        // (profile direction × path), flipped when transposed; a capped solid faces outward
        // instead, the outer profile counter-clockwise and the holes clockwise.
        let outer = outer_ring(&rings);
        for (k, (r, closed)) in rings.iter().enumerate() {
            let flip = if capped {
                (area(r) > 0.0) != (k == outer)
            } else {
                self.transposed
            };
            let base = positions.len() as u32;
            let n = r.len() as u32;
            positions.extend(r.iter().map(|&p| place(p, 0)));
            positions.extend(r.iter().map(|&p| place(p, 1)));
            let segments = if *closed { n } else { n - 1 };
            for i in 0..segments {
                let j = (i + 1) % n;
                let (s0, s1, e0, e1) = (base + i, base + j, base + n + i, base + n + j);
                if flip {
                    triangles.extend([[s0, e1, s1], [s0, e0, e1]]);
                } else {
                    triangles.extend([[s0, s1, e1], [s0, e1, e0]]);
                }
            }
        }

        // Caps: the outer profile with its holes, triangulated in the profile plane; the start
        // faces against the path, the end along it.
        if capped {
            let mut order = vec![outer];
            order.extend((0..rings.len()).filter(|&k| k != outer));
            let mut flat = Vec::new();
            let mut holes = Vec::new();
            for (n, &k) in order.iter().enumerate() {
                if n > 0 {
                    holes.push(flat.len() as u32);
                }
                flat.extend_from_slice(&rings[k].0);
            }
            let mut indices: Vec<u32> = Vec::new();
            earcut::Earcut::new().earcut(flat.iter().copied(), &holes, &mut indices);
            for (e, _) in caps.iter().enumerate().filter(|(_, cap)| **cap) {
                let base = positions.len() as u32;
                positions.extend(flat.iter().map(|&p| place(p, e)));
                for t in indices.chunks_exact(3) {
                    let [a, b, c] = [t[0], t[1], t[2]];
                    let ccw =
                        signed_area2(flat[a as usize], flat[b as usize], flat[c as usize]) > 0.0;
                    // Counter-clockwise in the profile plane faces along the path.
                    triangles.push(if ccw == (e == 1) {
                        [base + a, base + b, base + c]
                    } else {
                        [base + a, base + c, base + b]
                    });
                }
            }
        }
        if triangles.is_empty() || positions.iter().flatten().any(|v| !v.is_finite()) {
            return None;
        }
        Some(Mesh {
            positions_mm: positions,
            triangles,
            dropped_faces: 0,
        })
    }
}

/// A profile sampled in its plane, and whether it is closed (then without the repeated end).
fn ring(profile: &Curve) -> (Vec<[f64; 2]>, bool) {
    let [t0, t1] = profile.domain();
    let mut points: Vec<[f64; 2]> = profile
        .sample(t0, t1)
        .into_iter()
        .map(|p| [p[0], p[1]])
        .collect();
    let size = points.iter().fold(0.0f64, |m, p| {
        let d = [p[0] - points[0][0], p[1] - points[0][1]];
        m.max((d[0] * d[0] + d[1] * d[1]).sqrt())
    });
    let closed = points.len() > 2
        && points.first().zip(points.last()).is_some_and(|(a, b)| {
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
                <= CLOSED_TOLERANCE * size.max(1.0)
        });
    if closed {
        points.pop();
    }
    (points, closed)
}

/// The ring enclosing the most area (the outer profile of a valid extrusion is the first).
fn outer_ring(rings: &[(Vec<[f64; 2]>, bool)]) -> usize {
    (0..rings.len())
        .max_by(|&a, &b| area(&rings[a].0).abs().total_cmp(&area(&rings[b].0).abs()))
        .unwrap_or(0)
}

/// Signed area of a closed ring (positive when counter-clockwise).
fn area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum::<f64>()
        / 2.0
}

fn signed_area2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit(a: [f64; 3]) -> Option<[f64; 3]> {
    let len = dot(a, a).sqrt();
    (len.is_finite() && len > 0.0).then(|| scale(a, 1.0 / len))
}

/// `ON_MeshCache`: version, then items each announced by a marker byte 1; 0 ends the list.
/// The render mesh is chosen by its id; another cached mesh is used when it is missing.
fn cache(mut c: Cursor, ctx: &Context) -> Result<Option<Mesh>> {
    if c.i32()? != 1 {
        return Err(ReadError::Unsupported("mesh cache version"));
    }
    c.i32()?;
    let mut fallback = None;
    while c.u8()? == 1 {
        let mut item = c.chunk(ctx.width, TCODE_ANONYMOUS)?;
        if item.i32()? != 1 {
            continue;
        }
        item.i32()?;
        let id = item.uuid()?;
        let Some(w) = embedded(&mut item, ctx.width)? else {
            continue;
        };
        if id == RENDER_MESH_ID {
            return mesh::read(&w, ctx).map(Some);
        }
        if fallback.is_none() {
            fallback = Some(mesh::read(&w, ctx)?);
        }
    }
    Ok(fallback)
}

fn v5_cache(w: &Wrapper, ctx: &Context) -> Result<Option<Mesh>> {
    let width = ctx.width;
    for body in &w.userdata {
        let mut c = Cursor::new(body);
        if c.u8()? >> 4 != 2 {
            continue;
        }
        if c.chunk(width, TCODE_USERDATA_HEADER)?.uuid()? != V5_MESH_CACHE {
            continue;
        }
        let mut payload = c.chunk(width, TCODE_ANONYMOUS)?;
        for _ in 0..2 {
            if let Some(m) = embedded(&mut payload, width)? {
                return mesh::read(&m, ctx).map(Some);
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polyline(points: &[[f64; 2]]) -> Curve {
        Curve::Polyline {
            points: points.iter().map(|p| [p[0], p[1], 0.0]).collect(),
            params: (0..points.len()).map(|i| i as f64).collect(),
        }
    }

    fn circle(r: f64) -> Curve {
        Curve::Arc {
            origin: [0.0; 3],
            x: [1.0, 0.0, 0.0],
            y: [0.0, 1.0, 0.0],
            radius: r,
            angles: [0.0, std::f64::consts::TAU],
            domain: [0.0, 1.0],
        }
    }

    fn shape(profile: Curve, profile_count: usize) -> Shape {
        Shape {
            profile,
            profile_count,
            from: [0.0; 3],
            to: [0.0, 0.0, 3.0],
            t: [0.0, 1.0],
            up: [0.0, 1.0, 0.0],
            miters: [None, None],
            transposed: false,
            caps: Some([true, true]),
        }
    }

    /// Volume enclosed by a closed, consistently oriented mesh (positive when it faces out).
    fn volume(m: &Mesh) -> f64 {
        m.triangles
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| m.positions_mm[i as usize]);
                dot(a, cross(b, c)) / 6.0
            })
            .sum()
    }

    fn bounds(m: &Mesh) -> ([f64; 3], [f64; 3]) {
        m.positions_mm
            .iter()
            .fold(([f64::MAX; 3], [f64::MIN; 3]), |(lo, hi), p| {
                (
                    [0, 1, 2].map(|i| lo[i].min(p[i])),
                    [0, 1, 2].map(|i| hi[i].max(p[i])),
                )
            })
    }

    #[test]
    fn a_rectangular_column_is_a_closed_box_facing_out() {
        let rect = polyline(&[
            [-0.2, -0.3],
            [0.2, -0.3],
            [0.2, 0.3],
            [-0.2, 0.3],
            [-0.2, -0.3],
        ]);
        let m = shape(rect, 1).mesh().unwrap();
        assert_eq!(m.triangles.len(), 4 * 2 + 2 * 2);
        assert!(
            (volume(&m) - 0.4 * 0.6 * 3.0).abs() < 1e-9,
            "{}",
            volume(&m)
        );
        // Y is up, Z the path: the profile's x runs along X = Y × Z = +x.
        let (lo, hi) = bounds(&m);
        assert!(
            (lo[0] + 0.2).abs() < 1e-12
                && (hi[1] - 0.3).abs() < 1e-12
                && (hi[2] - 3.0).abs() < 1e-12
        );
    }

    #[test]
    fn a_clockwise_profile_still_faces_out() {
        let rect = polyline(&[
            [-0.2, -0.3],
            [-0.2, 0.3],
            [0.2, 0.3],
            [0.2, -0.3],
            [-0.2, -0.3],
        ]);
        let m = shape(rect, 1).mesh().unwrap();
        assert!((volume(&m) - 0.72).abs() < 1e-9);
    }

    #[test]
    fn a_tube_with_a_hole_keeps_the_hole() {
        let profile = Curve::Poly {
            segments: vec![circle(0.5), circle(0.3)],
            params: vec![0.0, 1.0, 2.0],
        };
        let m = shape(profile, 2).mesh().unwrap();
        let expected = std::f64::consts::PI * (0.25 - 0.09) * 3.0;
        assert!(
            (volume(&m) - expected).abs() / expected < 1e-3,
            "{}",
            volume(&m)
        );
    }

    #[test]
    fn a_mitered_end_is_tilted_and_keeps_the_volume_of_the_sheared_prism() {
        let rect = polyline(&[
            [-0.5, -0.5],
            [0.5, -0.5],
            [0.5, 0.5],
            [-0.5, 0.5],
            [-0.5, -0.5],
        ]);
        let mut s = shape(rect, 1);
        let n = unit([0.0, 1.0, 1.0]).unwrap();
        s.miters = [None, Some(n)];
        let m = s.mesh().unwrap();
        // The end plane tilts 45° about X: z = 3 − y at the end; the mean length is still 3.
        let (lo, hi) = bounds(&m);
        assert!((hi[2] - 3.5).abs() < 1e-9 && (lo[2] - 0.0).abs() < 1e-9);
        assert!((volume(&m) - 3.0).abs() < 1e-9, "{}", volume(&m));
    }

    #[test]
    fn an_open_profile_makes_a_wall_without_caps() {
        let line = polyline(&[[0.0, 0.0], [4.0, 0.0]]);
        let mut s = shape(line, 1);
        s.caps = None;
        assert!(!s.closed());
        let m = s.mesh().unwrap();
        assert_eq!(m.triangles.len(), 2);
    }

    #[test]
    fn a_degenerate_path_gives_no_mesh() {
        let rect = polyline(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]);
        let mut s = shape(rect, 1);
        s.to = s.from;
        assert!(s.mesh().is_none());
    }
}
