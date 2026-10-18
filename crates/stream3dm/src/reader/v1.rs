//! Rhino 1 files (archive 1): a flat run of chunks after the 32-byte header, with no tables and
//! no object records. Each chunk is a typecode (u32) and a length (u32; for short chunks, with
//! the top bit of the typecode set, the value itself). Read here:
//!
//! - units and tolerances (`0x02000010`): as the later units record of version 1;
//! - points (`0x00100001`): three f64;
//! - curves (`0x00010008`): a "curve stuff" child (dimension, closure, segment count, box)
//!   followed by spline chunks (`0x00010009`), each with a "spline stuff" child: dimension,
//!   rational mode (1: x, y, z, w; 2: w·x, w·y, w·z, w), order, control point count, closure,
//!   form, a box, a clamp flag (order > 2) saying which end knots are left out (stored once),
//!   the knots (openNURBS's order + count − 2) and the control points;
//! - trimmed surfaces: a shell (`0x00010003`: outer flag, face count, box, glue indices, then
//!   faces) or a lone face (`0x00010004`). A face's stuff: reversed flag, type, twice its loop
//!   count, box, glue indices, its surface (`0x00010007`: dimension, form, degrees, span counts,
//!   rational and closure and singularity flags, box, knots, control points u-major) and its
//!   loops (`0x00010005`: trim count, kind 0 outer / 1 inner / −1 unset, 2D box, trims). A trim
//!   (`0x00010006`): flags (bit 0: it has an edge), reversed, continuity, monotonicity, 3D and 2D
//!   tolerances, its 2D curve, then its 3D curve when it has an edge;
//! - meshes (`0x00100015`): a compressed geometry child (`0x00100017`): point and face counts,
//!   normal and texture flags, a box, points as three u16 steps across the box, faces as four
//!   u16 (u32 from 65,535 points), then normals and texture points. Shells and faces often keep
//!   Rhino's render mesh in such a child too, used when present.
//!
//! - lights (`0x00800001`): counted;
//! - layers (`0x00400010`): children for the name (`0x00400011`: a u32 length, then the
//!   characters), the colour (short `0x80400001`, RGBA bytes) and the mode (short `0x80400033`:
//!   0 normal, 1 hidden, 2 locked);
//! - every object's attributes, short children before its geometry (after it, for points):
//!   its layer index (`0xA0000001`) and colour (`0x80400001`); user data (`0x40000001`: tag
//!   and data lengths, the tag, the data) turns points into annotations: `RhAnnotateArrow`
//!   (the other end, drawn as a line) and `RhAnnotateDot` (its text).
//!
//! The last child of a wrapper may run two bytes past it (the wrapper's CRC16 at its end is
//! counted in the child). Views and settings are not read. Trimmed surfaces without a render
//! mesh are tessellated like Brep faces ([`super::tessellate`]); each trim's edge is its 2D curve placed
//! on the surface (the file's 3D trim curves are not needed).

use super::curve::{Curve, Nurbs};
use super::document::{self, Document};
use super::surface::Surface;
use super::topology::{Edge, Face, Loop, LoopKind, Topology, Trim};
use super::{ReadError, Result, tessellate};
use crate::native::ObjectKind;
use crate::object::{Counted, MeshGeom, ObjectView};

