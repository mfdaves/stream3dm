//! Brep surfaces: plane, NURBS, sum (curve + curve) and revolution surfaces, decoded, with a
//! flatness test that gives the plane a planar face lies in.
//!
//! Layouts (version byte first):
//! - plane: plane (origin, x, y, z, equation), u and v domains, extents (minor 1+);
//! - NURBS: dimension, rational flag, orders, control point counts, two reserved ints, box,
//!   knots in u and v, control points u-major (homogeneous when rational);
//! - sum: base point, box, two embedded curves (u, v);
//! - revolution: axis, angle interval, parameter interval (major 2), box, transposed flag,
//!   then the profile curve when present.

use super::classes::{self, ClassKind};
use super::cursor::Cursor;
use super::curve::{self, Curve, MAX_ORDER};
use super::record::{Wrapper, embedded};
use super::{ReadError, Result};
use crate::stream::frame::Width;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Surface {
    Plane {
        origin: [f64; 3],
        x: [f64; 3],
        y: [f64; 3],
        z: [f64; 3],
        domain: [[f64; 2]; 2],
        extents: [[f64; 2]; 2],
    },
    Nurbs {
        order: [usize; 2],
        counts: [usize; 2],
        /// openNURBS knot vectors in u and v (order + count − 2 knots each).
        knots: [Vec<f64>; 2],
        /// Homogeneous control points `[w·x, w·y, w·z, w]`, u-major.
        cvs: Vec<[f64; 4]>,
    },
    Sum {
        base: [f64; 3],
        curves: Box<[Curve; 2]>,
    },
    /// A profile curve turned about an axis; u is the angle and v the profile parameter,
    /// swapped when transposed.
    Revolution {
        axis: [[f64; 3]; 2],
        angles: [f64; 2],
        /// The parameter interval mapped onto `angles`.
        domain: [f64; 2],
        transposed: bool,
        profile: Box<Curve>,
    },
}

/// A plane with orthonormal axes; `z` is the surface's natural normal (∂S/∂u × ∂S/∂v).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Frame {
    pub origin: [f64; 3],
    pub x: [f64; 3],
    pub y: [f64; 3],
    pub z: [f64; 3],
}

impl Frame {
    pub(crate) fn project(&self, p: [f64; 3]) -> [f64; 2] {
        let d = sub(p, self.origin);
        [dot(d, self.x), dot(d, self.y)]
    }
}

pub(crate) fn read(w: &Wrapper, width: Width) -> Result<Surface> {
    let mut c = Cursor::new(w.data);
    match classes::kind(&w.class) {
        Some(ClassKind::PlaneSurface) => {
            let version = c.u8()?;
            let origin = c.point3()?;
            let x = c.point3()?;
            let y = c.point3()?;
            let z = c.point3()?;
            c.skip(32)?; // plane equation
            let domain = [[c.f64()?, c.f64()?], [c.f64()?, c.f64()?]];
            let extents = if version & 0x0f >= 1 {
                [[c.f64()?, c.f64()?], [c.f64()?, c.f64()?]]
            } else {
                domain
            };
            Ok(Surface::Plane {
                origin,
                x,
                y,
                z,
                domain,
                extents,
            })
        }
        Some(ClassKind::Surface) => {
            match u32::from_le_bytes([w.class[0], w.class[1], w.class[2], w.class[3]]) {
                0xc4cd_5359 | 0x665f_6331 => {
                    c.u8()?;
                    let base = c.point3()?;
                    c.skip(48)?; // box
                    let mut curve = || -> Result<Curve> {
                        let w = embedded(&mut c, width)?.ok_or(ReadError::Invalid {
                            at: 0,
                            what: "sum surface without curve",
                        })?;
                        curve::read(&w, width)
                    };
                    let first = curve()?;
                    let second = curve()?;
                    Ok(Surface::Sum {
                        base,
                        curves: Box::new([first, second]),
                    })
                }
                0xa162_20d3 | 0x0a84_01b6 => {
                    let major = c.u8()? >> 4;
                    let axis = [c.point3()?, c.point3()?];
                    let angles = [c.f64()?, c.f64()?];
                    let domain = if major >= 2 {
                        [c.f64()?, c.f64()?]
                    } else {
                        angles
                    };
                    c.skip(48)?; // box
                    let transposed = c.i32()? != 0;
                    let profile = if c.u8()? != 0 {
                        embedded(&mut c, width)?
                    } else {
                        None
                    }
                    .ok_or(ReadError::Unsupported("revolution without profile"))?;
                    Ok(Surface::Revolution {
                        axis,
                        angles,
                        domain,
                        transposed,
                        profile: Box::new(curve::read(&profile, width)?),
                    })
                }
                _ => read_nurbs(&mut c),
            }
        }
        _ => Err(ReadError::Unsupported("surface class")),
    }
}

