//! Curves embedded in Breps and surfaces: lines, polylines, arcs, NURBS curves and polycurves,
//! decoded and sampled into points.
//!
//! Layouts (all start with a version byte, major 1):
//! - line: from, to, domain, dimension;
//! - Rhino 2 arrow (an object, not embedded): tail, head; openNURBS reads it as the line from
//!   tail to head, with the domain 0 to its length;
//! - polyline: points, parameters, dimension;
//! - arc: plane, radius, three redundant points, angle interval, domain, dimension;
//! - NURBS: dimension, rational flag, order, control point count, two reserved ints, box,
//!   knots (order + count − 2), control points (homogeneous when rational: weight last);
//! - polycurve: segment count, two reserved ints, box, segment parameters, segments (each an
//!   embedded curve).

use super::classes::{self, ClassKind};
use super::cursor::Cursor;
use super::record::{Wrapper, embedded};
use super::{ReadError, Result};
use crate::stream::frame::Width;

/// Nested polycurves are followed at most this deep.
const MAX_DEPTH: usize = 8;
/// Sampled curves stay within this share of the curve's size of the true curve.
pub(crate) const CHORD_TOLERANCE: f64 = 1e-4;
/// Starting pieces per curved NURBS span, before refining.
const SPAN_START: usize = 4;
/// Bisections of one piece at most.
const MAX_BISECTIONS: u32 = 12;
/// Points produced for one curve at most.
const MAX_SAMPLES: usize = 8192;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Curve {
    Line {
        from: [f64; 3],
        to: [f64; 3],
        domain: [f64; 2],
    },
    Polyline {
        points: Vec<[f64; 3]>,
        params: Vec<f64>,
    },
    Arc {
        origin: [f64; 3],
        x: [f64; 3],
        y: [f64; 3],
        radius: f64,
        angles: [f64; 2],
        domain: [f64; 2],
    },
    Nurbs(Nurbs),
    Poly {
        segments: Vec<Curve>,
        params: Vec<f64>,
    },
}

/// A NURBS curve with homogeneous control points `[w·x, w·y, w·z, w]` and the openNURBS knot
/// vector (order + count − 2 knots).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Nurbs {
    pub order: usize,
    pub knots: Vec<f64>,
    pub cvs: Vec<[f64; 4]>,
}

pub(crate) fn read(w: &Wrapper, width: Width) -> Result<Curve> {
    read_depth(w, width, 0)
}

fn read_depth(w: &Wrapper, width: Width, depth: usize) -> Result<Curve> {
    if classes::kind(&w.class) != Some(ClassKind::Curve) {
        return Err(ReadError::Unsupported("curve class"));
    }
    let mut c = Cursor::new(w.data);
    let version = c.u8()?;
    // Distinguish the curve classes by their UUID's first bytes (see `classes`).
    match curve_type(&w.class) {
        CurveType::Line => {
            let from = c.point3()?;
            let to = c.point3()?;
            Ok(Curve::Line {
                from,
                to,
                domain: [c.f64()?, c.f64()?],
            })
        }
        CurveType::Arrow => {
            let from = c.point3()?;
            let to = c.point3()?;
            let length = (0..3)
                .map(|i| (to[i] - from[i]).powi(2))
                .sum::<f64>()
                .sqrt();
            Ok(Curve::Line {
                from,
                to,
                domain: [0.0, length],
            })
        }
        CurveType::Polyline => {
            let n = c.count(24)?;
            let points = (0..n).map(|_| c.point3()).collect::<Result<Vec<_>>>()?;
            let m = c.count(8)?;
            let params = (0..m).map(|_| c.f64()).collect::<Result<Vec<_>>>()?;
            if points.len() < 2 || params.len() != points.len() {
                return Err(ReadError::Invalid {
                    at: 0,
                    what: "polyline",
                });
            }
            Ok(Curve::Polyline { points, params })
        }
        CurveType::Arc => {
            let origin = c.point3()?;
            let x = c.point3()?;
            let y = c.point3()?;
            c.skip(24 + 32)?; // z axis, plane equation
            let radius = c.f64()?;
            c.skip(72)?; // three redundant points
            let angles = [c.f64()?, c.f64()?];
            let domain = [c.f64()?, c.f64()?];
            Ok(Curve::Arc {
                origin,
                x,
                y,
                radius,
                angles,
                domain,
            })
        }
        CurveType::Nurbs => {
            if version >> 4 != 1 {
                return Err(ReadError::Unsupported("NURBS curve version"));
            }
            read_nurbs(&mut c)
        }
        CurveType::Poly => {
            if depth >= MAX_DEPTH {
                return Err(ReadError::Unsupported("polycurve nesting"));
            }
            let n = c.count(1)?;
            c.skip(8 + 48)?; // reserved, box
            let m = c.count(8)?;
            let params = (0..m).map(|_| c.f64()).collect::<Result<Vec<_>>>()?;
            let mut segments = Vec::with_capacity(n);
            for _ in 0..n {
                let seg = embedded(&mut c, width)?.ok_or(ReadError::Invalid {
                    at: 0,
                    what: "null polycurve segment",
                })?;
                segments.push(read_depth(&seg, width, depth + 1)?);
            }
            if segments.is_empty() || params.len() != segments.len() + 1 {
                return Err(ReadError::Invalid {
                    at: 0,
                    what: "polycurve",
                });
            }
            Ok(Curve::Poly { segments, params })
        }
    }
}

