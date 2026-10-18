//! Meshes for Brep faces that have no cached render mesh.
//!
//! A face's loops are built from its edges' 3D curves, so neighbouring faces sample the same
//! points along a shared edge and meet without cracks. Each loop is followed trim by trim:
//! the edge's curve is sampled over the part of its domain the edge uses, reversed when the
//! edge runs against its curve and again when the trim runs against its edge, and its ends
//! are snapped to the trim's vertices. The loops are projected onto the face's plane and
//! triangulated together (outer boundary and holes); triangles face along the surface's
//! natural normal, flipped when the face is reversed.
//!
//! Curved faces are triangulated in the surface's parameter space instead: the loops come
//! from the trims' 2D curves (each boundary point keeps the 3D point of the shared edge), a
//! grid of interior points follows the surface's spans, and a constrained Delaunay
//! triangulation of both is kept where it lies inside the loops, then mapped onto the surface.

use super::mesh::Mesh;
use super::surface::{Frame, Surface, cross, dot, sub};
use super::topology::{Face, LoopKind, Topology, Trim};
use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};

/// A face is flat when every control point lies within this share of its size of one plane.
pub(crate) const FLAT_TOLERANCE: f64 = 1e-6;
/// Points in one face's loops at most.
const MAX_FACE_POINTS: usize = 1 << 20;
/// A curved face's boundary point keeps its edge's 3D point when within this share of the
/// face's size of the surface point at its parameters.
const EDGE_AGREEMENT: f64 = 1e-3;
/// A curved face's mesh is rejected when more than this share of its area faces against the
/// surface normal. (The two faces of v2_HumanHead2 that fold by 1.02 % fold by as much on a grid
/// 16 times finer: a feature of the surface, drawn rather than dropped.)
const MAX_FOLDED_SHARE: f64 = 0.02;
/// A curved face folding by more than this share is not retried on a finer grid.
const MAX_REFINABLE_SHARE: f64 = 0.1;
/// Grid subdivision of the second attempt at a curved face that folded.
const REFINE: usize = 4;

/// Why a face was not tessellated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// The face's surface is missing or of a class not decoded.
    NoSurface,
    /// A curved face whose loops cannot be built in its parameter space.
    Curved,
    /// A loop refers to a missing trim, edge, curve or vertex, or has fewer than three points.
    BadLoop,
    /// The triangulation produced nothing (degenerate or self-intersecting loops).
    Empty,
    /// Triangles of a curved face folded against the surface's normal: rejected rather than
    /// drawn wrong.
    Folded,
    /// A curved face whose loops would take too long to triangulate (hundreds of thousands of
    /// points crossing the face, as only damaged files have).
    TooComplex,
}

#[derive(Debug, Default)]
pub(crate) struct Tessellation {
    /// One mesh per tessellated face, in face order.
    pub meshes: Vec<Mesh>,
    /// Faces not tessellated, and why.
    pub skipped: Vec<Skip>,
}

pub(crate) fn tessellate(t: &Topology, mm_per_unit: f64) -> Tessellation {
    let mut out = Tessellation::default();
    let mut earcut = earcut::Earcut::new();
    for face in &t.faces {
        match face_mesh(t, face, &mut earcut) {
            Ok(mut mesh) => {
                for p in &mut mesh.positions_mm {
                    *p = p.map(|v| v * mm_per_unit);
                }
                out.meshes.push(mesh);
            }
            Err(skip) => out.skipped.push(skip),
        }
    }
    out
}

