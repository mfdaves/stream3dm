//! Raw WebAssembly exports of `stream3dm`, for streaming a `.3dm` from JavaScript in slices
//! (`js/stream3dm.js` wraps them).
//!
//! Protocol: `stream3dm_new` returns a handle; copy each slice into memory from
//! `stream3dm_alloc` and call `stream3dm_push`; then `stream3dm_finish`. A finish result of 1
//! means the file has blocks: push the whole input again from the start and finish again. On 0
//! the report JSON (on -1 the error text) is at `stream3dm_result_ptr` / `stream3dm_result_len`.
//! Release with `stream3dm_drop`. After any push or finish, `stream3dm_needs_full` 1 means the
//! file has records only the full build (`--features full`, with cadmpeg) decodes: drop the
//! session and read the file again, from the start, with that build (`stream3dm_full` says which
//! build a module is).
//!
//! Sessions made with `stream3dm_new_with_meshes` also collect the visible meshes: after any
//! push (and after finishing), `stream3dm_chunk` describes the buffers collected since the last
//! `stream3dm_chunk_clear`: positions (f32 x, y, z, in metres from the origin
//! `stream3dm_offset` reports), colours (u8 r, g, b per vertex), triangle indices (u32, into
//! this chunk's vertices) and ranges (eight u32 per mesh: first vertex, vertex count, layer + 1,
//! material + 1, object id, pick, index count, line segment count), where layer and material
//! index the report's `structure.layers` and `materials` (0 = none), and line segments (f32 x,
//! y, z pairs from the same origin): the feature edges, where faces meet at more than 30° or a
//! mesh has a border. A mesh's indices and segments follow the previous mesh's in the chunk.
//! Curves and points use the same ranges: a curve is a range with segments only (no vertices,
//! no indices), consecutive sampled points joined; a set of points (a point object, a point
//! cloud) is a range with vertices only (no indices, no segments).
//!
//! A pick numbers the selectable unit a mesh belongs to, from 1: the object, or the top-level
//! block instance of a block member. After finishing, `stream3dm_picks_ptr` /
//! `stream3dm_picks_len` give a JSON array describing pick n at position n - 1 (`null` when
//! undescribed): `{"kind", "uuid", "name", "layer", "material", "block", "class", "groups",
//! "user_strings"}`, absent fields left out, layer and material as in the ranges' indices
//! without the + 1, groups as indices into the report's `structure.groups`.
//!
//! Sessions made with `stream3dm_new_with_meshes` or `stream3dm_new_fingerprint` also compute
//! the fingerprint (docs/fingerprint.md): `stream3dm_fingerprint_options` sets the key (32
//! bytes, or none for the public key) and clear names before finishing; after a successful
//! finish its canonical JSON is at `stream3dm_fingerprint_ptr` / `stream3dm_fingerprint_len`.
//!
//! Numbers: handles, pointers and lengths are 32-bit unsigned, but reach JavaScript as `i32`,
//! which reads values from 2 GiB up as negative: read them with `>>> 0`. `stream3dm_alloc`
//! aligns only to bytes; out-arrays (`stream3dm_chunk`'s ten u32, `stream3dm_reader_stats`'
//! three u32) need 4-byte alignment, `stream3dm_offset`'s three f64 and the f64 positions given
//! to `stream3dm_ifc_product` need 8-byte alignment.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use stream3dm::{
    DocumentInfo, FingerprintOptions, Fingerprinter, Finish, IfcInspector, InspectOptions,
    Inspector, MeshSink, Pick, PlacedMesh, PlacedWire, Tee,
};

/// What a session reads: a `.3dm` (pushed once, or twice for blocks), or an IFC file (pushed,
/// prepared, then given each product's mesh; `None` once finished).
enum Source {
    Rhino(Box<Inspector>),
    Ifc(Option<Box<IfcInspector>>),
}

