//! SubD objects (`ON_SubD::Write`): the level-0 control cage, for subdividing into a mesh
//! ([`crate::subdivide`]).
//!
//! Layout: a byte (1 when a cage follows), then an anonymous chunk holding the `ON_SubDimple`:
//! version 1.minor (two i32), the level count, three u32, a bounding box (six f64), and one
//! anonymous chunk per level (more after it from minor 1 on: texture mapping, symmetry, hashes,
//! not needed here). A level chunk: version 1.minor (two i32, minor ≥ 1), the level index
//! (u16), three algorithm bytes (4 = Catmull-Clark), a bounding box, four u32 that split the
//! component ids into vertices, edges and faces, then the vertex, edge and face records in id
//! order and a render mesh flag.
//!
//! Every record starts with its id (u32), a u32 and its level (u16). Before archive 70 two
//! optional vectors follow (a size byte, 0 or 4, then three f64 when 4) and the record ends with
//! a zero byte; from archive 70 on, optional "additions" follow instead (a size byte: 0 absent,
//! the expected size then the value, 254 an anonymous chunk to skip, 255 the end of the record's
//! additions; otherwise that many bytes to skip), as at the record's end. References to other
//! components are their id (u32) and a flag byte (bit 0: used reversed).
//!
//! - Vertex: tag (u8: 1 smooth, 2 crease, 3 corner, 4 dart), the control point (three f64),
//!   edge and face counts (u16 each), a byte that announces saved limit points (then a u32
//!   count and per point twelve f64 and a reference), then the edges and the faces (a u16 count
//!   and references each).
//! - Edge: tag (u8: 1 smooth, 2 crease, 4 smooth X), its face count (u16), two sector
//!   coefficients and a sharpness (f64), its two vertices (u16 count = 2 and references), its
//!   faces (u16 count and references); from archive 80 an optional end sharpness (size 8).
//! - Face: two u32, its edge count twice (u16), the edges in order (references, reversed when
//!   the face runs against the edge); from archive 70 optional packing rectangle (size 34),
//!   material channel, colour, pack id (4 each) and texture points.

use super::cursor::Cursor;
use super::{Context, ReadError, Result};
use crate::subdivide::Cage;

const TCODE_ANONYMOUS: u32 = 0x4000_8000;
/// Components per level at most (as openNURBS bounds a level).
const MAX_COMPONENTS: usize = 4_000_000;

/// The level-0 cage of a SubD object, points in millimetres; `None` for an empty SubD.
pub(crate) fn read(data: &[u8], ctx: &Context, mm_per_unit: f64) -> Result<Option<Cage>> {
    let mut c = Cursor::new(data);
    if c.u8()? != 1 {
        return Ok(None);
    }
    let mut imple = c.chunk(ctx.width, TCODE_ANONYMOUS)?;
    if imple.i32()? != 1 {
        return Err(ReadError::Unsupported("SubDimple version"));
    }
    imple.i32()?; // minor
    let levels = imple.u32()?;
    if levels == 0 {
        return Ok(None);
    }
    imple.skip(12 + 48)?; // three u32, bounding box
    let level = imple.chunk(ctx.width, TCODE_ANONYMOUS)?;
    level_zero(level, ctx.archive, mm_per_unit).map(Some)
}

fn level_zero(mut c: Cursor, archive: u32, mm_per_unit: f64) -> Result<Cage> {
    let at = c.position();
    let bad = |what| ReadError::Invalid { at, what };
    if c.i32()? != 1 || c.i32()? < 1 {
        return Err(ReadError::Unsupported("SubD level version"));
    }
    if c.u16()? != 0 {
        return Err(bad("SubD level index"));
    }
    if (0..3).any(|_| c.u8().ok() != Some(4)) {
        return Err(ReadError::Unsupported("SubD subdivision algorithm"));
    }
    c.skip(48)?; // bounding box
    let ids = [c.u32()?, c.u32()?, c.u32()?, c.u32()?];
    if ids.windows(2).any(|w| w[1] < w[0])
        || (ids[3] - ids[0]) as usize > MAX_COMPONENTS
        || (ids[3] - ids[0]) as usize > c.remaining() / 10
    {
        return Err(bad("SubD component ids"));
    }
    let (vertices, edges, faces) = (ids[1] - ids[0], ids[2] - ids[1], ids[3] - ids[2]);
    let mut cage = Cage::default();

    for id in ids[0]..ids[1] {
        base(&mut c, id, archive)?;
        let tag = c.u8()?;
        let p = c.point3()?;
        if !p.iter().all(|v| v.is_finite()) {
            return Err(bad("SubD control point"));
        }
        cage.points.push(p.map(|v| v * mm_per_unit));
        cage.corner.push(tag == 3);
        let (edge_count, face_count) = (c.u16()?, c.u16()?);
        if c.u8()? != 0 {
            // Saved limit points: twelve f64 and a reference each.
            let n = c.count(12 * 8 + 5)?;
            c.skip(n * (12 * 8 + 5))?;
        }
        for expected in [edge_count, face_count] {
            if c.u16()? != expected {
                return Err(bad("SubD vertex incidence"));
            }
            c.skip(5 * usize::from(expected))?;
        }
        record_end(&mut c, archive)?;
    }

    for id in ids[1]..ids[2] {
        base(&mut c, id, archive)?;
        let tag = c.u8()?;
        let face_count = c.u16()?;
        c.skip(3 * 8)?; // sector coefficients, sharpness
        if c.u16()? != 2 {
            return Err(bad("SubD edge vertices"));
        }
        let mut ends = [0u32; 2];
        for end in &mut ends {
            let (v, _) = reference(&mut c)?;
            *end = v
                .checked_sub(ids[0])
                .filter(|&i| i < vertices)
                .ok_or(bad("SubD edge vertex"))?;
        }
        cage.edges.push(ends);
        cage.crease.push(tag == 2);
        if c.u16()? != face_count {
            return Err(bad("SubD edge faces"));
        }
        c.skip(5 * usize::from(face_count))?;
        if archive < 70 {
            zero(&mut c)?;
        } else {
            if archive >= 80 {
                match c.u8()? {
                    8 => c.skip(8)?, // end sharpness
                    255 => continue,
                    0 => {}
                    _ => return Err(bad("SubD edge addition")),
                }
            }
            additions(&mut c)?;
        }
    }

    for id in ids[2]..ids[3] {
        base(&mut c, id, archive)?;
        c.skip(8)?;
        let n = c.u16()?;
        if c.u16()? != n {
            return Err(bad("SubD face edges"));
        }
        let mut ring = Vec::with_capacity(usize::from(n));
        let mut sides = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            let (e, reversed) = reference(&mut c)?;
            let e = e
                .checked_sub(ids[1])
                .filter(|&i| i < edges)
                .ok_or(bad("SubD face edge"))?;
            let [a, b] = cage.edges[e as usize];
            ring.push(if reversed { b } else { a });
            sides.push(e);
        }
        if ring.len() >= 3 {
            cage.loops.push(ring);
            cage.sides.push(sides);
        }
        if archive < 70 {
            zero(&mut c)?;
        } else {
            face_additions(&mut c, n)?;
        }
    }
    let _ = faces;
    Ok(cage)
}