/// The mesh of one flat face, in model units.
pub(crate) fn face_mesh(
    t: &Topology,
    face: &Face,
    earcut: &mut earcut::Earcut<f64>,
) -> Result<Mesh, Skip> {
    let surface = usize::try_from(face.surface)
        .ok()
        .and_then(|i| t.surfaces.get(i))
        .and_then(Option::as_ref)
        .ok_or(Skip::NoSurface)?;
    let Some(frame) = surface.frame(FLAT_TOLERANCE) else {
        let (mesh, folded) = curved_mesh(t, face, surface, 1)?;
        if folded <= MAX_FOLDED_SHARE {
            return Ok(mesh);
        }
        // A fold the grid's triangles cause shrinks on a finer grid; a large one comes from
        // loops read wrong, and stays rejected.
        if folded <= MAX_REFINABLE_SHARE {
            let (mesh, refined) = curved_mesh(t, face, surface, REFINE)?;
            if refined <= MAX_FOLDED_SHARE && refined <= folded / 4.0 {
                return Ok(mesh);
            }
        }
        return Err(Skip::Folded);
    };
    let mut rings: Vec<Vec<[f64; 3]>> = Vec::new();
    let mut outer = None;
    for &li in &face.loops {
        let lp = usize::try_from(li)
            .ok()
            .and_then(|i| t.loops.get(i))
            .ok_or(Skip::BadLoop)?;
        if lp.kind == LoopKind::Other {
            continue;
        }
        let ring = loop_points(t, &lp.trims).ok_or(Skip::BadLoop)?;
        if ring.len() < 3 {
            if lp.kind == LoopKind::Outer {
                return Err(Skip::BadLoop);
            }
            continue;
        }
        if lp.kind == LoopKind::Outer && outer.is_none() {
            outer = Some(rings.len());
        }
        rings.push(ring);
    }
    if rings.is_empty() {
        return Err(Skip::BadLoop);
    }
    // Trust the geometry over the stored loop types: the outer boundary encloses the most area.
    let flat: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .map(|r| r.iter().map(|p| frame.project(*p)).collect())
        .collect();
    let largest = (0..flat.len())
        .max_by(|&a, &b| area(&flat[a]).abs().total_cmp(&area(&flat[b]).abs()))
        .expect("non-empty");
    let outer = outer
        .filter(|&o| area(&flat[o]).abs() >= area(&flat[largest]).abs() * 0.999)
        .unwrap_or(largest);
    let mut order = vec![outer];
    order.extend((0..rings.len()).filter(|&i| i != outer));

    let mut positions = Vec::new();
    let mut points2 = Vec::new();
    let mut holes: Vec<u32> = Vec::new();
    for (k, &i) in order.iter().enumerate() {
        if k > 0 {
            holes.push(points2.len() as u32);
        }
        positions.extend_from_slice(&rings[i]);
        points2.extend_from_slice(&flat[i]);
    }
    if points2.len() > MAX_FACE_POINTS {
        return Err(Skip::BadLoop);
    }
    let mut indices: Vec<u32> = Vec::new();
    earcut.earcut(points2.iter().copied(), &holes, &mut indices);
    let mut triangles = Vec::with_capacity(indices.len() / 3);
    for t in indices.chunks_exact(3) {
        let [a, b, c] = [t[0], t[1], t[2]];
        // Counter-clockwise in the plane means facing the natural normal.
        let ccw = signed_area2(
            points2[a as usize],
            points2[b as usize],
            points2[c as usize],
        ) > 0.0;
        triangles.push(if ccw != face.reversed {
            [a, b, c]
        } else {
            [a, c, b]
        });
    }
    if triangles.is_empty() {
        return Err(Skip::Empty);
    }
    debug_assert!(normal_agrees(&positions, &triangles, frame, face.reversed));
    Ok(Mesh {
        positions_mm: positions,
        triangles,
        dropped_faces: 0,
    })
}

/// What a trim contributes to a loop in 3D.
enum TrimPoints {
    /// Points along the trim's edge in the trim's direction, each with its position along the
    /// trim (0 at the start, 1 at the end), the ends snapped to the trim's vertices.
    Edge(Vec<(f64, [f64; 3])>),
    /// A singular trim: no edge, a single point in 3D (such as a sphere's pole).
    Singular,
}

/// The 3D points of a trim; `None` when it refers to data that is missing.
fn trim_points(t: &Topology, trim: &Trim) -> Option<TrimPoints> {
    let vertex = |i: i32| {
        usize::try_from(i)
            .ok()
            .and_then(|i| t.vertices.get(i))
            .copied()
    };
    let Ok(ei) = usize::try_from(trim.edge) else {
        return Some(TrimPoints::Singular);
    };
    let edge = t.edges.get(ei)?;
    let curve = t.curves.get(usize::try_from(edge.curve).ok()?)?.as_ref()?;
    let [p0, p1] = edge.proxy;
    let mut points: Vec<(f64, [f64; 3])> = curve
        .sample_parameters(p0, p1)
        .into_iter()
        .map(|p| {
            let s = if p1 != p0 { (p - p0) / (p1 - p0) } else { 0.0 };
            (s, curve.point_at(p))
        })
        .collect();
    for flip in [edge.reversed, trim.reversed] {
        if flip {
            points.reverse();
            for p in &mut points {
                p.0 = 1.0 - p.0;
            }
        }
    }
    if let (Some(first), Some(start)) = (points.first_mut(), vertex(trim.vertices[0])) {
        first.1 = start;
    }
    if let (Some(last), Some(end)) = (points.last_mut(), vertex(trim.vertices[1])) {
        last.1 = end;
    }
    Some(TrimPoints::Edge(points))
}