pub struct Session {
    inspector: Source,
    result: Vec<u8>,
    /// The raw context (`stream3dm::context`) of the finished report, as JSON.
    context: Vec<u8>,
    meshes: Option<Rc<RefCell<Collector>>>,
    fingerprinter: Option<Rc<RefCell<Fingerprinter>>>,
    fingerprint_options: FingerprintOptions,
    /// The fingerprint JSON of the finished report (empty before, or without a fingerprinter).
    fingerprint: Vec<u8>,
    /// `[[object id as in the ranges (u32), key], …]` for the fingerprinted objects.
    object_keys: Vec<u8>,
    /// What each pick of the collected meshes is, as JSON (see the module notes).
    picks: Vec<u8>,
    /// The finished report, kept so the fingerprint can be recomputed with another key.
    report: Option<stream3dm::Report>,
}

impl Session {
    /// Computes the fingerprint and object keys from the kept report and accumulators.
    fn refingerprint(&mut self) {
        let (Some(f), Some(report)) = (&self.fingerprinter, &self.report) else {
            return;
        };
        let f = f.borrow();
        let fingerprint = f.finish(report, &self.fingerprint_options);
        self.fingerprint = serde_json::to_vec(&fingerprint).expect("fingerprint serializes");
        let keys: Vec<(u32, String)> = f
            .object_keys(&self.fingerprint_options)
            .into_iter()
            .map(|(id, key)| (id as u32, key))
            .collect();
        self.object_keys = serde_json::to_vec(&keys).expect("keys serialize");
    }

    fn new(inspector: Inspector) -> Self {
        Self::with_source(Source::Rhino(Box::new(inspector)))
    }

    fn with_source(inspector: Source) -> Self {
        Self {
            inspector,
            result: Vec::new(),
            context: Vec::new(),
            meshes: None,
            fingerprinter: None,
            fingerprint_options: FingerprintOptions::default(),
            fingerprint: Vec::new(),
            object_keys: Vec::new(),
            picks: Vec::new(),
            report: None,
        }
    }
}

/// Visible meshes as flat buffers ready for the GPU.
#[derive(Default)]
struct Collector {
    /// Subtracted from every position so f32 keeps millimetres on large coordinates.
    origin: Option<[f64; 3]>,
    layer_colors: Vec<Option<[u8; 3]>>,
    material_colors: Vec<Option<[u8; 3]>>,
    positions: Vec<f32>,
    colors: Vec<u8>,
    indices: Vec<u32>,
    ranges: Vec<u32>,
    lines: Vec<f32>,
    /// Line segments kept so far (all chunks), to stay within MAX_SEGMENTS.
    segments: usize,
    /// Curve segments kept so far (all chunks), to stay within MAX_CURVE_SEGMENTS.
    curve_segments: usize,
    /// Feature edges are skipped when false (`stream3dm_edges`).
    no_edges: bool,
    /// Pick id → its number in the ranges (from 1).
    pick_numbers: HashMap<u64, u32>,
    /// Descriptions by pick number - 1.
    picks: Vec<Option<serde_json::Value>>,
}

/// User strings kept per pick, and characters kept per key or value.
const MAX_PICK_USER_STRINGS: usize = 80;
const MAX_PICK_TEXT: usize = 200;

impl Collector {
    /// The colour of geometry on `layer` with `material`: the material's, else the layer's.
    fn color(
        &self,
        color: Option<[u8; 3]>,
        layer: Option<usize>,
        material: Option<usize>,
    ) -> [u8; 3] {
        color
            .or_else(|| material.and_then(|m| self.material_colors.get(m).copied().flatten()))
            .or_else(|| layer.and_then(|l| self.layer_colors.get(l).copied().flatten()))
            .unwrap_or(NO_COLOR)
    }

    /// Starts a range for `wire` with `vertices` positions and `segments` line segments.
    fn wire_range(&mut self, wire: &PlacedWire, vertices: usize, segments: usize) {
        let base = (self.positions.len() / 3) as u32;
        let pick = self.pick_number(wire.pick);
        self.ranges.extend([
            base,
            vertices as u32,
            wire.layer.map_or(0, |l| l as u32 + 1),
            wire.material.map_or(0, |m| m as u32 + 1),
            wire.object as u32,
            pick,
            0,
            segments as u32,
        ]);
    }