fn read_nurbs(c: &mut Cursor) -> Result<Curve> {
    let at = c.position();
    let dim = c.i32()?;
    let rational = c.i32()? != 0;
    let order = c.i32()?;
    let count = c.i32()?;
    c.skip(8 + 48)?; // reserved, box
    let bad = |what| ReadError::Invalid { at, what };
    if !(1..=3).contains(&dim) || order < 2 || order as usize > MAX_ORDER || count < order {
        return Err(bad("NURBS curve header"));
    }
    let (dim, order, count) = (dim as usize, order as usize, count as usize);
    let knot_count = c.count(8)?;
    if knot_count != order + count - 2 {
        return Err(bad("NURBS knot count"));
    }
    let knots = (0..knot_count)
        .map(|_| c.f64())
        .collect::<Result<Vec<_>>>()?;
    let stride = dim + usize::from(rational);
    let stored = c.count(8 * stride)?;
    if stored != count {
        return Err(bad("NURBS control point count"));
    }
    let mut cvs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut cv = [0.0, 0.0, 0.0, 1.0];
        for v in cv.iter_mut().take(dim) {
            *v = c.f64()?;
        }
        if rational {
            cv[3] = c.f64()?;
        }
        cvs.push(cv);
    }
    if knots.windows(2).any(|k| k[1] < k[0]) || cvs.iter().any(|cv| cv[3] == 0.0) {
        return Err(bad("NURBS curve values"));
    }
    Ok(Curve::Nurbs(Nurbs { order, knots, cvs }))
}

#[derive(Clone, Copy)]
enum CurveType {
    Line,
    Arrow,
    Polyline,
    Arc,
    Nurbs,
    Poly,
}

fn curve_type(class: &[u8; 16]) -> CurveType {
    // First four stored bytes (Data1, little-endian) tell the curve classes apart.
    match u32::from_le_bytes([class[0], class[1], class[2], class[3]]) {
        0x4ed7_d4db => CurveType::Line,
        0x8bd9_4e1a => CurveType::Arrow,
        0x4ed7_d4e6 => CurveType::Polyline,
        0xcf33_be2a => CurveType::Arc,
        0x4ed7_d4e0 | 0xef63_8317 => CurveType::Poly,
        _ => CurveType::Nurbs, // 4ed7d4dd, TL 5eaf1119, old 76a709d5
    }
}

impl Nurbs {
    pub(crate) fn degree(&self) -> usize {
        self.order - 1
    }

    pub(crate) fn domain(&self) -> [f64; 2] {
        [self.knots[self.order - 2], self.knots[self.cvs.len() - 1]]
    }

    /// Homogeneous point by de Boor's algorithm on the openNURBS knot vector.
    pub(crate) fn homogeneous_at(&self, t: f64) -> [f64; 4] {
        de_boor(self.order, &self.knots, self.cvs.len(), t, |i| self.cvs[i])
    }

    pub(crate) fn point_at(&self, t: f64) -> [f64; 3] {
        let h = self.homogeneous_at(t);
        [h[0] / h[3], h[1] / h[3], h[2] / h[3]]
    }

    /// Distinct knots inside `[t0, t1]`, with both ends.
    fn breaks(&self, t0: f64, t1: f64) -> Vec<f64> {
        let mut out = vec![t0];
        for &k in &self.knots {
            if k > t0 && k < t1 && out.last().is_none_or(|&l| k > l) {
                out.push(k);
            }
        }
        out.push(t1);
        out
    }
}