/// The points of one loop in order, without repeating the start.
fn loop_points(t: &Topology, trims: &[i32]) -> Option<Vec<[f64; 3]>> {
    let mut ring: Vec<[f64; 3]> = Vec::new();
    for &ti in trims {
        let trim = t.trims.get(usize::try_from(ti).ok()?)?;
        let TrimPoints::Edge(mut points) = trim_points(t, trim)? else {
            continue; // singular trim: a point in 3D
        };
        points.pop(); // the next trim starts here
        for (_, p) in points {
            if ring.last() != Some(&p) {
                ring.push(p);
            }
        }
    }
    while ring.len() > 1 && ring.first() == ring.last() {
        ring.pop();
    }
    Some(ring)
}

fn area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (p, q) = (ring[i], ring[(i + 1) % n]);
            p[0] * q[1] - q[0] * p[1]
        })
        .sum::<f64>()
        / 2.0
}

fn signed_area2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn normal_agrees(
    positions: &[[f64; 3]],
    triangles: &[[u32; 3]],
    frame: Frame,
    reversed: bool,
) -> bool {
    let n = triangles.iter().fold([0.0; 3], |acc, t| {
        let [a, b, c] = t.map(|i| positions[i as usize]);
        let m = cross(sub(b, a), sub(c, a));
        [acc[0] + m[0], acc[1] + m[1], acc[2] + m[2]]
    });
    let d = dot(n, frame.z);
    if reversed { d <= 0.0 } else { d >= 0.0 }
}

/// A loop point: surface parameters and 3D position.
type LoopPoint = ([f64; 2], [f64; 3]);

/// One loop in the surface's parameter space. Points on edges keep the edge's 3D point; their
/// parameters come from the trim's 2D curve at the same share of the trim. Singular trims
/// contribute their 2D points, placed on the surface.
fn loop_uv(t: &Topology, surface: &Surface, trims: &[i32]) -> Option<Vec<LoopPoint>> {
    let mut ring: Vec<LoopPoint> = Vec::new();
    for &ti in trims {
        let trim = t.trims.get(usize::try_from(ti).ok()?)?;
        let c2 = t
            .curves_2d
            .get(usize::try_from(trim.curve_2d).ok()?)?
            .as_ref()?;
        let [q0, q1] = trim.proxy;
        let uv = |share: f64| {
            let share = if trim.proxy_reversed {
                1.0 - share
            } else {
                share
            };
            let p = c2.point_at(q0 + share * (q1 - q0));
            [p[0], p[1]]
        };
        let mut points: Vec<LoopPoint> = match trim_points(t, trim)? {
            TrimPoints::Edge(points) => points.into_iter().map(|(s, p)| (uv(s), p)).collect(),
            TrimPoints::Singular => c2
                .sample_parameters(q0, q1)
                .into_iter()
                .map(|q| {
                    let s = if q1 != q0 { (q - q0) / (q1 - q0) } else { 0.0 };
                    let at = uv(if trim.proxy_reversed { 1.0 - s } else { s });
                    (at, surface.point_at(at[0], at[1]))
                })
                .collect(),
        };
        points.pop(); // the next trim starts here
        for p in points {
            if ring.last().is_none_or(|l| l.0 != p.0) {
                ring.push(p);
            }
        }
    }
    while ring.len() > 1 && ring.first().map(|f| f.0) == ring.last().map(|l| l.0) {
        ring.pop();
    }
    Some(ring)
}

/// Whether `p` lies inside the region bounded by `rings` (even–odd rule).
#[cfg(test)]
fn inside(p: [f64; 2], rings: &[Vec<[f64; 2]>]) -> bool {
    let mut odd = false;
    for ring in rings {
        let n = ring.len();
        for i in 0..n {
            odd ^= crosses_right(p, ring[i], ring[(i + 1) % n]);
        }
    }
    odd
}

/// Distance from `p` to the nearest ring segment.
#[cfg(test)]
fn boundary_distance(p: [f64; 2], rings: &[Vec<[f64; 2]>]) -> f64 {
    let mut best = f64::INFINITY;
    for ring in rings {
        let n = ring.len();
        for i in 0..n {
            best = best.min(segment_distance(p, ring[i], ring[(i + 1) % n]));
        }
    }
    best
}