    fn pick_number(&mut self, pick: u64) -> u32 {
        let next = self.pick_numbers.len() as u32 + 1;
        let n = *self.pick_numbers.entry(pick).or_insert(next);
        if n == next {
            self.picks.push(None);
        }
        n
    }
}

/// Grey for meshes without a material or layer colour.
const NO_COLOR: [u8; 3] = [190, 190, 190];
/// Faces meeting at a sharper angle than this draw an edge (cos 30°).
const CREASE_COS: f64 = 0.866_025_4;
/// At most this many feature edge segments for a model (24 bytes each).
const MAX_SEGMENTS: usize = 6_000_000;
/// At most this many curve segments for a model (24 bytes each).
const MAX_CURVE_SEGMENTS: usize = 4_000_000;

/// Appends the feature edges of one mesh to `lines` (positions minus `origin`): borders,
/// non-manifold edges and creases sharper than 30°, with vertices welded at 0.1 mm (render
/// meshes often split vertices along creases and seams, some meshes store every face's
/// vertices apart). Vertices are welded by sorting their quantized positions; edges are
/// bucketed by their lower vertex.
fn feature_edges(
    mesh: &PlacedMesh,
    origin: [f64; 3],
    lines: &mut Vec<f32>,
    budget: usize,
) -> usize {
    let mut keyed: Vec<([i64; 3], u32)> = mesh
        .positions
        .iter()
        .enumerate()
        .map(|(i, p)| (p.map(|c| (c * 10_000.0).round() as i64), i as u32))
        .collect();
    keyed.sort_unstable();
    let mut ids = vec![0u32; mesh.positions.len()];
    let mut next = 0u32;
    for (k, (key, i)) in keyed.iter().enumerate() {
        if k > 0 && keyed[k - 1].0 != *key {
            next += 1;
        }
        ids[*i as usize] = next;
    }
    drop(keyed);
    // Face normals, and every edge as (lower welded vertex, higher, face, endpoints).
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(mesh.triangles.len());
    let mut edges: Vec<(u32, u32, u32, u32, u32)> = Vec::with_capacity(mesh.triangles.len() * 3);
    for t in mesh.triangles {
        let (Some(&a), Some(&b), Some(&c)) = (
            mesh.positions.get(t[0] as usize),
            mesh.positions.get(t[1] as usize),
            mesh.positions.get(t[2] as usize),
        ) else {
            continue;
        };
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len == 0.0 {
            continue;
        }
        let face = normals.len() as u32;
        normals.push(n.map(|x| (x / len) as f32));
        for (i, j) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            let (wi, wj) = (ids[i as usize], ids[j as usize]);
            if wi != wj {
                edges.push((wi.min(wj), wi.max(wj), face, i, j));
            }
        }
    }
    // Bucket the edges by their lower vertex (a counting sort: welded ids are dense), then
    // match the few edges of each bucket by their higher vertex.
    let vertices = next as usize + 1;
    let mut start = vec![0u32; vertices + 1];
    for e in &edges {
        start[e.0 as usize + 1] += 1;
    }
    for k in 0..vertices {
        start[k + 1] += start[k];
    }
    let mut fill = start.clone();
    let mut bucketed = vec![(0u32, 0u32, 0u32, 0u32); edges.len()];
    for &(lo, hi, face, i, j) in &edges {
        let slot = &mut fill[lo as usize];
        bucketed[*slot as usize] = (hi, face, i, j);
        *slot += 1;
    }
    drop(edges);
    let mut added = 0;
    for k in 0..vertices {
        if added >= budget {
            break;
        }
        let bucket = &mut bucketed[start[k] as usize..start[k + 1] as usize];
        bucket.sort_unstable_by_key(|e| e.0);
        let mut at = 0;
        while at < bucket.len() && added < budget {
            let (hi, face, i, j) = bucket[at];
            let n0 = normals[face as usize];
            let mut end = at + 1;
            let mut sharp = false;
            while end < bucket.len() && bucket[end].0 == hi {
                let n = normals[bucket[end].1 as usize];
                // Orientation may differ between neighbours: compare unsigned.
                let cos = f64::from(n0[0] * n[0] + n0[1] * n[1] + n0[2] * n[2]);
                if cos.abs() < CREASE_COS {
                    sharp = true;
                }
                end += 1;
            }
            let count = end - at;
            if count == 1 || count > 2 || sharp {
                for v in [i, j] {
                    let p = mesh.positions[v as usize];
                    lines.extend([
                        (p[0] - origin[0]) as f32,
                        (p[1] - origin[1]) as f32,
                        (p[2] - origin[2]) as f32,
                    ]);
                }
                added += 1;
            }
            at = end;
        }
    }
    added
}