const TCODE_SHORT: u32 = 0x8000_0000;
const TCODE_ENDOFFILE: u32 = 0x8000_7fff;
const UNITS: u32 = 0x0200_0010;
const POINT: u32 = 0x0010_0001;
const CURVE: u32 = 0x0001_0008;
const CURVE_STUFF: u32 = 0x0001_0108;
const SPLINE: u32 = 0x0001_0009;
const SPLINE_STUFF: u32 = 0x0001_0109;
const SHELL: u32 = 0x0001_0003;
const SHELL_STUFF: u32 = 0x0001_0103;
const FACE: u32 = 0x0001_0004;
const FACE_STUFF: u32 = 0x0001_0104;
const LOOP: u32 = 0x0001_0005;
const LOOP_STUFF: u32 = 0x0001_0105;
const TRIM: u32 = 0x0001_0006;
const TRIM_STUFF: u32 = 0x0001_0106;
const SURFACE: u32 = 0x0001_0007;
const SURFACE_STUFF: u32 = 0x0001_0107;
const MESH: u32 = 0x0010_0015;
const MESH_GEOMETRY: u32 = 0x0010_0017;
const LAYER: u32 = 0x0040_0010;
const LIGHT: u32 = 0x0080_0001;
const LAYER_NAME: u32 = 0x0040_0011;
const COLOR: u32 = 0x8040_0001;
const LAYER_MODE: u32 = 0x8040_0033;
const OBJECT_LAYER: u32 = 0xA000_0001;
const USER_DATA: u32 = 0x4000_0001;
/// Header bytes before the first chunk.
const HEADER: usize = 32;
/// Orders above this are not decoded (as for later files).
const MAX_ORDER: usize = super::curve::MAX_ORDER;

/// One chunk: typecode, body (empty for a short chunk) and where the next one starts.
#[derive(Clone, Copy)]
struct Chunk<'a> {
    typecode: u32,
    body: &'a [u8],
    /// A short chunk's value (the length otherwise).
    value: u32,
    next: usize,
}

/// The chunk at `at`, ending by `end`.
fn chunk(data: &[u8], at: usize, end: usize) -> Result<Chunk<'_>> {
    let bad = |what| ReadError::Invalid { at, what };
    let head = data.get(at..at + 8).ok_or(bad("V1 chunk header"))?;
    let typecode = u32::from_le_bytes(head[..4].try_into().expect("4 bytes"));
    let value = u32::from_le_bytes(head[4..].try_into().expect("4 bytes"));
    if typecode & TCODE_SHORT != 0 {
        return Ok(Chunk {
            typecode,
            body: &[],
            value,
            next: at + 8,
        });
    }
    let stop = at + 8 + value as usize;
    if stop > end.min(data.len()) {
        return Err(bad("V1 chunk length"));
    }
    Ok(Chunk {
        typecode,
        body: &data[at + 8..stop],
        value,
        next: stop,
    })
}

/// Reads chunks in order from a body (allowed to reach two bytes past it, see the module).
struct Children<'a> {
    data: &'a [u8],
    at: usize,
    end: usize,
}

impl<'a> Children<'a> {
    fn of(data: &'a [u8], body: &'a [u8]) -> Self {
        // `body` lies within `data` (a chunk's body or part of it), except a short chunk's
        // empty body, which has no children.
        let start = (body.as_ptr() as usize).wrapping_sub(data.as_ptr() as usize);
        if body.is_empty() || start > data.len() {
            return Self {
                data,
                at: 0,
                end: 0,
            };
        }
        Self {
            data,
            at: start,
            end: (start + body.len() + 2).min(data.len()),
        }
    }

    /// The next chunk, whatever it is.
    fn next(&mut self) -> Result<Chunk<'a>> {
        if self.at + 8 > self.end {
            return Err(ReadError::Invalid {
                at: self.at,
                what: "V1 children end",
            });
        }
        let c = chunk(self.data, self.at, self.end)?;
        self.at = c.next;
        Ok(c)
    }

    /// The next chunk, which must have typecode `want`.
    fn expect(&mut self, want: u32) -> Result<Chunk<'a>> {
        let c = chunk(self.data, self.at, self.end)?;
        if c.typecode != want {
            return Err(ReadError::Invalid {
                at: self.at,
                what: "V1 child chunk",
            });
        }
        self.at = c.next;
        Ok(c)
    }

    /// The first chunk from here with typecode `want`.
    fn find(&mut self, want: u32) -> Result<Chunk<'a>> {
        while self.at + 8 <= self.end {
            let c = chunk(self.data, self.at, self.end)?;
            self.at = c.next;
            if c.typecode == want {
                return Ok(c);
            }
        }
        Err(ReadError::Invalid {
            at: self.at,
            what: "V1 child chunk missing",
        })
    }
}