/// `ON_NurbsSurface::Read` from its version byte on (also the raw surfaces of Rhino 2 Breps).
pub(super) fn read_nurbs(c: &mut Cursor) -> Result<Surface> {
    let at = c.position();
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported("NURBS surface version"));
    }
    let dim = c.i32()?;
    let rational = c.i32()? != 0;
    let order = [c.i32()?, c.i32()?];
    let counts = [c.i32()?, c.i32()?];
    c.skip(8 + 48)?; // reserved, box
    let bad = ReadError::Invalid {
        at,
        what: "NURBS surface header",
    };
    if !(1..=3).contains(&dim)
        || (0..2).any(|i| order[i] < 2 || order[i] as usize > MAX_ORDER || counts[i] < order[i])
    {
        return Err(bad);
    }
    let order = [order[0] as usize, order[1] as usize];
    let counts = [counts[0] as usize, counts[1] as usize];
    let mut knots: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    for (i, k) in knots.iter_mut().enumerate() {
        let n = c.count(8)?;
        if n != order[i] + counts[i] - 2 {
            return Err(bad);
        }
        *k = (0..n).map(|_| c.f64()).collect::<Result<_>>()?;
        if k.windows(2).any(|w| w[1] < w[0]) {
            return Err(bad);
        }
    }
    let dim = dim as usize;
    let stride = dim + usize::from(rational);
    let total = c.count(8 * stride)?;
    if Some(total) != counts[0].checked_mul(counts[1]) {
        return Err(bad);
    }
    let mut cvs = Vec::with_capacity(total);
    for _ in 0..total {
        let mut cv = [0.0, 0.0, 0.0, 1.0];
        for v in cv.iter_mut().take(dim) {
            *v = c.f64()?;
        }
        if rational {
            cv[3] = c.f64()?;
            if cv[3] == 0.0 {
                return Err(bad);
            }
        }
        cvs.push(cv);
    }
    Ok(Surface::Nurbs {
        order,
        counts,
        knots,
        cvs,
    })
}

impl Surface {
    /// The plane of a flat surface, within `relative` of its size; `None` when curved.
    pub(crate) fn frame(&self, relative: f64) -> Option<Frame> {
        match self {
            Self::Plane {
                origin,
                x,
                y,
                z,
                domain,
                extents,
            } => {
                // Extents running against the domain flip the natural normal.
                let sign = |i: usize| {
                    ((extents[i][1] - extents[i][0]) * (domain[i][1] - domain[i][0])).signum()
                };
                let z = scale(normalize(*z)?, sign(0) * sign(1));
                let x = normalize(*x)?;
                Some(Frame {
                    origin: *origin,
                    x,
                    y: cross(z, x),
                    z,
                })
                .filter(|_| dot(normalize(*y).unwrap_or(z), z).abs() < 1e-6)
            }
            Self::Nurbs { counts, cvs, .. } => {
                let [nu, nv] = *counts;
                let points: Vec<[f64; 3]> = cvs
                    .iter()
                    .map(|c| [c[0] / c[3], c[1] / c[3], c[2] / c[3]])
                    .collect();
                let points = &points;
                let at = |i: usize, j: usize| points[i * nv + j];
                // Boundary ring of the control net in parameter order: its normal points
                // along ∂S/∂u × ∂S/∂v.
                let mut ring = Vec::new();
                ring.extend((0..nu).map(|i| at(i, 0)));
                ring.extend((1..nv).map(|j| at(nu - 1, j)));
                ring.extend((0..nu - 1).rev().map(|i| at(i, nv - 1)));
                ring.extend((1..nv - 1).rev().map(|j| at(0, j)));
                planar_frame(&ring, points, relative)
            }
            Self::Sum { base, curves } => {
                let [c0, c1] = &**curves;
                let (d0, d1) = (c0.domain(), c1.domain());
                let s0 = c0.sample(d0[0], d0[1]);
                let s1 = c1.sample(d1[0], d1[1]);
                let corner = add(add(s0[0], s1[0]), *base);
                // All points c0(u) + c1(v) + base lie in one plane iff both curves' points
                // lie in planes with the same normal.
                let mut points: Vec<[f64; 3]> =
                    s0.iter().map(|p| add(add(*p, s1[0]), *base)).collect();
                points.extend(s1.iter().map(|p| add(add(s0[0], *p), *base)));
                let far0 = farthest(&s0, s0[0])?;
                let far1 = farthest(&s1, s1[0])?;
                let n = normalize(cross(sub(far0, s0[0]), sub(far1, s1[0])))?;
                let size = diagonal(&points);
                if points
                    .iter()
                    .any(|p| dot(sub(*p, corner), n).abs() > relative * size)
                {
                    return None;
                }
                // Orient along ∂S/∂u × ∂S/∂v at the middle of the domain.
                let tangent = |c: &Curve, d: [f64; 2]| {
                    let (m, h) = ((d[0] + d[1]) / 2.0, (d[1] - d[0]) * 1e-3);
                    sub(c.point_at(m + h), c.point_at(m - h))
                };
                let natural = cross(tangent(c0, d0), tangent(c1, d1));
                let z = if dot(natural, n) < 0.0 {
                    scale(n, -1.0)
                } else {
                    n
                };
                Some(frame_from_normal(corner, z))
            }
            Self::Revolution { .. } => None,
        }
    }