impl MeshSink for Collector {
    fn document(&mut self, document: &DocumentInfo) {
        self.layer_colors = document.layers.iter().map(|l| l.color).collect();
        self.material_colors = document.materials.iter().map(|m| m.color).collect();
    }

    fn mesh(&mut self, mesh: &PlacedMesh) {
        let Some(&first) = mesh.positions.first() else {
            return;
        };
        let origin = *self.origin.get_or_insert(first.map(f64::round));
        let color = self.color(mesh.color, mesh.layer, mesh.material);
        let base = (self.positions.len() / 3) as u32;
        let pick = self.pick_number(mesh.pick);
        self.ranges.extend([
            base,
            mesh.positions.len() as u32,
            mesh.layer.map_or(0, |l| l as u32 + 1),
            mesh.material.map_or(0, |m| m as u32 + 1),
            mesh.object as u32,
            pick,
            mesh.triangles.len() as u32 * 3,
            0,
        ]);
        for p in mesh.positions {
            for k in 0..3 {
                self.positions.push((p[k] - origin[k]) as f32);
            }
            self.colors.extend_from_slice(&color);
        }
        for t in mesh.triangles {
            self.indices.extend(t.iter().map(|i| base + i));
        }
        let budget = MAX_SEGMENTS.saturating_sub(self.segments);
        if budget > 0 && !self.no_edges {
            let added = feature_edges(mesh, origin, &mut self.lines, budget);
            self.segments += added;
            *self.ranges.last_mut().expect("range just added") = added as u32;
        }
    }

    fn curve(&mut self, curve: &PlacedWire) {
        let Some(&first) = curve.positions.first() else {
            return;
        };
        let pieces = curve.positions.len() - 1;
        if pieces == 0 || self.curve_segments + pieces > MAX_CURVE_SEGMENTS {
            return;
        }
        let origin = *self.origin.get_or_insert(first.map(f64::round));
        self.wire_range(curve, 0, pieces);
        for pair in curve.positions.windows(2) {
            for p in pair {
                for k in 0..3 {
                    self.lines.push((p[k] - origin[k]) as f32);
                }
            }
        }
        self.curve_segments += pieces;
    }

    fn points(&mut self, points: &PlacedWire) {
        let Some(&first) = points.positions.first() else {
            return;
        };
        let origin = *self.origin.get_or_insert(first.map(f64::round));
        let color = self.color(None, points.layer, points.material);
        self.wire_range(points, points.positions.len(), 0);
        for p in points.positions {
            for k in 0..3 {
                self.positions.push((p[k] - origin[k]) as f32);
            }
            self.colors.extend_from_slice(&color);
        }
    }