/// A little-endian reader over a chunk body.
struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self.b.get(self.at..self.at + N).ok_or(ReadError::Invalid {
            at: self.at,
            what: "V1 data ends early",
        })?;
        self.at += N;
        Ok(s.try_into().expect("N bytes"))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take()?))
    }
    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take()?))
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        if n > self.b.len() - self.at {
            return Err(ReadError::Invalid {
                at: self.at,
                what: "V1 data ends early",
            });
        }
        self.at += n;
        Ok(())
    }
    /// What follows, as a body for child chunks.
    fn rest(&self) -> &'a [u8] {
        &self.b[self.at..]
    }
}

/// Reads a Rhino 1 file: its units and tolerances as a document, and its objects. Trimmed
/// surfaces are tessellated when `tessellate` is set.
pub(crate) fn read(data: &[u8], tessellate: bool) -> Result<(Document, Vec<ObjectView>)> {
    let mut doc = Document {
        archive_version: Some(1),
        unit_system: Some("unset".to_string()),
        ..Document::default()
    };
    let mut objects = Vec::new();
    // Objects are read after the whole file, once the unit is known (it comes first in
    // practice, but nothing requires it).
    let mut chunks = Vec::new();
    let mut at = HEADER;
    while at + 8 <= data.len() {
        let c = chunk(data, at, data.len())?;
        if c.typecode == TCODE_ENDOFFILE {
            break;
        }
        if c.typecode == UNITS {
            // Same layout as a units record of version 1; a damaged one leaves the unit unset.
            let _ = document::units(c.body, &mut doc);
        } else if c.typecode == LIGHT {
            doc.lights += 1;
        } else if c.typecode == LAYER {
            let index = doc.layers.len();
            doc.layers.push(layer(data, c.body, index));
        } else {
            chunks.push(c);
        }
        at = c.next;
    }
    // No unit: 1 unit = 1 mm, as for later files.
    let scale = doc.mm_per_unit.unwrap_or(1.0);
    for c in chunks {
        if !matches!(c.typecode, POINT | CURVE | SHELL | FACE | MESH) {
            continue;
        }
        let n = objects.len();
        let mut view = blank(format!("v1 object {n}"));
        // Attributes: the children before the geometry (after it, for a point's 24 bytes).
        let from = if c.typecode == POINT {
            24.min(c.body.len())
        } else {
            0
        };
        attributes(data, &c.body[from..], &mut view);
        let ok = match c.typecode {
            POINT => point(data, c.body, scale, &mut view),
            CURVE => curve_object(data, c.body, scale, &mut view),
            SHELL | FACE => brep(data, c, scale, tessellate, &mut view),
            MESH => mesh(data, c.body, scale, &mut view),
            _ => continue,
        };
        if ok.is_err() {
            doc.unparsed += 1;
            view.kind = Counted::Object(ObjectKind::Other);
        }
        objects.push(view);
    }
    Ok((doc, objects))
}

/// A layer: its name, colour and mode.
fn layer(data: &[u8], body: &[u8], index: usize) -> crate::native::LayerRec {
    let mut rec = crate::native::LayerRec {
        archive_index: Some(index as i64),
        name: None,
        color: None,
        visible: Some(true),
        locked: Some(false),
        material_index: None,
        parent_uuid: None,
        source_uuid: None,
    };
    let mut children = Children::of(data, body);
    children.end = children.end.min(children.at + body.len());
    while let Ok(c) = children.next() {
        match c.typecode {
            LAYER_NAME => rec.name = text(c.body),
            COLOR => rec.color = Some(document::hex_rgba(&c.value.to_le_bytes())),
            LAYER_MODE => {
                rec.visible = Some(c.value != 1);
                rec.locked = Some(c.value == 2);
            }
            _ => {}
        }
    }
    rec
}