/// Whether segment `a`–`b` crosses the ray from `p` towards +x (the even–odd rule's step).
fn crosses_right(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> bool {
    (a[1] > p[1]) != (b[1] > p[1]) && a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]) > p[0]
}

fn segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let e = [b[0] - a[0], b[1] - a[1]];
    let ee = e[0] * e[0] + e[1] * e[1];
    let s = if ee > 0.0 {
        (((p[0] - a[0]) * e[0] + (p[1] - a[1]) * e[1]) / ee).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d = [p[0] - a[0] - s * e[0], p[1] - a[1] - s * e[1]];
    (d[0] * d[0] + d[1] * d[1]).sqrt()
}

/// The largest coordinate magnitude of `rings` (the scale of their rounding errors).
fn size_2d(rings: &[Vec<[f64; 2]>]) -> f64 {
    rings
        .iter()
        .flatten()
        .fold(0.0, |m: f64, p| m.max(p[0].abs()).max(p[1].abs()))
}

/// Bands at most per face index.
const MAX_BANDS: usize = 4096;
/// Segment tests a face may cost (grid points and triangles against their bands); a face
/// beyond it (loops of hundreds of thousands of points crossing the whole face) is skipped.
const MAX_FACE_WORK: usize = 1 << 28;

/// The rings' segments sorted into horizontal bands, each holding the segments within `reach`
/// of it, so that the even–odd and clearance tests of a point read only its band's segments.
/// The answers are those of testing every segment: a segment crossing a point's row, or within
/// `reach` of the point, is always in the point's band.
struct RingIndex {
    segments: Vec<([f64; 2], [f64; 2])>,
    bottom: f64,
    height: f64,
    bands: usize,
    /// Band `k`'s segments are `members[starts[k]..starts[k + 1]]`.
    starts: Vec<usize>,
    members: Vec<u32>,
}

impl RingIndex {
    fn new(rings: &[Vec<[f64; 2]>], reach: f64) -> Self {
        let mut segments = Vec::new();
        for ring in rings {
            let n = ring.len();
            segments.extend((0..n).map(|i| (ring[i], ring[(i + 1) % n])));
        }
        let (mut bottom, mut top) = (f64::INFINITY, f64::NEG_INFINITY);
        for p in rings.iter().flatten() {
            bottom = bottom.min(p[1]);
            top = top.max(p[1]);
        }
        let mut index = Self {
            segments,
            bottom,
            height: 0.0,
            bands: 1,
            starts: Vec::new(),
            members: Vec::new(),
        };
        let mut bands = (index.segments.len() / 4).clamp(1, MAX_BANDS);
        if !(top > bottom && (top - bottom).is_finite() && reach.is_finite()) {
            bands = 1;
        }
        // Fewer bands while segments would be copied into many of them.
        let budget = 8 * index.segments.len() + MAX_BANDS;
        loop {
            index.bands = bands;
            index.height = (top - bottom) / bands as f64;
            let copies: usize = (0..index.segments.len())
                .map(|i| index.span(i, reach).count())
                .sum();
            if bands == 1 || copies <= budget {
                break;
            }
            bands /= 2;
        }
        let mut starts = vec![0usize; index.bands + 1];
        for i in 0..index.segments.len() {
            for k in index.span(i, reach) {
                starts[k + 1] += 1;
            }
        }
        for k in 0..index.bands {
            starts[k + 1] += starts[k];
        }
        let mut fill = starts.clone();
        let mut members = vec![0u32; starts[index.bands]];
        for i in 0..index.segments.len() {
            for k in index.span(i, reach) {
                members[fill[k]] = i as u32;
                fill[k] += 1;
            }
        }
        index.starts = starts;
        index.members = members;
        index
    }

    /// The band of height `y` (monotone in `y`; heights beyond the rings fall in the end bands).
    fn band(&self, y: f64) -> usize {
        if self.bands == 1 {
            return 0;
        }
        let k = ((y - self.bottom) / self.height).floor();
        if k >= 0.0 {
            (k as usize).min(self.bands - 1)
        } else {
            0
        }
    }

    /// The bands segment `i` belongs to: those its heights, widened by `reach`, overlap.
    fn span(&self, i: usize, reach: f64) -> std::ops::RangeInclusive<usize> {
        let (a, b) = self.segments[i];
        self.band(a[1].min(b[1]) - reach)..=self.band(a[1].max(b[1]) + reach)
    }