/// de Boor's algorithm on an openNURBS knot vector (order + count − 2 knots, no end knots
/// repeated beyond the order), with control points supplied on demand; only the `order` points
/// of the span containing `t` are asked for.
pub(crate) fn de_boor(
    order: usize,
    knots: &[f64],
    count: usize,
    t: f64,
    cv: impl Fn(usize) -> [f64; 4],
) -> [f64; 4] {
    let p = order - 1;
    // Span s with knots[s] <= t < knots[s + 1], s in [p - 1, count - 2]: past the leading knots
    // at or below t (a binary search; knot vectors do not decrease).
    let s = p - 1 + knots[p..count - 1].partition_point(|&k| k <= t);
    let mut d = [[0.0; 4]; 16];
    for (j, slot) in d.iter_mut().enumerate().take(p + 1) {
        *slot = cv(s + 1 - p + j);
    }
    for r in 1..=p {
        for j in (r..=p).rev() {
            let i = s + 1 - p + j; // control point index
            let left = knots[i - 1];
            let right = knots[i + p - r];
            let a = if right > left {
                (t - left) / (right - left)
            } else {
                0.0
            };
            for k in 0..4 {
                d[j][k] = (1.0 - a) * d[j - 1][k] + a * d[j][k];
            }
        }
    }
    d[p]
}

/// Orders above this are not decoded (de Boor works in a fixed buffer).
pub(crate) const MAX_ORDER: usize = 16;

impl Curve {
    pub(crate) fn domain(&self) -> [f64; 2] {
        match self {
            Self::Line { domain, .. } | Self::Arc { domain, .. } => *domain,
            Self::Polyline { params, .. } | Self::Poly { params, .. } => {
                [params[0], *params.last().expect("checked non-empty")]
            }
            Self::Nurbs(n) => n.domain(),
        }
    }

    pub(crate) fn point_at(&self, t: f64) -> [f64; 3] {
        match self {
            Self::Line { from, to, domain } => {
                let s = normalized(t, *domain);
                [0, 1, 2].map(|i| from[i] + s * (to[i] - from[i]))
            }
            Self::Polyline { points, params } => {
                let k = segment(params, t);
                let s = normalized(t, [params[k], params[k + 1]]);
                [0, 1, 2].map(|i| points[k][i] + s * (points[k + 1][i] - points[k][i]))
            }
            Self::Arc {
                origin,
                x,
                y,
                radius,
                angles,
                domain,
            } => {
                let a = angles[0] + normalized(t, *domain) * (angles[1] - angles[0]);
                let (sin, cos) = a.sin_cos();
                [0, 1, 2].map(|i| origin[i] + radius * (cos * x[i] + sin * y[i]))
            }
            Self::Nurbs(n) => n.point_at(t),
            Self::Poly { segments, params } => {
                let k = segment(params, t);
                let seg = &segments[k];
                let d = seg.domain();
                let s = normalized(t, [params[k], params[k + 1]]);
                seg.point_at(d[0] + s * (d[1] - d[0]))
            }
        }
    }

    /// Points along `[t0, t1]` (increasing), both ends included: the corners of straight
    /// pieces, and on curved pieces enough points that every chord stays within
    /// [`CHORD_TOLERANCE`] of the sampled part's size from the curve. The result depends only
    /// on the curve and the interval, so two faces sharing an edge get the same points.
    pub(crate) fn sample(&self, t0: f64, t1: f64) -> Vec<[f64; 3]> {
        self.sample_parameters(t0, t1)
            .into_iter()
            .map(|t| self.point_at(t))
            .collect()
    }

    /// The parameters [`Self::sample`] evaluates, in increasing order.
    pub(crate) fn sample_parameters(&self, t0: f64, t1: f64) -> Vec<f64> {
        let tol = CHORD_TOLERANCE * self.part_size(t0, t1);
        let mut params = Vec::new();
        // Refinement stops well past the cap, so the points kept are those of an unbounded
        // refinement (a curve of many spans would otherwise build millions first).
        self.sample_params(t0, t1, tol, 2 * MAX_SAMPLES + 2, &mut params);
        params.dedup();
        if params.len() > MAX_SAMPLES {
            params.truncate(MAX_SAMPLES - 1);
            params.push(t1);
        }
        params
    }