/// An object's attributes among the children of its wrapper (for a point, after its 24 bytes).
fn attributes(data: &[u8], body: &[u8], view: &mut ObjectView) {
    let mut children = Children::of(data, body);
    children.end = children.end.min(children.at + body.len());
    while let Ok(c) = children.next() {
        match c.typecode {
            OBJECT_LAYER => view.layer_index = Some(i64::from(c.value)),
            0xFFFF_FFFF => break,
            _ => {}
        }
    }
}

/// A u32 length, then that many characters (Latin-1), without a trailing NUL.
fn text(body: &[u8]) -> Option<String> {
    let n = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let chars = body.get(4..4 + n.min(body.len() - 4))?;
    let s: String = chars
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| char::from(b))
        .collect();
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

fn blank(key: String) -> ObjectView {
    ObjectView {
        uuid: None,
        fallback_key: key,
        name: None,
        layer_index: None,
        material_index: None,
        material_source: None,
        visible: Some(true),
        mode: None,
        groups: Vec::new(),
        user_strings: Vec::new(),
        kind: Counted::Object(ObjectKind::Other),
        meshes: Vec::new(),
        curves: Vec::new(),
        points: Vec::new(),
        instance: None,
        faces: 0,
        planar_faces: 0,
        generated_mesh: false,
        undisplayed_faces: 0,
        class: None,
        type_name: None,
    }
}

/// A point, or an annotation stored as one: its user data tagged `RhAnnotateArrow` holds the
/// arrow's other end (three f64), drawn as a line; `RhAnnotateDot`, the dot's text.
fn point(data: &[u8], body: &[u8], scale: f64, view: &mut ObjectView) -> Result<()> {
    view.kind = Counted::Object(ObjectKind::Point);
    let mut b = Bytes::new(body);
    let p = [b.f64()?, b.f64()?, b.f64()?];
    if !p.iter().all(|v| v.is_finite()) {
        return Ok(());
    }
    let mut children = Children::of(data, b.rest());
    children.end = children.end.min(children.at + b.rest().len());
    while let Ok(c) = children.next() {
        if c.typecode != USER_DATA {
            continue;
        }
        let Some((tag, value)) = user_data(c.body) else {
            continue;
        };
        if tag == b"RhAnnotateArrow" && value.len() >= 24 {
            let mut v = Bytes::new(value);
            let q = [v.f64()?, v.f64()?, v.f64()?];
            if q.iter().all(|x| x.is_finite()) {
                view.kind = Counted::Object(ObjectKind::Curve);
                view.curves
                    .push(vec![p.map(|x| x * scale), q.map(|x| x * scale)]);
                return Ok(());
            }
        } else if tag == b"RhAnnotateDot" {
            view.kind = Counted::Object(ObjectKind::Annotation);
            return Ok(());
        }
    }
    view.points.push(p.map(|v| v * scale));
    Ok(())
}

/// An object's user data: tag length (u32), data length (u32), the tag, the data.
fn user_data(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let n = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let m = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?) as usize;
    let tag = body.get(8..8 + n)?;
    let tag = &tag[..tag.iter().position(|&b| b == 0).unwrap_or(tag.len())];
    Some((tag, body.get(8 + n..(8 + n + m).min(body.len()))?))
}

fn curve_object(data: &[u8], body: &[u8], scale: f64, view: &mut ObjectView) -> Result<()> {
    view.kind = Counted::Object(ObjectKind::Curve);
    let curve = legacy_curve(data, body)?;
    let [t0, t1] = curve.domain();
    let points = curve.sample(t0, t1);
    if points.len() >= 2 && points.iter().flatten().all(|v| v.is_finite()) {
        view.curves
            .push(points.into_iter().map(|p| p.map(|v| v * scale)).collect());
    }
    Ok(())
}