    /// Parameter domains in u and v.
    pub(crate) fn domain(&self) -> [[f64; 2]; 2] {
        match self {
            Self::Plane { domain, .. } => *domain,
            Self::Nurbs {
                order,
                counts,
                knots,
                ..
            } => [0, 1].map(|i| [knots[i][order[i] - 2], knots[i][counts[i] - 1]]),
            Self::Sum { curves, .. } => [curves[0].domain(), curves[1].domain()],
            Self::Revolution {
                domain,
                transposed,
                profile,
                ..
            } => {
                if *transposed {
                    [profile.domain(), *domain]
                } else {
                    [*domain, profile.domain()]
                }
            }
        }
    }

    pub(crate) fn point_at(&self, u: f64, v: f64) -> [f64; 3] {
        match self {
            Self::Plane {
                origin,
                x,
                y,
                domain,
                extents,
                ..
            } => {
                let map = |t: f64, d: [f64; 2], e: [f64; 2]| {
                    let s = if d[1] != d[0] {
                        (t - d[0]) / (d[1] - d[0])
                    } else {
                        0.0
                    };
                    e[0] + s * (e[1] - e[0])
                };
                let (a, b) = (map(u, domain[0], extents[0]), map(v, domain[1], extents[1]));
                [0, 1, 2].map(|i| origin[i] + a * x[i] + b * y[i])
            }
            Self::Nurbs {
                order,
                counts,
                knots,
                cvs,
            } => {
                let [nu, nv] = *counts;
                let h = curve::de_boor(order[0], &knots[0], nu, u, |i| {
                    curve::de_boor(order[1], &knots[1], nv, v, |j| cvs[i * nv + j])
                });
                [h[0] / h[3], h[1] / h[3], h[2] / h[3]]
            }
            Self::Sum { base, curves } => {
                let (a, b) = (curves[0].point_at(u), curves[1].point_at(v));
                [0, 1, 2].map(|i| a[i] + b[i] + base[i])
            }
            Self::Revolution {
                axis,
                angles,
                domain,
                transposed,
                profile,
            } => {
                let (t, s) = if *transposed { (v, u) } else { (u, v) };
                let f = if domain[1] != domain[0] {
                    (t - domain[0]) / (domain[1] - domain[0])
                } else {
                    0.0
                };
                let angle = angles[0] + f * (angles[1] - angles[0]);
                rotate(profile.point_at(s), axis[0], sub(axis[1], axis[0]), angle)
            }
        }
    }