    fn pick(&mut self, pick: &Pick) {
        let n = self.pick_number(pick.pick);
        let clip = |t: &str| t.chars().take(MAX_PICK_TEXT).collect::<String>();
        let mut entry = serde_json::Map::new();
        entry.insert("kind".into(), pick.kind.into());
        let mut text = |key: &str, value: Option<&str>| {
            if let Some(v) = value {
                entry.insert(key.into(), clip(v).into());
            }
        };
        text("uuid", pick.uuid);
        text("name", pick.name);
        text("block", pick.block);
        text("class", pick.class);
        if let Some(l) = pick.layer {
            entry.insert("layer".into(), l.into());
        }
        if let Some(m) = pick.material {
            entry.insert("material".into(), m.into());
        }
        if !pick.groups.is_empty() {
            entry.insert("groups".into(), pick.groups.to_vec().into());
        }
        if !pick.user_strings.is_empty() {
            let strings: Vec<serde_json::Value> = pick
                .user_strings
                .iter()
                .take(MAX_PICK_USER_STRINGS)
                .map(|(k, v)| serde_json::json!([clip(k), clip(v)]))
                .collect();
            entry.insert("user_strings".into(), strings.into());
        }
        self.picks[n as usize - 1] = Some(entry.into());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_alloc(len: usize) -> *mut u8 {
    let mut buffer = vec![0u8; len].into_boxed_slice();
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

/// # Safety
/// `ptr` and `len` must come from one `stream3dm_alloc` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_free(ptr: *mut u8, len: usize) {
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

/// `batch_bytes` 0 picks the automatic size; `cadmpeg_only` 1 decodes objects with cadmpeg.
#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_new(batch_bytes: usize, cadmpeg_only: u32) -> *mut Session {
    let options = InspectOptions {
        batch_bytes: (batch_bytes > 0).then_some(batch_bytes),
        cadmpeg_only: cadmpeg_only != 0,
        ..Default::default()
    };
    Box::into_raw(Box::new(Session::new(Inspector::new(options))))
}

/// Like `stream3dm_new`, also collecting the visible meshes (see the module notes).
#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_new_with_meshes(batch_bytes: usize) -> *mut Session {
    let options = InspectOptions {
        batch_bytes: (batch_bytes > 0).then_some(batch_bytes),
        ..Default::default()
    };
    let collector = Rc::new(RefCell::new(Collector::default()));
    let fingerprinter = Rc::new(RefCell::new(Fingerprinter::new()));
    let sink = Tee(collector.clone(), fingerprinter.clone());
    let mut session = Session::new(Inspector::with_sink(options, Box::new(sink)));
    session.meshes = Some(collector);
    session.fingerprinter = Some(fingerprinter);
    Box::into_raw(Box::new(session))
}

/// An IFC session collecting meshes and the fingerprint, like `stream3dm_new_with_meshes`: push the
/// file's bytes (`stream3dm_push`), call `stream3dm_ifc_prepare`, add each product's mesh from the geometry
/// engine (`stream3dm_ifc_product`), drain the chunks as usual, then `stream3dm_finish` (0 or -1).
#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_ifc_new_with_meshes() -> *mut Session {
    let collector = Rc::new(RefCell::new(Collector::default()));
    let fingerprinter = Rc::new(RefCell::new(Fingerprinter::new()));
    let sink = Tee(collector.clone(), fingerprinter.clone());
    let mut session = Session::with_source(Source::Ifc(Some(Box::new(IfcInspector::with_sink(
        Box::new(sink),
    )))));
    session.meshes = Some(collector);
    session.fingerprinter = Some(fingerprinter);
    Box::into_raw(Box::new(session))
}

/// Resolves the IFC file's storeys, types, materials and property sets once all its bytes are
/// pushed. Returns the number of products with a representation, or -1 (error text at
/// `stream3dm_result_ptr`).
///
/// # Safety
/// `session` from `stream3dm_ifc_new_with_meshes`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_ifc_prepare(session: *mut Session) -> i32 {
    let s = unsafe { &mut *session };
    let Source::Ifc(Some(i)) = &mut s.inspector else {
        s.result = b"not an IFC session".to_vec();
        return -1;
    };
    match i.prepare() {
        Ok(()) => i.product_count().min(i32::MAX as usize) as i32,
        Err(e) => {
            s.result = e.to_string().into_bytes();
            -1
        }
    }
}