/// A curve chunk's body: its splines joined as a polycurve.
fn legacy_curve(data: &[u8], body: &[u8]) -> Result<Curve> {
    let stuff = Children::of(data, body).find(CURVE_STUFF)?;
    let mut b = Bytes::new(stuff.body);
    let dim = usize::from(b.u8()?);
    if !(2..=3).contains(&dim) {
        return Err(ReadError::Unsupported("V1 curve dimension"));
    }
    b.u8()?; // closure
    let n = usize::from(b.u16()?);
    b.skip(dim * 16)?; // box
    let mut children = Children::of(data, b.rest());
    let mut segments = Vec::with_capacity(n);
    for _ in 0..n {
        let spline = children.expect(SPLINE)?;
        let stuff = Children::of(data, spline.body).find(SPLINE_STUFF)?;
        segments.push(Curve::Nurbs(spline_data(stuff.body)?));
    }
    if segments.is_empty() {
        return Err(ReadError::Invalid {
            at: 0,
            what: "V1 curve without splines",
        });
    }
    // Each segment over its own domain, end to end.
    let mut params = vec![segments[0].domain()[0]];
    for s in &segments {
        let [a, b] = s.domain();
        params.push(params.last().copied().unwrap_or(0.0) + (b - a));
    }
    if segments.len() == 1 {
        return Ok(segments.pop().expect("one"));
    }
    Ok(Curve::Poly { segments, params })
}

fn spline_data(body: &[u8]) -> Result<Nurbs> {
    let mut b = Bytes::new(body);
    let bad = |what| ReadError::Invalid { at: 0, what };
    let dim = usize::from(b.u8()?);
    let rational = b.u8()?;
    let order = usize::from(b.u8()?);
    let count = usize::from(b.u16()?);
    if !(2..=3).contains(&dim) || rational > 2 || !(2..=MAX_ORDER).contains(&order) || count < order
    {
        return Err(bad("V1 spline header"));
    }
    b.u8()?; // closure
    b.u8()?; // form
    b.skip(dim * 16)?; // box
    let clamped = if order > 2 { b.u8()? } else { 0 };
    let total = order + count - 2;
    // The clamped ends' repeated knots are stored once.
    let mut knots = vec![b.f64()?];
    if clamped & 1 != 0 {
        while knots.len() <= order - 2 {
            knots.push(knots[0]);
        }
    }
    while knots.len() < count {
        knots.push(b.f64()?);
    }
    let last = *knots.last().expect("non-empty");
    while knots.len() < total {
        knots.push(if clamped & 2 != 0 { last } else { b.f64()? });
    }
    if order == 2 && count == 2 && knots[0] > knots[1] {
        knots[0] = -knots[0];
        knots[1] = -knots[1];
    }
    if knots.windows(2).any(|k| k[1] < k[0]) {
        return Err(bad("V1 spline knots"));
    }
    let mut cvs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut p = [0.0, 0.0, 0.0, 1.0];
        for v in p.iter_mut().take(dim) {
            *v = b.f64()?;
        }
        if rational != 0 {
            let w = b.f64()?;
            if w == 0.0 || !w.is_finite() {
                return Err(bad("V1 spline weight"));
            }
            // Homogeneous control points: stored so in mode 2, scaled here in mode 1.
            if rational == 1 {
                for v in p.iter_mut().take(3) {
                    *v *= w;
                }
            }
            p[3] = w;
        }
        cvs.push(p);
    }
    Ok(Nurbs { order, knots, cvs })
}