/// A component's id, a u32, its level, and (before archive 70) two optional vectors; from
/// archive 70 its additions.
fn base(c: &mut Cursor, expected: u32, archive: u32) -> Result<()> {
    let at = c.position();
    if c.u32()? != expected {
        return Err(ReadError::Invalid {
            at,
            what: "SubD component id",
        });
    }
    c.u32()?;
    c.u16()?;
    if archive < 70 {
        for _ in 0..2 {
            match c.u8()? {
                0 => {}
                4 => c.skip(24)?,
                _ => {
                    return Err(ReadError::Invalid {
                        at,
                        what: "SubD component vector",
                    });
                }
            }
        }
        Ok(())
    } else {
        // Displacement (24), group id (4), symmetry link (5): all skipped.
        additions(c)
    }
}

/// A reference to another component: its id and whether it is used reversed.
fn reference(c: &mut Cursor) -> Result<(u32, bool)> {
    let id = c.u32()?;
    let flags = c.u8()?;
    Ok((id, flags & 1 != 0))
}

fn record_end(c: &mut Cursor, archive: u32) -> Result<()> {
    if archive < 70 { zero(c) } else { additions(c) }
}

fn zero(c: &mut Cursor) -> Result<()> {
    let at = c.position();
    if c.u8()? != 0 {
        return Err(ReadError::Invalid {
            at,
            what: "SubD record end",
        });
    }
    Ok(())
}

/// Additions to the end (255): a size byte then that many bytes, 0 for an absent one, 254 for
/// an anonymous chunk.
fn additions(c: &mut Cursor) -> Result<()> {
    loop {
        match c.u8()? {
            255 => return Ok(()),
            254 => skip_anonymous(c)?,
            0 => {}
            size => c.skip(usize::from(size))?,
        }
    }
}

/// A face's additions: the texture points (announced by size 4 like the three u32 before them,
/// then a u32 count of ten-point groups of 240 bytes and a remainder) do not fit the
/// size-then-bytes rule, so the known ones are read in order.
fn face_additions(c: &mut Cursor, edges: u16) -> Result<()> {
    // Packing rectangle (34), material channel, colour, pack id (4 each).
    for size in [34u8, 4, 4, 4] {
        if !known_addition(c, size)? {
            return Ok(());
        }
    }
    // Texture points.
    match c.u8()? {
        255 => return Ok(()),
        254 => skip_anonymous(c)?,
        0 => {}
        4 => {
            let groups = c.u32()? as usize;
            for _ in 0..groups {
                c.u8()?; // 240
                c.skip(30 * 8)?;
            }
            let rest = usize::from(edges) % 10;
            if rest > 0 {
                c.u8()?;
                c.skip(rest * 24)?;
            }
        }
        size => c.skip(usize::from(size))?,
    }
    additions(c)
}

/// One expected addition: skips it when present; false at the end of the additions.
fn known_addition(c: &mut Cursor, size: u8) -> Result<bool> {
    loop {
        match c.u8()? {
            0 => return Ok(true),
            255 => return Ok(false),
            254 => skip_anonymous(c)?,
            s if s == size => {
                c.skip(usize::from(size))?;
                return Ok(true);
            }
            s => {
                c.skip(usize::from(s))?;
                return Ok(true);
            }
        }
    }
}

fn skip_anonymous(c: &mut Cursor) -> Result<()> {
    // Additions' chunks are written with the archive's chunk width; a four-byte length is the
    // only width before archive 50, and SubD exists from archive 60 on.
    let _ = c.chunk(crate::stream::frame::Width::Eight, TCODE_ANONYMOUS)?;
    Ok(())
}