/// Adds IFC product `id` (its STEP id) from the geometry engine: `positions` f64 x, y, z in
/// metres, z up, world coordinates; `indices` u32, three per triangle; `parts` five u32 per
/// part of the product (first vertex, vertex count, first index, index count, colour as
/// 0xRRGGBB or any value above 0xFFFFFF for none), each part's indices relative to its first
/// vertex. Returns 0 or -1.
///
/// # Safety
/// `session` from `stream3dm_ifc_new_with_meshes`; the pointers describe readable arrays.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn stream3dm_ifc_product(
    session: *mut Session,
    id: u32,
    positions: *const f64,
    positions_len: usize,
    indices: *const u32,
    indices_len: usize,
    parts: *const u32,
    parts_count: usize,
) -> i32 {
    let s = unsafe { &mut *session };
    let Source::Ifc(Some(i)) = &mut s.inspector else {
        return -1;
    };
    let positions = unsafe { std::slice::from_raw_parts(positions, positions_len) };
    let indices = unsafe { std::slice::from_raw_parts(indices, indices_len) };
    let Some(table_len) = parts_count.checked_mul(5) else {
        s.result = b"part count out of range".to_vec();
        return -1;
    };
    let table = unsafe { std::slice::from_raw_parts(parts, table_len) };
    let mut list = Vec::with_capacity(parts_count);
    for p in table.chunks_exact(5) {
        let (v0, vn, i0, i_n, rgb) = (
            p[0] as usize,
            p[1] as usize,
            p[2] as usize,
            p[3] as usize,
            p[4],
        );
        let range = |start: usize, len: usize, k: usize| {
            Some(start.checked_mul(k)?..start.checked_add(len)?.checked_mul(k)?)
        };
        let (Some(pos), Some(idx)) = (
            range(v0, vn, 3).and_then(|r| positions.get(r)),
            range(i0, i_n, 1).and_then(|r| indices.get(r)),
        ) else {
            s.result = b"part out of range".to_vec();
            return -1;
        };
        list.push(stream3dm::ProductPart {
            positions: pos,
            indices: idx,
            color: (rgb <= 0xff_ffff).then_some([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]),
        });
    }
    match i.add_product(u64::from(id), &list) {
        Ok(()) => 0,
        Err(e) => {
            s.result = e.to_string().into_bytes();
            -1
        }
    }
}

/// Turns feature edges on (1, the default) or off (0) for the meshes collected from now on.
///
/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_edges(session: *mut Session, on: u32) {
    if let Some(meshes) = unsafe { &(*session).meshes } {
        meshes.borrow_mut().no_edges = on == 0;
    }
}

/// Like `stream3dm_new`, also computing the fingerprint (no meshes collected).
#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_new_fingerprint(batch_bytes: usize) -> *mut Session {
    let options = InspectOptions {
        batch_bytes: (batch_bytes > 0).then_some(batch_bytes),
        ..Default::default()
    };
    let fingerprinter = Rc::new(RefCell::new(Fingerprinter::new()));
    let mut session = Session::new(Inspector::with_sink(
        options,
        Box::new(fingerprinter.clone()),
    ));
    session.fingerprinter = Some(fingerprinter);
    Box::into_raw(Box::new(session))
}

/// Sets the fingerprint's key (32 bytes at `key`, or null for the public key) and whether
/// names are kept in clear (1) or left out (0). Call before `stream3dm_finish`.
///
/// # Safety
/// `session` from `stream3dm_new*`; `key` null or pointing to 32 readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_fingerprint_options(
    session: *mut Session,
    key: *const u8,
    clear_names: u32,
) {
    let s = unsafe { &mut *session };
    s.fingerprint_options = FingerprintOptions {
        key: (!key.is_null()).then(|| {
            let mut k = [0u8; 32];
            k.copy_from_slice(unsafe { std::slice::from_raw_parts(key, 32) });
            k
        }),
        clear_names: clear_names != 0,
    };
}

/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_fingerprint_ptr(session: *const Session) -> *const u8 {
    unsafe { (*session).fingerprint.as_ptr() }
}

/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_fingerprint_len(session: *const Session) -> usize {
    unsafe { (*session).fingerprint.len() }
}

/// Recomputes the fingerprint and object keys of a finished session with new options (as
/// `stream3dm_fingerprint_options`), without reading the file again. Returns 0, or -1 when the
/// session has no finished fingerprint.
///
/// # Safety
/// `session` from `stream3dm_new_with_meshes` or `stream3dm_new_fingerprint`; `key` null or 32 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_fingerprint_rekey(
    session: *mut Session,
    key: *const u8,
    clear_names: u32,
) -> i32 {
    unsafe { stream3dm_fingerprint_options(session, key, clear_names) };
    let s = unsafe { &mut *session };
    if s.report.is_none() {
        return -1;
    }
    s.refingerprint();
    0
}