fn surface_data(body: &[u8]) -> Result<Surface> {
    let mut b = Bytes::new(body);
    let bad = |what| ReadError::Invalid { at: 0, what };
    let dim = usize::from(b.u8()?);
    if !(2..=3).contains(&dim) {
        return Err(bad("V1 surface dimension"));
    }
    b.u8()?; // form
    let order = [usize::from(b.u8()?) + 1, usize::from(b.u8()?) + 1];
    let counts = [
        order[0] - 1 + usize::from(b.u16()?),
        order[1] - 1 + usize::from(b.u16()?),
    ];
    let modes = [b.u8()?, b.u8()?];
    if (0..2).any(|i| order[i] < 2 || order[i] > MAX_ORDER || counts[i] < order[i])
        || modes.iter().any(|&m| m > 2)
    {
        return Err(bad("V1 surface header"));
    }
    let rational = modes.into_iter().rfind(|&m| m != 0).unwrap_or(0);
    b.skip(4)?; // closure and singularity flags
    b.skip(dim * 16)?; // box
    let mut knots: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    for (i, k) in knots.iter_mut().enumerate() {
        let n = order[i] + counts[i] - 2;
        *k = (0..n).map(|_| b.f64()).collect::<Result<_>>()?;
        if k.windows(2).any(|w| w[1] < w[0]) {
            return Err(bad("V1 surface knots"));
        }
    }
    let total = counts[0] * counts[1];
    let mut cvs = Vec::with_capacity(total);
    for _ in 0..total {
        let mut p = [0.0, 0.0, 0.0, 1.0];
        for v in p.iter_mut().take(dim) {
            *v = b.f64()?;
        }
        if rational != 0 {
            let w = b.f64()?;
            if w == 0.0 || !w.is_finite() {
                return Err(bad("V1 surface weight"));
            }
            if rational == 1 {
                for v in p.iter_mut().take(3) {
                    *v *= w;
                }
            }
            p[3] = w;
        }
        cvs.push(p);
    }
    Ok(Surface::Nurbs {
        order,
        counts,
        knots,
        cvs,
    })
}

/// A shell or a lone face: one topology, tessellated.
fn brep(data: &[u8], c: Chunk, scale: f64, tessellate: bool, view: &mut ObjectView) -> Result<()> {
    view.kind = Counted::Object(ObjectKind::Brep { render_mesh: false });
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
    if c.typecode == FACE {
        face(data, c.body, &mut t)?;
    } else {
        let stuff = Children::of(data, c.body).find(SHELL_STUFF)?;
        let mut b = Bytes::new(stuff.body);
        b.i32()?; // outer
        let n = usize::try_from(b.i32()?).map_err(|_| ReadError::Invalid {
            at: 0,
            what: "V1 shell face count",
        })?;
        b.skip(48)?; // box
        let glue = usize::try_from(b.i32()?).unwrap_or(0);
        b.skip(2 * glue)?;
        let mut children = Children::of(data, b.rest());
        for _ in 0..n {
            let f = children.expect(FACE)?;
            face(data, f.body, &mut t)?;
        }
    }
    view.faces = t.faces.len();
    view.undisplayed_faces = t.faces.len();
    // Rhino's own render mesh, when the file keeps one beside the surfaces.
    let cached = Children::of(data, c.body)
        .find(MESH_GEOMETRY)
        .ok()
        .and_then(|g| compressed_mesh(g.body, scale).ok());
    if let Some(m) = cached {
        view.kind = Counted::Object(ObjectKind::Brep { render_mesh: true });
        view.undisplayed_faces = 0;
        view.meshes = meshes(view, vec![m]);
    } else if tessellate {
        let tess = tessellate::tessellate(&t, scale);
        view.generated_mesh = !tess.meshes.is_empty();
        view.undisplayed_faces = tess.skipped.len();
        view.meshes = meshes(view, tess.meshes);
    }
    Ok(())
}