    fn near(&self, y: f64) -> impl Iterator<Item = ([f64; 2], [f64; 2])> + '_ {
        let k = self.band(y);
        self.members[self.starts[k]..self.starts[k + 1]]
            .iter()
            .map(|&i| self.segments[i as usize])
    }

    /// Segment tests a point at height `y` costs.
    fn cost(&self, y: f64) -> usize {
        let k = self.band(y);
        self.starts[k + 1] - self.starts[k]
    }

    /// Whether `p` lies inside the rings (even–odd rule).
    fn inside(&self, p: [f64; 2]) -> bool {
        self.near(p[1])
            .fold(false, |odd, (a, b)| odd ^ crosses_right(p, a, b))
    }

    /// Whether every segment lies farther than `clearance` (at most the index's reach) from `p`.
    fn clear(&self, p: [f64; 2], clearance: f64) -> bool {
        let mut best = f64::INFINITY;
        for (a, b) in self.near(p[1]) {
            best = best.min(segment_distance(p, a, b));
            if best <= clearance {
                return false;
            }
        }
        best > clearance
    }
}

/// Loops with more points than this are triangulated in one go ([`bulk_load`]).
const BULK_POINTS: usize = 4096;

/// The constrained triangulation of the loops, loaded in one go, with each point's 3D position
/// pushed onto `positions` by vertex index. Inserting points one at a time takes quadratic time
/// on a long straight run (every point on one line until the run ends), as damaged trims give.
fn bulk_load(
    rings: &[Vec<LoopPoint>],
    flat: &[Vec<[f64; 2]>],
    positions: &mut Vec<Option<[f64; 3]>>,
) -> Result<ConstrainedDelaunayTriangulation<Point2<f64>>, Skip> {
    // Repeated points are merged here, as one-at-a-time insertion merges them (the first one's
    // position kept), so that vertex indices stay those of `positions`.
    let mut seen: std::collections::HashMap<[u64; 2], usize> = std::collections::HashMap::new();
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    for (ring, points) in rings.iter().zip(flat) {
        let ids: Vec<usize> = ring
            .iter()
            .zip(points)
            .filter_map(|((_, p), at)| {
                let v = Point2::new(at[0], at[1]);
                spade::validate_vertex(&v).ok()?;
                // + 0.0 turns −0 into 0, which the triangulation counts as the same point.
                let key = [(at[0] + 0.0).to_bits(), (at[1] + 0.0).to_bits()];
                Some(*seen.entry(key).or_insert_with(|| {
                    vertices.push(v);
                    positions.push(Some(*p));
                    vertices.len() - 1
                }))
            })
            .collect();
        for k in 0..ids.len() {
            let (a, b) = (ids[k], ids[(k + 1) % ids.len()]);
            if a != b {
                edges.push([a, b]);
            }
        }
    }
    // Crossing constraints are left out, as `try_add_constraint` leaves them.
    ConstrainedDelaunayTriangulation::try_bulk_load_cdt(vertices, edges, |_| {})
        .map_err(|_| Skip::BadLoop)
}