/// The fingerprinted objects' ids (as in the mesh ranges) and keys, as JSON
/// `[[id, "key"], …]`, after a successful finish.
///
/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_object_keys_ptr(session: *const Session) -> *const u8 {
    unsafe { (*session).object_keys.as_ptr() }
}

/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_object_keys_len(session: *const Session) -> usize {
    unsafe { (*session).object_keys.len() }
}

/// A result text owned by WebAssembly memory: read with `stream3dm_text_ptr` / `stream3dm_text_len`,
/// release with `stream3dm_text_drop`.
pub struct Text(Vec<u8>);

/// Compares two fingerprints (JSON, `before` then `after`, see docs/fingerprint.md). Returns a
/// text: the diff JSON, or `{"error": …}` when they cannot be compared.
///
/// # Safety
/// The pointers must describe readable byte ranges.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_diff(
    before: *const u8,
    before_len: usize,
    after: *const u8,
    after_len: usize,
) -> *mut Text {
    let parse = |ptr: *const u8, len: usize| -> Result<stream3dm::Fingerprint, String> {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        serde_json::from_slice(bytes).map_err(|e| format!("not a fingerprint: {e}"))
    };
    let result = parse(before, before_len)
        .and_then(|a| Ok((a, parse(after, after_len)?)))
        .and_then(|(a, b)| stream3dm::fingerprint::diff(&a, &b));
    let json = match result {
        Ok(d) => serde_json::to_vec(&d).expect("diff serializes"),
        Err(e) => serde_json::to_vec(&serde_json::json!({ "error": e })).expect("error serializes"),
    };
    Box::into_raw(Box::new(Text(json)))
}

/// # Safety
/// `text` from `stream3dm_diff`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_text_ptr(text: *const Text) -> *const u8 {
    unsafe { (*text).0.as_ptr() }
}

/// # Safety
/// `text` from `stream3dm_diff`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_text_len(text: *const Text) -> usize {
    unsafe { (*text).0.len() }
}

/// # Safety
/// `text` from `stream3dm_diff`, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_text_drop(text: *mut Text) {
    drop(unsafe { Box::from_raw(text) });
}

/// Writes ten `u32`s to `out`: pointer and length of the positions (f32), colours (u8),
/// indices (u32), ranges (u32) and line segments (f32) collected since the last clear. Zeros
/// when the session collects no meshes.
///
/// # Safety
/// `session` from `stream3dm_new*`; `out` must point to ten writable `u32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_chunk(session: *const Session, out: *mut u32) {
    let out = unsafe { std::slice::from_raw_parts_mut(out, 10) };
    out.fill(0);
    if let Some(meshes) = unsafe { &(*session).meshes } {
        let m = meshes.borrow();
        out.copy_from_slice(&[
            m.positions.as_ptr() as u32,
            m.positions.len() as u32,
            m.colors.as_ptr() as u32,
            m.colors.len() as u32,
            m.indices.as_ptr() as u32,
            m.indices.len() as u32,
            m.ranges.as_ptr() as u32,
            m.ranges.len() as u32,
            m.lines.as_ptr() as u32,
            m.lines.len() as u32,
        ]);
    }
}

/// Empties the collected buffers (after JavaScript copied them).
///
/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_chunk_clear(session: *mut Session) {
    if let Some(meshes) = unsafe { &(*session).meshes } {
        let mut m = meshes.borrow_mut();
        m.positions.clear();
        m.colors.clear();
        m.indices.clear();
        m.ranges.clear();
        m.lines.clear();
    }
}

/// Writes the origin (metres) subtracted from collected positions as three `f64`s; zeros
/// before the first mesh.
///
/// # Safety
/// `session` from `stream3dm_new*`; `out` must point to three writable `f64`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_offset(session: *const Session, out: *mut f64) {
    let out = unsafe { std::slice::from_raw_parts_mut(out, 3) };
    let origin = unsafe { &(*session).meshes }
        .as_ref()
        .and_then(|m| m.borrow().origin)
        .unwrap_or([0.0; 3]);
    out.copy_from_slice(&origin);
}