    /// Diagonal of the box around points spread over `[t0, t1]` (at least the part's chord,
    /// and never more than the whole curve's control box).
    fn part_size(&self, t0: f64, t1: f64) -> f64 {
        const SPREAD: usize = 16;
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for i in 0..=SPREAD {
            let p = self.point_at(t0 + (t1 - t0) * i as f64 / SPREAD as f64);
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let d = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
        let part = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if part.is_finite() && part > 0.0 {
            part.min(self.size())
        } else {
            self.size()
        }
    }

    /// Diagonal of the box around the control points (or defining points).
    fn size(&self) -> f64 {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        let mut add = |p: [f64; 3]| {
            for i in 0..3 {
                lo[i] = lo[i].min(p[i]);
                hi[i] = hi[i].max(p[i]);
            }
        };
        self.for_each_point(&mut add);
        let d = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
        let size = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if size.is_finite() { size } else { 0.0 }
    }

    fn for_each_point(&self, add: &mut impl FnMut([f64; 3])) {
        match self {
            Self::Line { from, to, .. } => {
                add(*from);
                add(*to);
            }
            Self::Polyline { points, .. } => points.iter().for_each(|p| add(*p)),
            Self::Arc { origin, radius, .. } => {
                add(origin.map(|v| v - radius));
                add(origin.map(|v| v + radius));
            }
            Self::Nurbs(n) => n
                .cvs
                .iter()
                .for_each(|cv| add([cv[0] / cv[3], cv[1] / cv[3], cv[2] / cv[3]])),
            Self::Poly { segments, .. } => segments.iter().for_each(|s| s.for_each_point(add)),
        }
    }

    /// Pushes the parameters of `[t0, t1]` onto `out`, refining no further once `out` holds
    /// `limit`.
    fn sample_params(&self, t0: f64, t1: f64, tol: f64, limit: usize, out: &mut Vec<f64>) {
        match self {
            Self::Line { .. } => out.extend([t0, t1]),
            Self::Polyline { params, .. } => {
                out.push(t0);
                out.extend(params.iter().copied().filter(|&p| p > t0 && p < t1));
                out.push(t1);
            }
            Self::Arc {
                radius,
                angles,
                domain,
                ..
            } => {
                // Sagitta r (1 − cos(θ / 2)) ≤ tol gives the largest angle per piece.
                let span = (angles[1] - angles[0]).abs() * (t1 - t0).abs()
                    / (domain[1] - domain[0]).abs().max(f64::MIN_POSITIVE);
                let step = if *radius > tol {
                    2.0 * (1.0 - tol / radius).acos()
                } else {
                    span
                };
                let n = (span / step.max(1e-6)).ceil().clamp(1.0, 4096.0) as usize;
                out.extend((0..=n).map(|i| t0 + (t1 - t0) * i as f64 / n as f64));
            }
            Self::Nurbs(n) => {
                let breaks = n.breaks(t0, t1);
                out.push(t0);
                for w in breaks.windows(2) {
                    if out.len() >= limit {
                        break;
                    }
                    if n.degree() == 1 {
                        out.push(w[1]);
                        continue;
                    }
                    for i in 0..SPAN_START {
                        let a = w[0] + (w[1] - w[0]) * i as f64 / SPAN_START as f64;
                        let b = w[0] + (w[1] - w[0]) * (i + 1) as f64 / SPAN_START as f64;
                        self.refine(a, b, tol, 0, limit, out);
                    }
                }
            }
            Self::Poly { segments, params } => {
                out.push(t0);
                for (k, seg) in segments.iter().enumerate() {
                    let (a, b) = (params[k].max(t0), params[k + 1].min(t1));
                    if a >= b {
                        continue;
                    }
                    let d = seg.domain();
                    let to_seg =
                        |t: f64| d[0] + normalized(t, [params[k], params[k + 1]]) * (d[1] - d[0]);
                    let mut inner = Vec::new();
                    seg.sample_params(
                        to_seg(a),
                        to_seg(b),
                        tol,
                        limit.saturating_sub(out.len()),
                        &mut inner,
                    );
                    // Back to polycurve parameters.
                    out.extend(
                        inner
                            .into_iter()
                            .map(|s| params[k] + normalized(s, d) * (params[k + 1] - params[k])),
                    );
                }
                out.push(t1);
            }
        }
    }

    /// Pushes the end of `[a, b]` after enough interior points that each chord stays within
    /// `tol` of the curve (checked at the quarter points, so S-bends are not missed).
    fn refine(&self, a: f64, b: f64, tol: f64, depth: u32, limit: usize, out: &mut Vec<f64>) {
        if out.len() >= limit {
            return;
        }
        let (pa, pb) = (self.point_at(a), self.point_at(b));
        let deviates = depth < MAX_BISECTIONS
            && [0.25, 0.5, 0.75]
                .iter()
                .any(|&f| chord_distance(self.point_at(a + (b - a) * f), pa, pb) > tol);
        if deviates {
            let m = (a + b) / 2.0;
            self.refine(a, m, tol, depth + 1, limit, out);
            self.refine(m, b, tol, depth + 1, limit, out);
        } else {
            out.push(b);
        }
    }
}

/// Distance from `p` to the segment `a`–`b`.
fn chord_distance(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let e = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let w = [p[0] - a[0], p[1] - a[1], p[2] - a[2]];
    let ee = e[0] * e[0] + e[1] * e[1] + e[2] * e[2];
    let s = if ee > 0.0 {
        ((w[0] * e[0] + w[1] * e[1] + w[2] * e[2]) / ee).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d = [w[0] - s * e[0], w[1] - s * e[1], w[2] - s * e[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn normalized(t: f64, [a, b]: [f64; 2]) -> f64 {
    if b != a { (t - a) / (b - a) } else { 0.0 }
}

/// Index k of the piece `[params[k], params[k + 1]]` containing `t` (clamped to the ends).
fn segment(params: &[f64], t: f64) -> usize {
    let last = params.len() - 2;
    // The first piece ending past t (a binary search; the parameters do not decrease).
    params[1..=last].partition_point(|&p| p.partial_cmp(&t) != Some(std::cmp::Ordering::Greater))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nurbs_quarter_circle_is_exact() {
        // Rational quadratic quarter circle, radius 2, as openNURBS stores it.
        let w = std::f64::consts::FRAC_1_SQRT_2;
        let n = Nurbs {
            order: 3,
            knots: vec![0.0, 0.0, 1.0, 1.0],
            cvs: vec![
                [2.0, 0.0, 0.0, 1.0],
                [2.0 * w, 2.0 * w, 0.0, w],
                [0.0, 2.0, 0.0, 1.0],
            ],
        };
        for i in 0..=10 {
            let p = n.point_at(i as f64 / 10.0);
            assert!(
                ((p[0] * p[0] + p[1] * p[1]).sqrt() - 2.0).abs() < 1e-12,
                "{p:?}"
            );
        }
        assert_eq!(n.point_at(0.0), [2.0, 0.0, 0.0]);
        assert_eq!(n.point_at(1.0), [0.0, 2.0, 0.0]);
    }

    #[test]
    fn degree_one_nurbs_samples_only_corners() {
        let n = Curve::Nurbs(Nurbs {
            order: 2,
            knots: vec![0.0, 1.0, 2.0],
            cvs: vec![
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 0.0, 1.0],
            ],
        });
        assert_eq!(
            n.sample(0.0, 2.0),
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]]
        );
        assert_eq!(
            n.sample(0.5, 1.5),
            vec![[0.5, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.5, 0.0]]
        );
    }

    #[test]
    fn curved_samples_stay_within_the_chord_tolerance() {
        let w = std::f64::consts::FRAC_1_SQRT_2;
        let quarter = Curve::Nurbs(Nurbs {
            order: 3,
            knots: vec![0.0, 0.0, 1.0, 1.0],
            cvs: vec![
                [2.0, 0.0, 0.0, 1.0],
                [2.0 * w, 2.0 * w, 0.0, w],
                [0.0, 2.0, 0.0, 1.0],
            ],
        });
        let points = quarter.sample(0.0, 1.0);
        let tol = CHORD_TOLERANCE * quarter.part_size(0.0, 1.0);
        for pair in points.windows(2) {
            let m = [0, 1, 2].map(|i| (pair[0][i] + pair[1][i]) / 2.0);
            let sagitta = 2.0 - (m[0] * m[0] + m[1] * m[1]).sqrt();
            assert!(sagitta <= tol * 1.01, "sagitta {sagitta} over {tol}");
        }
        assert!(points.len() > 8);
    }
}