/// The mesh of one curved face, in model units, on its grid subdivided `refine` times, with the
/// share of its area that faces against the surface's normal.
fn curved_mesh(
    t: &Topology,
    face: &Face,
    surface: &Surface,
    refine: usize,
) -> Result<(Mesh, f64), Skip> {
    let mut rings: Vec<Vec<LoopPoint>> = Vec::new();
    for &li in &face.loops {
        let lp = usize::try_from(li)
            .ok()
            .and_then(|i| t.loops.get(i))
            .ok_or(Skip::BadLoop)?;
        if lp.kind == LoopKind::Other {
            continue;
        }
        let ring = loop_uv(t, surface, &lp.trims).ok_or(Skip::Curved)?;
        if ring.len() >= 3 {
            rings.push(ring);
        }
    }
    if rings.is_empty() || rings.iter().map(Vec::len).sum::<usize>() > MAX_FACE_POINTS {
        return Err(Skip::BadLoop);
    }

    // Scale parameters to rough 3D lengths so the triangulation's shapes suit the surface.
    let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for (uv, _) in rings.iter().flatten() {
        for k in 0..2 {
            lo[k] = lo[k].min(uv[k]);
            hi[k] = hi[k].max(uv[k]);
        }
    }
    if !(0..2).all(|k| hi[k] > lo[k] && (hi[k] - lo[k]).is_finite()) {
        return Err(Skip::BadLoop);
    }
    let length = |a: [f64; 3], b: [f64; 3]| dot(sub(a, b), sub(a, b)).sqrt();
    // Length along the surface in steps (an end-to-end chord is zero on closed surfaces),
    // taken on three parameter lines in each direction.
    const STEPS: usize = 16;
    let along = |dir: usize| -> f64 {
        [0.0, 0.5, 1.0]
            .iter()
            .map(|&f| {
                let other = lo[1 - dir] + f * (hi[1 - dir] - lo[1 - dir]);
                let at = |k: usize| {
                    let t = lo[dir] + (hi[dir] - lo[dir]) * k as f64 / STEPS as f64;
                    if dir == 0 {
                        surface.point_at(t, other)
                    } else {
                        surface.point_at(other, t)
                    }
                };
                (0..STEPS).map(|k| length(at(k), at(k + 1))).sum::<f64>()
            })
            .fold(0.0, f64::max)
            / (hi[dir] - lo[dir])
    };
    let (su, sv) = (along(0), along(1));
    let (su, sv) = (su.max(1e-12), sv.max(1e-12));
    let scaled = |uv: [f64; 2]| [(uv[0] - lo[0]) * su, (uv[1] - lo[1]) * sv];
    let flat: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .map(|r| r.iter().map(|(uv, _)| scaled(*uv)).collect())
        .collect();

    // A boundary point keeps the shared edge's 3D point (crack-free) only where that point
    // agrees with the surface at its parameters; where the edge and trim are parameterized
    // differently, the same share can land elsewhere along the edge, so the surface point is
    // used instead.
    let size = {
        let (mut a, mut b) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
        for (_, p) in rings.iter().flatten() {
            for k in 0..3 {
                a[k] = a[k].min(p[k]);
                b[k] = b[k].max(p[k]);
            }
        }
        length(a, b)
    };
    for (uv, p) in rings.iter_mut().flatten() {
        let on_surface = surface.point_at(uv[0], uv[1]);
        if length(on_surface, *p) > EDGE_AGREEMENT * size {
            *p = on_surface;
        }
    }

    let mut positions: Vec<Option<[f64; 3]>> = Vec::new();
    let bulk = flat.iter().map(Vec::len).sum::<usize>() > BULK_POINTS;
    let mut cdt: ConstrainedDelaunayTriangulation<Point2<f64>> = if bulk {
        bulk_load(&rings, &flat, &mut positions)?
    } else {
        ConstrainedDelaunayTriangulation::new()
    };
    let mut put = |cdt: &mut ConstrainedDelaunayTriangulation<Point2<f64>>,
                   at: [f64; 2],
                   p: [f64; 3]|
     -> Option<spade::handles::FixedVertexHandle> {
        let h = cdt.insert(Point2::new(at[0], at[1])).ok()?;
        if positions.len() <= h.index() {
            positions.resize(h.index() + 1, None);
        }
        positions[h.index()].get_or_insert(p);
        Some(h)
    };
    for (ring, points) in rings.iter().zip(&flat).filter(|_| !bulk) {
        let handles: Vec<_> = ring
            .iter()
            .zip(points)
            .filter_map(|((_, p), at)| put(&mut cdt, *at, *p))
            .collect();
        for k in 0..handles.len() {
            let (a, b) = (handles[k], handles[(k + 1) % handles.len()]);
            if a != b {
                cdt.try_add_constraint(a, b);
            }
        }
    }

    // Interior points on the surface's grid, kept clear of the boundary.
    let domain = surface.domain();
    let us = subdivide(
        surface.grid_parameters(0, [lo[0].max(domain[0][0]), hi[0].min(domain[0][1])]),
        refine,
    );
    let vs = subdivide(
        surface.grid_parameters(1, [lo[1].max(domain[1][0]), hi[1].min(domain[1][1])]),
        refine,
    );
    let spacing = |ps: &[f64], s: f64| {
        ps.windows(2)
            .map(|w| (w[1] - w[0]) * s)
            .filter(|d| *d > 0.0)
            .fold(f64::INFINITY, f64::min)
    };
    let clearance = 0.3 * spacing(&us, su).min(spacing(&vs, sv));
    // The reach covers rounding in the distance arithmetic, so no segment within the clearance
    // falls outside a point's band.
    let reach = clearance * (1.0 + 1e-9) + 1e-9 * size_2d(&flat);
    let rings_index = RingIndex::new(&flat, reach);
    let (inner_us, inner_vs) = (
        &us[1..us.len().saturating_sub(1).max(1)],
        &vs[1..vs.len().saturating_sub(1).max(1)],
    );
    let grid_work = inner_vs
        .iter()
        .map(|&v| rings_index.cost(scaled([lo[0], v])[1]))
        .sum::<usize>()
        .saturating_mul(inner_us.len());
    if grid_work > MAX_FACE_WORK {
        return Err(Skip::TooComplex);
    }
    let mut work = grid_work;
    for &u in inner_us {
        for &v in inner_vs {
            let at = scaled([u, v]);
            if rings_index.inside(at) && rings_index.clear(at, clearance) {
                put(&mut cdt, at, surface.point_at(u, v));
            }
        }
    }

    let mut index = vec![u32::MAX; positions.len()];
    let mut out_positions = Vec::new();
    let mut triangles = Vec::new();
    let (mut total_area, mut folded_area) = (0.0, 0.0);
    for f in cdt.inner_faces() {
        let vs = f.vertices();
        let at = vs.map(|v| {
            let p = v.position();
            [p.x, p.y]
        });
        let centroid = [
            (at[0][0] + at[1][0] + at[2][0]) / 3.0,
            (at[0][1] + at[1][1] + at[2][1]) / 3.0,
        ];
        work += rings_index.cost(centroid[1]);
        if work > MAX_FACE_WORK {
            return Err(Skip::TooComplex);
        }
        if !rings_index.inside(centroid) {
            continue;
        }
        let ids = vs.map(|v| v.fix().index());
        let Some(points) = ids
            .iter()
            .map(|&i| positions.get(i).copied().flatten())
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let n = cross(sub(points[1], points[0]), sub(points[2], points[0]));
        if dot(n, n) == 0.0 {
            continue; // collapsed on the surface, as at a pole
        }
        // Self-check: the triangle should face the surface's natural normal at its centre.
        let centre = [lo[0] + centroid[0] / su, lo[1] + centroid[1] / sv];
        let natural = surface_normal(
            surface,
            centre,
            [(hi[0] - lo[0]) * 1e-4, (hi[1] - lo[1]) * 1e-4],
        );
        let triangle_area = dot(n, n).sqrt();
        total_area += triangle_area;
        if dot(n, natural) < 0.0 {
            folded_area += triangle_area;
        }
        let mut tri = [0u32; 3];
        for (k, &i) in ids.iter().enumerate() {
            if index[i] == u32::MAX {
                index[i] = out_positions.len() as u32;
                out_positions.push(points[k]);
            }
            tri[k] = index[i];
        }
        // Spade's faces are counter-clockwise in parameter space: the natural normal's side.
        triangles.push(if face.reversed {
            [tri[0], tri[2], tri[1]]
        } else {
            tri
        });
    }
    if triangles.is_empty() {
        return Err(Skip::Empty);
    }
    Ok((
        Mesh {
            positions_mm: out_positions,
            triangles,
            dropped_faces: 0,
        },
        folded_area / total_area,
    ))
}