/// # Safety
/// `session` from a `stream3dm_new*` function; `ptr`/`len` a readable range of linear memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_push(session: *mut Session, ptr: *const u8, len: usize) -> i32 {
    let s = unsafe { &mut *session };
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let pushed = match &mut s.inspector {
        Source::Rhino(i) => i.push(data),
        Source::Ifc(Some(i)) => i.push(data),
        Source::Ifc(None) => Ok(()),
    };
    match pushed {
        Ok(()) => 0,
        Err(e) => {
            s.result = e.to_string().into_bytes();
            -1
        }
    }
}

/// # Safety
/// `session` from a `stream3dm_new*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_finish(session: *mut Session) -> i32 {
    let s = unsafe { &mut *session };
    let finished = match &mut s.inspector {
        Source::Rhino(i) => i.finish(),
        Source::Ifc(i) => match i.take() {
            Some(i) => i.finish().map(|r| Finish::Report(Box::new(r))),
            None => Err(stream3dm::InspectError::Decode("already finished".into())),
        },
    };
    match finished {
        Ok(Finish::Report(report)) => {
            s.result = serde_json::to_vec(&report).expect("report serializes");
            if let Some(meshes) = &s.meshes {
                s.picks = serde_json::to_vec(&meshes.borrow().picks).expect("picks serialize");
            }
            s.context =
                serde_json::to_vec(&stream3dm::context(&report)).expect("context serializes");
            if s.fingerprinter.is_some() {
                s.report = Some(*report);
                s.refingerprint();
            }
            0
        }
        Ok(Finish::Again) => 1,
        Err(e) => {
            s.result = e.to_string().into_bytes();
            -1
        }
    }
}

/// # Safety
/// `session` from a `stream3dm_new*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_result_ptr(session: *const Session) -> *const u8 {
    unsafe { (*session).result.as_ptr() }
}

/// # Safety
/// `session` from a `stream3dm_new*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_result_len(session: *const Session) -> usize {
    unsafe { (*session).result.len() }
}

/// The raw context JSON after a successful finish (empty before).
///
/// # Safety
/// `session` from a `stream3dm_new*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_context_ptr(session: *const Session) -> *const u8 {
    unsafe { (*session).context.as_ptr() }
}

/// # Safety
/// `session` from a `stream3dm_new*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_context_len(session: *const Session) -> usize {
    unsafe { (*session).context.len() }
}

/// What each pick in the ranges is, as JSON (see the module notes), after a successful finish
/// of a session collecting meshes (empty otherwise).
///
/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_picks_ptr(session: *const Session) -> *const u8 {
    unsafe { (*session).picks.as_ptr() }
}

/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_picks_len(session: *const Session) -> usize {
    unsafe { (*session).picks.len() }
}

/// Records decoded by our reader, delegated to cadmpeg, and failed in ours (last pass).
///
/// # Safety
/// `session` from a `stream3dm_new*` function; `out` must point to three writable `u32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_reader_stats(session: *const Session, out: *mut u32) {
    let out = unsafe { std::slice::from_raw_parts_mut(out, 3) };
    out.fill(0);
    if let Source::Rhino(i) = unsafe { &(*session).inspector } {
        let r = i.reader_stats();
        out.copy_from_slice(&[r.own as u32, r.delegated as u32, r.failed as u32]);
    }
}

/// 1 when the input so far needs the full build (`--features full`): records only cadmpeg may
/// decode (rare record versions). Always 0 in the full build.
///
/// # Safety
/// `session` from `stream3dm_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_needs_full(session: *const Session) -> u32 {
    match unsafe { &(*session).inspector } {
        Source::Rhino(i) => u32::from(i.needs_full_reader()),
        Source::Ifc(_) => 0,
    }
}

/// 1 in the full build (with cadmpeg), 0 in the small one.
#[unsafe(no_mangle)]
pub extern "C" fn stream3dm_full() -> u32 {
    u32::from(cfg!(feature = "full"))
}

/// # Safety
/// `session` from a `stream3dm_new*` function, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stream3dm_drop(session: *mut Session) {
    drop(unsafe { Box::from_raw(session) });
}
