//! Meshes (`ON_Mesh::Read`): positions and triangles.
//!
//! Layout: version byte (major 1 or 3), vertex and face counts, a fixed 148-byte block of
//! domains and boxes, optional mesh-parameter and curvature chunks, the face array (index
//! width, then four indices per face; a triangle repeats its third index), then the vertex
//! arrays (major 3: compressed buffers V, N, T, K, C), then a tail gated by the minor version
//! that may hold double-precision positions.
//!
//! Faces with an index out of range are dropped, not the mesh. Quads are split along the
//! shorter diagonal, as openNURBS does.

use super::buffer;
use super::cursor::Cursor;
use super::record::Wrapper;
use super::{Context, ReadError, Result};
use crate::stream::frame::Width;

const TCODE_ANONYMOUS: u32 = 0x4000_8000;

/// `ON_V5_MeshDoubleVertices` userdata class id (`17f24e75-21be-4a7b-9f3d-7f85225247e3`).
const V5_DOUBLE_VERTICES: [u8; 16] = [
    0x75, 0x4e, 0xf2, 0x17, 0xbe, 0x21, 0x7b, 0x4a, 0x9f, 0x3d, 0x7f, 0x85, 0x22, 0x52, 0x47, 0xe3,
];

/// Writer stamp from which the mesh tail past the surface parameters is written.
const TAIL_STAMP: u32 = 200_606_010;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mesh {
    pub positions_mm: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
    /// Faces left out for an index out of range or fewer than three distinct vertices.
    pub dropped_faces: usize,
}

pub(crate) fn read(w: &Wrapper, ctx: &Context) -> Result<Mesh> {
    let scale = ctx
        .mm_per_unit
        .ok_or(ReadError::Unsupported("unit system none"))?;
    let width = ctx.width;
    let mut c = Cursor::new(w.data);
    let version = c.u8()?;
    let (major, minor) = (version >> 4, version & 0x0f);
    if major != 1 && major != 3 {
        return Err(ReadError::Unsupported("mesh version"));
    }
    let at = c.position();
    let vcount = usize::try_from(c.i32()?).map_err(|_| ReadError::Invalid {
        at,
        what: "vertex count",
    })?;
    let fcount = usize::try_from(c.i32()?).map_err(|_| ReadError::Invalid {
        at: at + 4,
        what: "face count",
    })?;
    c.skip(148)?; // texture and surface domains, scales, legacy boxes, closed flag
    if c.u8()? != 0 {
        c.chunk(width, TCODE_ANONYMOUS)?; // mesh parameters
    }
    for _ in 0..4 {
        if c.u8()? != 0 {
            c.chunk(width, TCODE_ANONYMOUS)?; // curvature statistics
        }
    }
    let at = c.position();
    let index_width = match c.i32()? {
        w @ (1 | 2 | 4) => w as usize,
        _ => {
            return Err(ReadError::Invalid {
                at,
                what: "face index width",
            });
        }
    };
    let face_bytes = fcount
        .checked_mul(4 * index_width)
        .ok_or(ReadError::Invalid {
            at,
            what: "face count",
        })?;
    let faces = c.bytes(face_bytes)?;

    let floats: Vec<[f32; 3]> = if major == 1 {
        let n = c.count(12)?;
        let v = f32x3(c.bytes(n * 12)?);
        for item in [12, 8, 16, 4] {
            c.skip_array(item)?; // normals, texture coordinates, curvatures, colours
        }
        v
    } else if vcount > 0 {
        let expected = vcount.checked_mul(12).ok_or(ReadError::Invalid {
            at,
            what: "vertex count",
        })?;
        let v = buffer::read(&mut c, width, expected)?.ok_or(ReadError::Invalid {
            at: c.position(),
            what: "missing vertex buffer",
        })?;
        for _ in 0..4 {
            buffer::skip(&mut c, width)?; // normals, texture coordinates, curvatures, colours
        }
        f32x3(&v)
    } else {
        Vec::new()
    };
    let vcount = floats.len();

    let mut doubles = None;
    if major == 3 {
        doubles = tail_doubles(&mut c, minor, vcount, ctx)?.filter(|d| synchronized(d, &floats));
    }
    if doubles.is_none() && ctx.archive == 50 {
        doubles = v5_doubles(w, width, &floats);
    }
    let positions_mm: Vec<[f64; 3]> = match doubles {
        Some(d) => d.iter().map(|p| p.map(|v| v * scale)).collect(),
        None => floats
            .iter()
            .map(|p| p.map(|v| f64::from(v) * scale))
            .collect(),
    };

    let (triangles, dropped_faces) = triangulate(faces, index_width, &positions_mm);
    Ok(Mesh {
        positions_mm,
        triangles,
        dropped_faces,
    })
}

fn f32x3(bytes: &[u8]) -> Vec<[f32; 3]> {
    bytes
        .chunks_exact(12)
        .map(|b| {
            [
                f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                f32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                f32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            ]
        })
        .collect()
}

fn f64x3(bytes: &[u8]) -> Vec<[f64; 3]> {
    bytes
        .chunks_exact(24)
        .map(|b| {
            let f = |i: usize| f64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
            [f(0), f(8), f(16)]
        })
        .collect()
}