    /// Parameters in direction `dir` (0 = u, 1 = v) within `range` where the surface needs
    /// grid lines: knots, with curved spans subdivided; both ends included.
    pub(crate) fn grid_parameters(&self, dir: usize, range: [f64; 2]) -> Vec<f64> {
        let [a, b] = range;
        if b.partial_cmp(&a) != Some(std::cmp::Ordering::Greater) {
            return vec![a, b];
        }
        let mut out = match self {
            Self::Plane { .. } => vec![a, b],
            Self::Nurbs {
                order,
                counts,
                knots,
                ..
            } => {
                let mut breaks = vec![a];
                for &k in &knots[dir][order[dir] - 2..counts[dir]] {
                    if k > a && k < b && breaks.last().is_none_or(|&l| k > l) {
                        breaks.push(k);
                    }
                }
                breaks.push(b);
                let pieces = if order[dir] == 2 {
                    1
                } else {
                    SURFACE_SPAN_PIECES
                };
                let mut out = vec![a];
                for w in breaks.windows(2) {
                    out.extend(
                        (1..=pieces).map(|i| w[0] + (w[1] - w[0]) * i as f64 / pieces as f64),
                    );
                }
                out
            }
            Self::Sum { curves, .. } => curves[dir].sample_parameters(a, b),
            Self::Revolution {
                angles,
                domain,
                transposed,
                profile,
                ..
            } => {
                if (dir == 0) != *transposed {
                    let span = (angles[1] - angles[0]).abs() * (b - a)
                        / (domain[1] - domain[0]).abs().max(f64::MIN_POSITIVE);
                    let n = (span / REVOLUTION_STEP).ceil().clamp(1.0, 720.0) as usize;
                    (0..=n).map(|i| a + (b - a) * i as f64 / n as f64).collect()
                } else {
                    profile.sample_parameters(a, b)
                }
            }
        };
        out.dedup();
        if out.len() > MAX_GRID_LINES {
            // Keep an even spread, both ends included.
            let n = MAX_GRID_LINES - 1;
            out = (0..=n).map(|i| out[i * (out.len() - 1) / n]).collect();
        }
        out
    }
}

/// Pieces per curved NURBS span in the interior grid of a curved face.
const SURFACE_SPAN_PIECES: usize = 6;
/// Largest angle between grid lines of a surface of revolution (radians).
const REVOLUTION_STEP: f64 = 7.5 * std::f64::consts::PI / 180.0;
/// Grid lines per direction at most.
const MAX_GRID_LINES: usize = 256;

/// `p` turned by `angle` about the line through `origin` along `axis` (Rodrigues).
fn rotate(p: [f64; 3], origin: [f64; 3], axis: [f64; 3], angle: f64) -> [f64; 3] {
    let Some(k) = normalize(axis) else {
        return p;
    };
    let v = sub(p, origin);
    let (sin, cos) = angle.sin_cos();
    let kv = cross(k, v);
    let kd = dot(k, v) * (1.0 - cos);
    [0, 1, 2].map(|i| origin[i] + v[i] * cos + kv[i] * sin + k[i] * kd)
}

impl Surface {
    /// The surface kind and, for NURBS nets, the largest control point distance from the
    /// best plane relative to the net's size (for diagnosing flatness).
    #[cfg(all(test, feature = "cadmpeg"))]
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Plane { .. } => "plane".to_string(),
            Self::Nurbs { counts, .. } => {
                let mut deviation = f64::NAN;
                for relative in [1e-9, 1e-8, 1e-7, 1e-6, 1e-5, 1e-4, 1e-3, 1e-2, 1e-1] {
                    if self.frame(relative).is_some() {
                        deviation = relative;
                        break;
                    }
                }
                format!(
                    "NURBS {}x{} flat within {deviation:e}",
                    counts[0], counts[1]
                )
            }
            Self::Sum { curves, .. } => format!(
                "sum of {} and {}",
                curve_name(&curves[0]),
                curve_name(&curves[1])
            ),
            Self::Revolution { .. } => "revolution".to_string(),
        }
    }
}

#[cfg(all(test, feature = "cadmpeg"))]
fn curve_name(c: &Curve) -> &'static str {
    match c {
        Curve::Line { .. } => "line",
        Curve::Polyline { .. } => "polyline",
        Curve::Arc { .. } => "arc",
        Curve::Nurbs(n) if n.degree() == 1 => "degree-1 NURBS",
        Curve::Nurbs(_) => "NURBS",
        Curve::Poly { .. } => "polycurve",
    }
}