/// Adds a face (its surface, loops and trims) to `t`.
fn face(data: &[u8], body: &[u8], t: &mut Topology) -> Result<()> {
    let stuff = Children::of(data, body).find(FACE_STUFF)?;
    let mut b = Bytes::new(stuff.body);
    let reversed = b.i32()? != 0;
    b.i32()?; // type
    let loops = usize::try_from(b.i32()? / 2).unwrap_or(0);
    b.skip(48)?; // box
    let glue = usize::try_from(b.i32()?).unwrap_or(0);
    b.skip(2 * glue)?;
    let mut children = Children::of(data, b.rest());
    let s = children.expect(SURFACE)?;
    let surface = surface_data(Children::of(data, s.body).find(SURFACE_STUFF)?.body)?;
    let mut face_loops = Vec::with_capacity(loops);
    for _ in 0..loops {
        let l = children.expect(LOOP)?;
        let stuff = Children::of(data, l.body).find(LOOP_STUFF)?;
        let mut lb = Bytes::new(stuff.body);
        let n = usize::try_from(lb.i32()?).unwrap_or(0);
        let kind = match lb.i32()? {
            0 => LoopKind::Outer,
            _ => LoopKind::Inner, // inner, or unset: the tessellator sorts them by area
        };
        lb.skip(32)?; // 2D box
        let mut trims = Children::of(data, lb.rest());
        let mut loop_trims = Vec::with_capacity(n);
        for _ in 0..n {
            let tr = trims.expect(TRIM)?;
            loop_trims.push(trim(data, tr.body, &surface, t)?);
        }
        face_loops.push(t.loops.len() as i32);
        t.loops.push(Loop {
            trims: loop_trims,
            kind,
        });
    }
    t.faces.push(Face {
        loops: face_loops,
        surface: t.surfaces.len() as i32,
        reversed,
    });
    t.surfaces.push(Some(surface));
    Ok(())
}

/// Adds a trim to `t`, returning its index. Its edge (when it has one) is its 2D curve placed
/// on the surface, sampled at the 2D curve's own parameters, so trim and edge agree by
/// construction.
fn trim(data: &[u8], body: &[u8], surface: &Surface, t: &mut Topology) -> Result<i32> {
    let stuff = Children::of(data, body).find(TRIM_STUFF)?;
    let mut b = Bytes::new(stuff.body);
    b.u8()?; // flags (bit 0: a 3D curve follows the 2D one; not needed here)
    b.skip(4 + 4 + 4 + 8 + 8)?; // reversed, continuity, monotonicity, tolerances
    let mut children = Children::of(data, b.rest());
    let pcurve = legacy_curve(data, children.expect(CURVE)?.body)?;
    let proxy = pcurve.domain();
    // A trim whose 3D image is one point (a pole) is singular. Others get an edge even when
    // the file stores its 3D curve with the other trim of a shared edge (as for seams). The
    // edge follows the surface: a trim straight in parameter space (a seam) can be a curve in
    // 3D, so pieces are halved until their chords lie close to the surface.
    let at = |q: f64| {
        let uv = pcurve.point_at(q);
        surface.point_at(uv[0], uv[1])
    };
    let size = surface_size(surface);
    let start = pcurve.sample_parameters(proxy[0], proxy[1]);
    let mut params = vec![start[0]];
    for w in start.windows(2) {
        refine(
            &at,
            w[0],
            w[1],
            at(w[0]),
            at(w[1]),
            EDGE_TOLERANCE * size,
            0,
            &mut params,
        );
    }
    let points: Vec<[f64; 3]> = params.iter().map(|&q| at(q)).collect();
    let spread = points
        .iter()
        .map(|p| distance(*p, points[0]))
        .fold(0.0, f64::max);
    let edge = if params.len() >= 2 && spread > SINGULAR * surface_size(surface) {
        t.curves.push(Some(Curve::Polyline { points, params }));
        t.edges.push(Edge {
            curve: t.curves.len() as i32 - 1,
            reversed: false,
            proxy,
            tolerance: 0.0,
        });
        t.edges.len() as i32 - 1
    } else {
        -1
    };
    t.curves_2d.push(Some(pcurve));
    t.trims.push(Trim {
        curve_2d: t.curves_2d.len() as i32 - 1,
        proxy,
        proxy_reversed: false,
        edge,
        vertices: [-1, -1],
        reversed: false,
    });
    Ok(t.trims.len() as i32 - 1)
}

/// A trim spanning less than this share of its surface's size in 3D is a point.
const SINGULAR: f64 = 1e-9;
/// An edge piece's chord may stray this share of its surface's size from the surface.
const EDGE_TOLERANCE: f64 = 1e-4;
/// Halvings of one edge piece at most.
const MAX_HALVINGS: u32 = 10;

