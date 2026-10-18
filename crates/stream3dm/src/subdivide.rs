//! Displayable meshes for SubD objects: the control cage, subdivided with Catmull–Clark rules,
//! approximates the limit surface Rhino draws. Crease edges (and borders) subdivide as curves
//! of their own, corner vertices stay put.

#[cfg(feature = "cadmpeg")]
use cadmpeg_ir::subd::SubdCage;
#[cfg(feature = "cadmpeg")]
use serde_json::Value;

/// Subdivision steps for display: close to the surface Rhino draws.
pub(crate) const LEVELS: usize = 2;

/// A polygon cage: points, which vertices are corners, edges with their crease flag, and
/// faces as vertex loops with the edge of each side (side `i` runs from `loop[i]` to
/// `loop[i + 1]`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Cage {
    pub points: Vec<[f64; 3]>,
    pub corner: Vec<bool>,
    pub edges: Vec<[u32; 2]>,
    pub crease: Vec<bool>,
    pub loops: Vec<Vec<u32>>,
    pub sides: Vec<Vec<u32>>,
}

/// Positions and triangles of a mesh.
pub(crate) type Triangles = (Vec<[f64; 3]>, Vec<[u32; 3]>);

/// Subdivides cadmpeg's `cage` `levels` times and triangulates it: positions (the cage's units)
/// and triangles. None when the cage cannot be read.
#[cfg(feature = "cadmpeg")]
pub(crate) fn mesh(cage: &SubdCage, levels: usize) -> Option<Triangles> {
    Some(triangulate(from_ir(cage)?, levels))
}

/// Subdivides `c` `levels` times and triangulates it: positions (the cage's units) and
/// triangles.
pub(crate) fn triangulate(mut c: Cage, levels: usize) -> Triangles {
    for _ in 0..levels {
        c = step(&c);
    }
    let mut triangles = Vec::new();
    for l in &c.loops {
        for k in 1..l.len().saturating_sub(1) {
            triangles.push([l[0], l[k], l[k + 1]]);
        }
    }
    (c.points, triangles)
}