/// Walks the tail after the vertex arrays to the double-precision vertex chunk (3.7+).
fn tail_doubles(
    c: &mut Cursor,
    minor: u8,
    vcount: usize,
    ctx: &Context,
) -> Result<Option<Vec<[f64; 3]>>> {
    if minor < 7 || ctx.stamp < TAIL_STAMP {
        return Ok(None);
    }
    let width = ctx.width;
    c.skip(4)?; // packed texture rotation
    c.skip(16)?; // texture mapping id
    if vcount > 0 {
        buffer::skip(c, width)?; // surface parameters
    }
    c.chunk(width, TCODE_ANONYMOUS)?; // mapping tag
    c.skip(3)?; // manifold, oriented, solid
    if c.u8()? != 0 {
        c.chunk(width, TCODE_ANONYMOUS)?; // n-gons
    }
    if c.u8()? == 0 {
        return Ok(None);
    }
    let mut chunk = c.chunk(width, TCODE_ANONYMOUS)?;
    if chunk.i32()? != 1 {
        return Ok(None);
    }
    chunk.i32()?;
    let n = chunk.u32()? as usize;
    if n == 0 || n != vcount {
        return Ok(None);
    }
    let Some(expected) = n.checked_mul(24) else {
        return Ok(None);
    };
    Ok(buffer::read(&mut chunk, width, expected)?.map(|b| f64x3(&b)))
}

/// openNURBS keeps double positions only when each matches its float within 1e-6 of the
/// float's largest coordinate.
fn synchronized(doubles: &[[f64; 3]], floats: &[[f32; 3]]) -> bool {
    doubles.len() == floats.len()
        && doubles.iter().zip(floats).all(|(d, f)| {
            let scale = f64::from(f.iter().fold(0.0f32, |m, v| m.max(v.abs())));
            d.iter()
                .zip(f)
                .all(|(a, b)| (a - f64::from(*b)).abs() <= scale * 1e-6)
        })
}

/// Double positions stored as `ON_V5_MeshDoubleVertices` userdata (archive 50), kept only
/// when every one rounds exactly to its float.
fn v5_doubles(w: &Wrapper, width: Width, floats: &[[f32; 3]]) -> Option<Vec<[f64; 3]>> {
    w.userdata.iter().find_map(|body| {
        let mut c = Cursor::new(body);
        if c.u8().ok()? >> 4 != 2 {
            return None;
        }
        let mut header = c.chunk(width, 0x0002_fff9).ok()?;
        if header.uuid().ok()? != V5_DOUBLE_VERTICES {
            return None;
        }
        let mut outer = c.chunk(width, TCODE_ANONYMOUS).ok()?;
        let mut inner = outer.chunk(width, TCODE_ANONYMOUS).ok()?;
        if inner.i32().ok()? != 1 {
            return None;
        }
        inner.i32().ok()?;
        inner.skip(16).ok()?; // float and double counts and CRCs
        let n = inner.count(24).ok()?;
        let d = f64x3(inner.bytes(n * 24).ok()?);
        let exact = d.len() == floats.len()
            && d.iter()
                .zip(floats)
                .all(|(d, f)| d.iter().zip(f).all(|(a, b)| (*a as f32) == *b));
        exact.then_some(d)
    })
}

pub(super) fn triangulate(
    faces: &[u8],
    index_width: usize,
    positions: &[[f64; 3]],
) -> (Vec<[u32; 3]>, usize) {
    let index = |b: &[u8], i: usize| -> u32 {
        match index_width {
            1 => u32::from(b[i]),
            2 => u32::from(u16::from_le_bytes([b[2 * i], b[2 * i + 1]])),
            _ => u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().expect("4 bytes")),
        }
    };
    let n = positions.len();
    let mut triangles = Vec::with_capacity(faces.len() / (4 * index_width) * 2);
    let mut dropped = 0;
    for face in faces.chunks_exact(4 * index_width) {
        let vi = [
            index(face, 0),
            index(face, 1),
            index(face, 2),
            index(face, 3),
        ];
        if vi.iter().any(|&i| i as usize >= n) {
            dropped += 1;
            continue;
        }
        let mut unique = [0u32; 4];
        let mut count = 0;
        for i in vi {
            if !unique[..count].contains(&i) {
                unique[count] = i;
                count += 1;
            }
        }
        match count {
            3 => triangles.push([unique[0], unique[1], unique[2]]),
            4 => {
                // Squared lengths: plain IEEE arithmetic, so native and WebAssembly builds
                // pick the same diagonal.
                let d = |a: u32, b: u32| {
                    let (p, q) = (positions[a as usize], positions[b as usize]);
                    let (x, y, z) = (p[0] - q[0], p[1] - q[1], p[2] - q[2]);
                    x * x + y * y + z * z
                };
                if d(vi[0], vi[2]) <= d(vi[1], vi[3]) {
                    triangles.push([vi[0], vi[1], vi[2]]);
                    triangles.push([vi[0], vi[2], vi[3]]);
                } else {
                    triangles.push([vi[0], vi[1], vi[3]]);
                    triangles.push([vi[1], vi[2], vi[3]]);
                }
            }
            _ => dropped += 1,
        }
    }
    (triangles, dropped)
}