/// Appends the parameters after `a` up to `b` at which the edge `at` is sampled: `b`, and
/// before it the midpoints of pieces whose chords stray more than `tol` from the edge.
#[allow(clippy::too_many_arguments)]
fn refine(
    at: &impl Fn(f64) -> [f64; 3],
    a: f64,
    b: f64,
    pa: [f64; 3],
    pb: [f64; 3],
    tol: f64,
    depth: u32,
    out: &mut Vec<f64>,
) {
    let m = (a + b) / 2.0;
    let pm = at(m);
    let chord = [0, 1, 2].map(|k| (pa[k] + pb[k]) / 2.0);
    if depth < MAX_HALVINGS && distance(pm, chord) > tol {
        refine(at, a, m, pa, pm, tol, depth + 1, out);
        refine(at, m, b, pm, pb, tol, depth + 1, out);
    } else {
        out.push(b);
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// The diagonal of the surface's control points' box.
fn surface_size(surface: &Surface) -> f64 {
    let Surface::Nurbs { cvs, .. } = surface else {
        return 1.0;
    };
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for cv in cvs {
        for k in 0..3 {
            lo[k] = lo[k].min(cv[k] / cv[3]);
            hi[k] = hi[k].max(cv[k] / cv[3]);
        }
    }
    distance(lo, hi)
}

fn mesh(data: &[u8], body: &[u8], scale: f64, view: &mut ObjectView) -> Result<()> {
    view.kind = Counted::Object(ObjectKind::Mesh);
    let geometry = Children::of(data, body).find(MESH_GEOMETRY)?;
    view.meshes = meshes(view, vec![compressed_mesh(geometry.body, scale)?]);
    Ok(())
}

/// A compressed mesh geometry chunk's body (meshes, and the render meshes of trimmed surfaces).
fn compressed_mesh(body: &[u8], scale: f64) -> Result<super::mesh::Mesh> {
    let mut b = Bytes::new(body);
    let bad = |what| ReadError::Invalid { at: 0, what };
    let points = usize::try_from(b.i32()?).map_err(|_| bad("V1 mesh point count"))?;
    let faces = usize::try_from(b.i32()?).map_err(|_| bad("V1 mesh face count"))?;
    if points == 0 || points > body.len() / 6 {
        return Err(bad("V1 mesh point count"));
    }
    b.i32()?; // normals
    b.i32()?; // texture points
    let min = [b.f64()?, b.f64()?, b.f64()?];
    let max = [b.f64()?, b.f64()?, b.f64()?];
    let step = [0, 1, 2].map(|k| (max[k] - min[k]) / 65535.0);
    let mut positions = Vec::with_capacity(points);
    for _ in 0..points {
        let q = [b.u16()?, b.u16()?, b.u16()?];
        positions.push([0, 1, 2].map(|k| (min[k] + step[k] * f64::from(q[k])) * scale));
    }
    let width = if points < 65535 { 2 } else { 4 };
    let start = b.at;
    b.skip(
        faces
            .checked_mul(4 * width)
            .ok_or(bad("V1 mesh face count"))?,
    )?;
    let (triangles, _) = super::mesh::triangulate(&body[start..b.at], width, &positions);
    Ok(super::mesh::Mesh {
        positions_mm: positions,
        triangles,
        dropped_faces: 0,
    })
}

fn meshes(view: &ObjectView, list: Vec<super::mesh::Mesh>) -> Vec<MeshGeom> {
    list.into_iter()
        .enumerate()
        .map(|(i, m)| MeshGeom {
            vertex_count: m.positions_mm.len(),
            triangle_count: m.triangles.len(),
            positions_mm: m.positions_mm,
            triangles: m.triangles,
            in_instance: false,
            source_visible: view.visible,
            source_layer: None,
            key: format!("{}.mesh{i}", view.fallback_key),
            color: None,
        })
        .collect()
}