/// `ps` with `n − 1` more values evenly between each consecutive two.
fn subdivide(ps: Vec<f64>, n: usize) -> Vec<f64> {
    if n <= 1 || ps.len() < 2 {
        return ps;
    }
    let mut out = vec![ps[0]];
    for w in ps.windows(2) {
        out.extend((1..=n).map(|i| w[0] + (w[1] - w[0]) * i as f64 / n as f64));
    }
    out
}

/// The surface's natural normal (∂S/∂u × ∂S/∂v, unnormalized) at `uv`, by central differences.
fn surface_normal(surface: &Surface, uv: [f64; 2], h: [f64; 2]) -> [f64; 3] {
    let du = sub(
        surface.point_at(uv[0] + h[0], uv[1]),
        surface.point_at(uv[0] - h[0], uv[1]),
    );
    let dv = sub(
        surface.point_at(uv[0], uv[1] + h[1]),
        surface.point_at(uv[0], uv[1] - h[1]),
    );
    cross(du, dv)
}

/// Diagnostics for one curved face: surface, parameter ranges, loop sizes.
#[cfg(all(test, feature = "cadmpeg"))]
pub(crate) fn describe_face(t: &Topology, face: &Face) -> String {
    let Some(surface) = usize::try_from(face.surface)
        .ok()
        .and_then(|i| t.surfaces.get(i))
        .and_then(Option::as_ref)
    else {
        return "no surface".to_string();
    };
    let mut out = format!("{} domain {:?}", surface.describe(), surface.domain());
    for &li in &face.loops {
        let Some(lp) = usize::try_from(li).ok().and_then(|i| t.loops.get(i)) else {
            continue;
        };
        match loop_uv(t, surface, &lp.trims) {
            Some(ring) => {
                let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
                for (uv, _) in &ring {
                    for k in 0..2 {
                        lo[k] = lo[k].min(uv[k]);
                        hi[k] = hi[k].max(uv[k]);
                    }
                }
                let flat: Vec<[f64; 2]> = ring.iter().map(|(uv, _)| *uv).collect();
                out += &format!(
                    "\n  loop {:?}: {} points, uv {lo:?}..{hi:?}, signed area {:.4}",
                    lp.kind,
                    ring.len(),
                    area(&flat)
                );
                for &ti in &lp.trims {
                    if let Some(trim) = usize::try_from(ti).ok().and_then(|i| t.trims.get(i)) {
                        out += &format!(
                            "\n    trim edge {} c2 {} proxy {:?} rev {} proxy_rev {}",
                            trim.edge,
                            trim.curve_2d,
                            trim.proxy,
                            trim.reversed,
                            trim.proxy_reversed
                        );
                    }
                }
            }
            None => out += "\n  loop: not built",
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring of `n` points around a centre, its radius jittered (`spikes` makes long thin
    /// zig-zags), from a fixed pseudo-random sequence.
    fn ring(
        seed: &mut u64,
        n: usize,
        centre: [f64; 2],
        radius: f64,
        spikes: bool,
    ) -> Vec<[f64; 2]> {
        let mut next = || {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (*seed >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                let r = if spikes && i % 2 == 1 {
                    radius * 0.1
                } else {
                    radius * (0.6 + 0.4 * next())
                };
                // Some points share a height exactly (horizontal segments, grid rows).
                let y = centre[1] + r * a.sin();
                let y = if i % 7 == 0 {
                    (y * 4.0).round() / 4.0
                } else {
                    y
                };
                [centre[0] + r * a.cos(), y]
            })
            .collect()
    }

    #[test]
    fn ring_index_answers_as_every_segment() {
        let mut seed = 7;
        for case in 0..24 {
            let n = [3, 4, 9, 40, 300, 2000][case % 6];
            let mut rings = vec![ring(&mut seed, n, [5.0, 5.0], 4.0, case % 4 == 3)];
            if case % 3 == 1 {
                rings.push(ring(&mut seed, n / 2 + 3, [5.5, 5.0], 1.5, false));
            }
            if case % 5 == 2 {
                let last = *rings[0].last().unwrap();
                rings[0].push(last); // a zero-length segment
            }
            let clearance = [0.0, 0.05, 0.3, 2.0][case % 4];
            let reach = clearance * (1.0 + 1e-9) + 1e-9 * size_2d(&rings);
            let index = RingIndex::new(&rings, reach);
            for i in 0..=60 {
                for j in 0..=60 {
                    // Points on and beyond the rings' extent, rows on ring heights included.
                    let p = [-0.5 + 11.0 * i as f64 / 60.0, -0.5 + 11.0 * j as f64 / 60.0];
                    assert_eq!(index.inside(p), inside(p, &rings), "case {case} at {p:?}");
                    assert_eq!(
                        index.clear(p, clearance),
                        boundary_distance(p, &rings) > clearance,
                        "case {case} at {p:?}"
                    );
                }
            }
            for p in rings.iter().flatten() {
                assert_eq!(index.inside(*p), inside(*p, &rings), "case {case} at {p:?}");
            }
        }
    }

    #[test]
    fn ring_index_on_flat_and_infinite_cases() {
        let flat = vec![vec![[0.0, 1.0], [2.0, 1.0], [4.0, 1.0]]];
        for clearance in [0.5, f64::INFINITY] {
            let index = RingIndex::new(&flat, clearance);
            for p in [[1.0, 1.0], [1.0, 0.0], [5.0, 3.0]] {
                assert_eq!(index.inside(p), inside(p, &flat));
                assert_eq!(
                    index.clear(p, clearance),
                    boundary_distance(p, &flat) > clearance
                );
            }
        }
    }
}