/// Reads the cage through its serialized form (its lists have no accessors).
#[cfg(feature = "cadmpeg")]
fn from_ir(cage: &SubdCage) -> Option<Cage> {
    let v = serde_json::to_value(cage).ok()?;
    let point = |p: &Value| -> Option<[f64; 3]> {
        if let Some(a) = p.as_array() {
            return Some([
                a.first()?.as_f64()?,
                a.get(1)?.as_f64()?,
                a.get(2)?.as_f64()?,
            ]);
        }
        Some([
            p.get("x")?.as_f64()?,
            p.get("y")?.as_f64()?,
            p.get("z")?.as_f64()?,
        ])
    };
    let mut c = Cage::default();
    for vertex in v.get("vertices")?.as_array()? {
        c.points.push(point(vertex.get("point")?)?);
        c.corner
            .push(vertex.get("tag").and_then(Value::as_str) == Some("corner"));
    }
    for edge in v.get("edges")?.as_array()? {
        let ends = edge.get("vertices")?.as_array()?;
        let (a, b) = (
            ends.first()?.as_u64()? as u32,
            ends.get(1)?.as_u64()? as u32,
        );
        if a as usize >= c.points.len() || b as usize >= c.points.len() {
            return None;
        }
        c.edges.push([a, b]);
        c.crease
            .push(edge.get("tag").and_then(Value::as_str) == Some("crease"));
    }
    for face in v.get("faces")?.as_array()? {
        let mut ring = Vec::new();
        let mut sides = Vec::new();
        for usage in face.get("edges")?.as_array()? {
            let e = usage.get("edge")?.as_u64()? as usize;
            let reversed = usage.get("reversed")?.as_bool()?;
            let [a, b] = *c.edges.get(e)?;
            ring.push(if reversed { b } else { a });
            sides.push(e as u32);
        }
        if ring.len() >= 3 {
            c.loops.push(ring);
            c.sides.push(sides);
        }
    }
    Some(c)
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// One Catmull–Clark step: every n-sided face becomes n quads.
fn step(c: &Cage) -> Cage {
    let (nv, ne, nf) = (c.points.len(), c.edges.len(), c.loops.len());
    // Face points.
    let face_points: Vec<[f64; 3]> = c
        .loops
        .iter()
        .map(|l| {
            let s = l
                .iter()
                .fold([0.0; 3], |s, &v| add(s, c.points[v as usize]));
            scale(s, 1.0 / l.len() as f64)
        })
        .collect();
    // Faces around each edge.
    let mut edge_faces: Vec<Vec<usize>> = vec![Vec::new(); ne];
    for (f, sides) in c.sides.iter().enumerate() {
        for &e in sides {
            edge_faces[e as usize].push(f);
        }
    }
    let sharp: Vec<bool> = (0..ne)
        .map(|e| c.crease[e] || edge_faces[e].len() != 2)
        .collect();
    // Edge points: midpoints on creases and borders, else the smooth rule.
    let edge_points: Vec<[f64; 3]> = (0..ne)
        .map(|e| {
            let [a, b] = c.edges[e];
            let mid = add(c.points[a as usize], c.points[b as usize]);
            if sharp[e] {
                scale(mid, 0.5)
            } else {
                let fs = &edge_faces[e];
                scale(add(mid, add(face_points[fs[0]], face_points[fs[1]])), 0.25)
            }
        })
        .collect();
    // Vertex points.
    let mut vertex_edges: Vec<Vec<usize>> = vec![Vec::new(); nv];
    for (e, &[a, b]) in c.edges.iter().enumerate() {
        vertex_edges[a as usize].push(e);
        vertex_edges[b as usize].push(e);
    }
    let mut vertex_faces: Vec<Vec<usize>> = vec![Vec::new(); nv];
    for (f, l) in c.loops.iter().enumerate() {
        for &v in l {
            vertex_faces[v as usize].push(f);
        }
    }
    let other = |e: usize, v: usize| {
        let [a, b] = c.edges[e];
        if a as usize == v { b } else { a }
    };
    let vertex_points: Vec<[f64; 3]> = (0..nv)
        .map(|v| {
            let p = c.points[v];
            let creases: Vec<usize> = vertex_edges[v]
                .iter()
                .copied()
                .filter(|&e| sharp[e])
                .collect();
            let n = vertex_edges[v].len();
            if c.corner[v] || creases.len() > 2 || n < 2 {
                p
            } else if creases.len() == 2 {
                let ends = add(
                    c.points[other(creases[0], v) as usize],
                    c.points[other(creases[1], v) as usize],
                );
                add(scale(p, 0.75), scale(ends, 0.125))
            } else if vertex_faces[v].is_empty() || n < 3 {
                p
            } else {
                // Smooth (and dart) vertices: (Q + 2R + (n − 3) P) / n.
                let q = scale(
                    vertex_faces[v]
                        .iter()
                        .fold([0.0; 3], |s, &f| add(s, face_points[f])),
                    1.0 / vertex_faces[v].len() as f64,
                );
                let r = scale(
                    vertex_edges[v].iter().fold([0.0; 3], |s, &e| {
                        let [a, b] = c.edges[e];
                        add(
                            s,
                            scale(add(c.points[a as usize], c.points[b as usize]), 0.5),
                        )
                    }),
                    1.0 / n as f64,
                );
                let n = n as f64;
                scale(add(add(q, scale(r, 2.0)), scale(p, n - 3.0)), 1.0 / n)
            }
        })
        .collect();

    let mut out = Cage {
        points: vertex_points,
        corner: c.corner.clone(),
        ..Cage::default()
    };
    out.points.extend(edge_points);
    out.points.extend(face_points);
    out.corner.extend(std::iter::repeat_n(false, ne + nf));
    // Each old edge in two halves (2e at its first end, 2e + 1 at its second), keeping its
    // crease; then one interior edge per face side, from the side's edge point to the face
    // point.
    for (e, &[a, b]) in c.edges.iter().enumerate() {
        let mid = (nv + e) as u32;
        out.edges.push([a, mid]);
        out.edges.push([mid, b]);
        out.crease.push(c.crease[e]);
        out.crease.push(c.crease[e]);
    }
    let half = |e: u32, v: u32| -> u32 {
        if c.edges[e as usize][0] == v {
            2 * e
        } else {
            2 * e + 1
        }
    };
    let mut interior = Vec::with_capacity(nf);
    for (f, sides) in c.sides.iter().enumerate() {
        interior.push(out.edges.len() as u32);
        for &e in sides {
            out.edges
                .push([(nv + e as usize) as u32, (nv + ne + f) as u32]);
            out.crease.push(false);
        }
    }
    for (f, (l, sides)) in c.loops.iter().zip(&c.sides).enumerate() {
        let n = l.len();
        let fp = (nv + ne + f) as u32;
        for i in 0..n {
            let v = l[i];
            let (e, before) = (sides[i], sides[(i + n - 1) % n]);
            let (ep, bp) = ((nv + e as usize) as u32, (nv + before as usize) as u32);
            out.loops.push(vec![v, ep, fp, bp]);
            out.sides.push(vec![
                half(e, v),
                interior[f] + i as u32,
                interior[f] + ((i + n - 1) % n) as u32,
                half(before, v),
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cube cage of 6 quads (side 2, centred at the origin).
    fn cube(creased: bool) -> Cage {
        let points = (0..8)
            .map(|i| {
                [
                    if i & 1 == 0 { -1.0 } else { 1.0 },
                    if i & 2 == 0 { -1.0 } else { 1.0 },
                    if i & 4 == 0 { -1.0 } else { 1.0 },
                ]
            })
            .collect::<Vec<_>>();
        let faces = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let mut c = Cage {
            points,
            corner: vec![false; 8],
            ..Cage::default()
        };
        let mut index = std::collections::HashMap::new();
        for f in faces {
            let mut sides = Vec::new();
            for i in 0..4 {
                let (a, b) = (f[i], f[(i + 1) % 4]);
                let key = (a.min(b), a.max(b));
                let e = *index.entry(key).or_insert_with(|| {
                    c.edges.push([a, b]);
                    c.crease.push(creased);
                    c.edges.len() - 1
                });
                sides.push(e as u32);
            }
            c.loops.push(f.to_vec());
            c.sides.push(sides);
        }
        c
    }

    #[test]
    fn a_cube_rounds_toward_a_sphere_and_stays_closed() {
        let mut c = cube(false);
        for _ in 0..2 {
            c = step(&c);
        }
        assert_eq!(c.loops.len(), 6 * 16);
        // Closed: every edge has two faces.
        let mut uses = vec![0; c.edges.len()];
        for sides in &c.sides {
            for &e in sides {
                uses[e as usize] += 1;
            }
        }
        assert!(uses.iter().all(|&u| u == 2));
        // Shrunk inside the cage, all points at a similar distance from the centre.
        let r: Vec<f64> = c
            .points
            .iter()
            .map(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt())
            .collect();
        let (lo, hi) = r
            .iter()
            .fold((f64::MAX, 0.0f64), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        assert!(hi < 1.6 && lo > 0.6, "radii {lo}..{hi}");
        // Every quad's sides join its loop's vertices in order.
        for (l, sides) in c.loops.iter().zip(&c.sides) {
            for i in 0..4 {
                let [a, b] = c.edges[sides[i] as usize];
                let (p, q) = (l[i], l[(i + 1) % 4]);
                assert!((a, b) == (p, q) || (a, b) == (q, p));
            }
        }
    }

    #[test]
    fn creased_cage_keeps_its_box_shape() {
        let mut c = cube(true);
        for k in 0..8 {
            c.corner[k] = true;
        }
        for _ in 0..2 {
            c = step(&c);
        }
        // All creases and corners: a subdivided box, every point on the original surface.
        for p in &c.points {
            let on_face = p.iter().any(|x| (x.abs() - 1.0).abs() < 1e-9);
            assert!(on_face, "{p:?}");
        }
    }
}