/// Plane through `points` with the orientation of `ring` (Newell's method), when every point
/// lies within `relative` × size of it.
fn planar_frame(ring: &[[f64; 3]], points: &[[f64; 3]], relative: f64) -> Option<Frame> {
    let mut n = [0.0; 3];
    for (k, p) in ring.iter().enumerate() {
        let q = ring[(k + 1) % ring.len()];
        n[0] += (p[1] - q[1]) * (p[2] + q[2]);
        n[1] += (p[2] - q[2]) * (p[0] + q[0]);
        n[2] += (p[0] - q[0]) * (p[1] + q[1]);
    }
    let n = normalize(n)?;
    let count = points.len() as f64;
    let centroid = points
        .iter()
        .fold([0.0; 3], |a, p| add(a, *p))
        .map(|v| v / count);
    let size = diagonal(points);
    if size == 0.0
        || points
            .iter()
            .any(|p| dot(sub(*p, centroid), n).abs() > relative * size)
    {
        return None;
    }
    Some(frame_from_normal(centroid, n))
}

fn frame_from_normal(origin: [f64; 3], z: [f64; 3]) -> Frame {
    let helper = if z[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let x = normalize(cross(helper, z)).expect("helper is not parallel to z");
    Frame {
        origin,
        x,
        y: cross(z, x),
        z,
    }
}

fn farthest(points: &[[f64; 3]], from: [f64; 3]) -> Option<[f64; 3]> {
    points
        .iter()
        .copied()
        .max_by(|a, b| length(sub(*a, from)).total_cmp(&length(sub(*b, from))))
        .filter(|p| length(sub(*p, from)) > 0.0)
}

fn diagonal(points: &[[f64; 3]]) -> f64 {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for p in points {
        for i in 0..3 {
            lo[i] = lo[i].min(p[i]);
            hi[i] = hi[i].max(p[i]);
        }
    }
    length(sub(hi, lo))
}

pub(crate) fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    a.map(|v| v * s)
}

pub(crate) fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn normalize(a: [f64; 3]) -> Option<[f64; 3]> {
    let l = length(a);
    (l > 0.0 && l.is_finite()).then(|| scale(a, 1.0 / l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(points: [[f64; 3]; 4]) -> Surface {
        Surface::Nurbs {
            order: [2, 2],
            counts: [2, 2],
            knots: [vec![0.0, 1.0], vec![0.0, 1.0]],
            cvs: points.iter().map(|p| [p[0], p[1], p[2], 1.0]).collect(),
        }
    }

    #[test]
    fn flat_nurbs_net_gives_its_plane_and_orientation() {
        // 2 × 2 net in the plane z = 5, u along x, v along y: natural normal +z.
        let s = net([
            [0.0, 0.0, 5.0],
            [0.0, 3.0, 5.0],
            [4.0, 0.0, 5.0],
            [4.0, 3.0, 5.0],
        ]);
        let f = s.frame(1e-9).expect("flat");
        assert!((f.z[2] - 1.0).abs() < 1e-12);
        assert!((f.origin[2] - 5.0).abs() < 1e-12);
        // Twisted net: not flat; its middle is the corners' average (bilinear).
        let twisted = net([
            [0.0, 0.0, 0.0],
            [0.0, 3.0, 0.0],
            [4.0, 0.0, 0.0],
            [4.0, 3.0, 1.0],
        ]);
        assert!(twisted.frame(1e-6).is_none());
        let m = twisted.point_at(0.5, 0.5);
        assert!((m[0] - 2.0).abs() < 1e-12 && (m[1] - 1.5).abs() < 1e-12);
        assert!((m[2] - 0.25).abs() < 1e-12);
    }

    #[test]
    fn revolution_turns_its_profile() {
        // A vertical line at x = 2 turned about the z axis: a cylinder of radius 2.
        let s = Surface::Revolution {
            axis: [[0.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            angles: [0.0, std::f64::consts::TAU],
            domain: [0.0, 1.0],
            transposed: false,
            profile: Box::new(Curve::Line {
                from: [2.0, 0.0, 0.0],
                to: [2.0, 0.0, 5.0],
                domain: [0.0, 1.0],
            }),
        };
        let p = s.point_at(0.25, 0.5);
        assert!(p[0].abs() < 1e-12 && (p[1] - 2.0).abs() < 1e-12 && (p[2] - 2.5).abs() < 1e-12);
    }
}
