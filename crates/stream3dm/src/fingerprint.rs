//! The stream3dm fingerprint (version 2): identifies a version of a model and lets two versions be
//! compared, without containing the model. Specified in `docs/fingerprint.md`; this module
//! follows it rule for rule.
//!
//! [`Fingerprinter`] is a [`MeshSink`]: it hashes the visible geometry as the file streams,
//! keeping a few dozen bytes per object, and [`Fingerprinter::finish`] combines it with the
//! report's tables. [`diff`] compares two fingerprints made with the same key.

use crate::{DocumentInfo, MeshSink, PlacedMesh, Projection, Report, ViewKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Version of the fingerprint rules this module follows; [`diff`] refuses other versions.
pub const FINGERPRINT_VERSION: u32 = 2;
const FORMAT: &str = "stream3dm-fingerprint";
/// The public key: for tests and public files only (see the spec's threat model).
pub const PUBLIC_KEY: [u8; 32] = [0; 32];
/// Grids for object centres, `(size, offset)` in millimetres.
const GRIDS: [(i64, i64); 4] = [(100, 0), (100, 50), (1000, 0), (1000, 500)];

/// How [`Fingerprinter::finish`] hashes: with which key, and whether names stay in clear.
/// Its `Debug` output leaves the key out.
#[derive(Clone, Default)]
pub struct FingerprintOptions {
    /// The owner's 32-byte key; `None` uses [`PUBLIC_KEY`].
    pub key: Option<[u8; 32]>,
    /// Include layer, material, block and view names in clear.
    pub clear_names: bool,
}

impl std::fmt::Debug for FingerprintOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FingerprintOptions")
            .field("key", &self.key.map(|_| "<redacted>"))
            .field("clear_names", &self.clear_names)
            .finish()
    }
}

/// A fingerprint as the spec's *Format* lays it out; serializes to its JSON. Hashes are
/// lowercase hexadecimal; table entries are sorted by key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// `stream3dm-fingerprint`.
    pub format: String,
    /// [`FINGERPRINT_VERSION`].
    pub version: u32,
    /// Made with an owner's key; `false` with [`PUBLIC_KEY`].
    pub keyed: bool,
    /// `h64("key-id", "")`: equal for two fingerprints made with the same key, without
    /// revealing it.
    pub key_id: String,
    /// `clear` when table entries carry their names, `hashed` otherwise.
    pub names: String,
    /// The part in clear: units, version, overall size, area and counts.
    pub summary: Summary,
    /// Digests of the geometry, the objects, the tables and the whole document.
    pub digests: Digests,
    /// One entry per layer.
    pub layers: Vec<LayerEntry>,
    /// One entry per material.
    pub materials: Vec<MaterialEntry>,
    /// One entry per block definition.
    pub blocks: Vec<BlockEntry>,
    /// One entry per saved viewport and named view.
    pub views: Vec<ViewEntry>,
    /// Names of the columns of `objects`: `key`, `layer`, `geometry`, `shape`, `cells`,
    /// `area_bits`.
    pub object_fields: Vec<String>,
    /// `[key, layer index, geometry, shape, cells, area bits]`, sorted by key.
    pub objects: Vec<(String, i64, String, String, String, u32)>,
}

/// The clear part of a fingerprint: nothing that reveals the layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// The report's unit system name.
    pub unit_system: String,
    /// The report's archive version (`None` when unknown).
    pub archive_version: Option<u32>,
    /// Size of the fingerprinted objects' overall bounding box in whole metres (rounded half
    /// up); zero without objects.
    pub size_m: [i64; 3],
    /// Total area of the fingerprinted triangles in square metres, rounded to two significant
    /// digits.
    pub area_m2: u64,
    /// Triangles fingerprinted (those with three distinct vertices at 1 mm).
    pub triangles: u64,
    /// Objects and table entries counted.
    pub counts: Counts,
}

/// Counts in a [`Summary`]: by kind from the report's [`Inventory`](crate::Inventory) (hidden
/// objects included), the rest from the fingerprint's own tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Model-space objects of every kind.
    pub objects: usize,
    /// Rows of [`Fingerprint::objects`]: objects and block-placement members with visible
    /// triangles.
    pub fingerprinted: usize,
    /// Layer entries.
    pub layers: usize,
    /// Material entries.
    pub materials: usize,
    /// Block entries.
    pub blocks: usize,
    /// Block placements, summed over the definitions.
    pub placements: usize,
    /// View entries.
    pub views: usize,
    /// Breps, extrusions and surfaces.
    pub breps: usize,
    /// Mesh objects.
    pub meshes: usize,
    /// SubD objects.
    pub subds: usize,
    /// Curve objects.
    pub curves: usize,
    /// Point objects and point clouds.
    pub points: usize,
    /// Text, leaders, dimensions and text dots.
    pub annotations: usize,
    /// Hatches.
    pub hatches: usize,
    /// Lights of the light table.
    pub lights: usize,
}

/// The fingerprint's digests (`h256`, 64 hexadecimal digits each).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digests {
    /// Over the three digests below and the summary: equal exactly when two fingerprints are.
    pub document: String,
    /// Over every object's geometry, sorted: the model's shape alone, whatever the
    /// identities and names.
    pub geometry: String,
    /// Over every object's key, geometry and layer.
    pub objects: String,
    /// Over the layer, material, block and view entries.
    pub structure: String,
}

/// A layer in a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerEntry {
    /// `h64("layer", path)`.
    pub key: String,
    /// The layer's path, with [`FingerprintOptions::clear_names`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hash of its colour, visibility, lock and material.
    pub props: String,
    /// Fingerprinted objects on it.
    pub objects: usize,
    /// Hash of the key and geometry of its objects.
    pub content: String,
}

/// A material in a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialEntry {
    /// `h64("material", name)`.
    pub key: String,
    /// The material's name, with [`FingerprintOptions::clear_names`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hash of its colour, PBR parameters and texture file names.
    pub params: String,
}

/// A block definition in a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEntry {
    /// `h64("block", name)`.
    pub key: String,
    /// The definition's name, with [`FingerprintOptions::clear_names`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// As in [`Block::placements`](crate::Block::placements).
    pub placements: usize,
    /// Objects the definition lists.
    pub members: usize,
    /// Hash of its member count and size to the millimetre.
    pub shape: String,
}

/// A saved viewport or named view in a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewEntry {
    /// `h64("view", text(kind, name))`.
    pub key: String,
    /// The view's name, with [`FingerprintOptions::clear_names`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hash of its projection, camera, direction, up, target, frustum and clipping planes.
    pub camera: String,
}

/// Per-object accumulation while the geometry streams.
#[derive(Debug, Clone, Copy)]
struct ObjectAcc {
    /// Position lanes (geometry).
    s0: u64,
    s1: u64,
    /// Edge-vector lanes (shape: invariant to translation).
    r0: u64,
    r1: u64,
    n: u64,
    min: [i64; 3],
    max: [i64; 3],
    a2: u128,
    /// Vertex sums and second moments (xx, yy, zz, xy, xz, yz) over the triangles' corners,
    /// for the translation-invariant central moments of the shape. Wrapping 64-bit: the
    /// moments' algebra holds exactly modulo 2^64, so invariance does too.
    sum: [i64; 3],
    moments: [i64; 6],
    layer: Option<usize>,
}

/// Hashes the visible geometry as it streams (see the module docs).
#[derive(Debug, Default)]
pub struct Fingerprinter {
    layer_paths: Vec<String>,
    objects: HashMap<u64, ObjectAcc>,
}

impl MeshSink for Fingerprinter {
    fn document(&mut self, document: &DocumentInfo) {
        self.layer_paths = document.layers.iter().map(|l| l.path.clone()).collect();
    }

    fn mesh(&mut self, mesh: &PlacedMesh) {
        let acc = self.objects.entry(mesh.object).or_insert(ObjectAcc {
            s0: 0,
            s1: 0,
            r0: 0,
            r1: 0,
            n: 0,
            min: [i64::MAX; 3],
            max: [i64::MIN; 3],
            a2: 0,
            sum: [0; 3],
            moments: [0; 6],
            layer: None,
        });
        // Each vertex quantized once, not once per triangle using it.
        let quantized: Vec<[i64; 3]> = mesh.positions.iter().map(|p| p.map(quantize)).collect();
        for t in mesh.triangles {
            let (Some(&a), Some(&b), Some(&c)) = (
                quantized.get(t[0] as usize),
                quantized.get(t[1] as usize),
                quantized.get(t[2] as usize),
            ) else {
                continue;
            };
            if a == b || b == c || a == c {
                continue;
            }
            if acc.n == 0 {
                acc.layer = mesh.layer;
            }
            let ((t0, t1), (e0, e1)) = triangle_hashes(a, b, c);
            acc.s0 = acc.s0.wrapping_add(t0);
            acc.s1 = acc.s1.wrapping_add(t1);
            acc.r0 = acc.r0.wrapping_add(e0);
            acc.r1 = acc.r1.wrapping_add(e1);
            acc.n += 1;
            for p in [a, b, c] {
                for (k, &v) in p.iter().enumerate() {
                    acc.min[k] = acc.min[k].min(v);
                    acc.max[k] = acc.max[k].max(v);
                }
                let [x, y, z] = p;
                acc.sum[0] = acc.sum[0].wrapping_add(x);
                acc.sum[1] = acc.sum[1].wrapping_add(y);
                acc.sum[2] = acc.sum[2].wrapping_add(z);
                let products = [
                    x.wrapping_mul(x),
                    y.wrapping_mul(y),
                    z.wrapping_mul(z),
                    x.wrapping_mul(y),
                    x.wrapping_mul(z),
                    y.wrapping_mul(z),
                ];
                for (m, v) in acc.moments.iter_mut().zip(products) {
                    *m = m.wrapping_add(v);
                }
            }
            acc.a2 += twice_area(a, b, c);
        }
    }
}

/// Metres to millimetres, rounded half away from zero.
fn quantize(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

/// Unit vectors and ratios to 1e-4, rounded half away from zero.
fn quantize_n(v: f64) -> i64 {
    (v * 10000.0).round() as i64
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The lanes of a quantized triangle rotated to start at its smallest vertex: two over its
/// nine coordinates (position) and two over its two edge vectors from that vertex (shape).
fn triangle_hashes(a: [i64; 3], b: [i64; 3], c: [i64; 3]) -> ((u64, u64), (u64, u64)) {
    let [p, q, r] = if a <= b && a <= c {
        [a, b, c]
    } else if b <= a && b <= c {
        [b, c, a]
    } else {
        [c, a, b]
    };
    let lane = |seed: u64, values: &[i64]| {
        let mut h = seed;
        for v in values {
            h = mix(h ^ (*v as u64));
        }
        h
    };
    let position = [p[0], p[1], p[2], q[0], q[1], q[2], r[0], r[1], r[2]];
    let edges = [
        q[0] - p[0],
        q[1] - p[1],
        q[2] - p[2],
        r[0] - p[0],
        r[1] - p[1],
        r[2] - p[2],
    ];
    (
        (
            lane(0x0123_4567_89AB_CDEF, &position),
            lane(0xFEDC_BA98_7654_3210, &position),
        ),
        (
            lane(0x5555_5555_5555_5555, &edges),
            lane(0xAAAA_AAAA_AAAA_AAAA, &edges),
        ),
    )
}

/// Twice the triangle's area in mm², rounded: the cross product's components (exact in
/// double precision for edges under about 90 km) squared and summed in double precision in
/// x, y, z order, square-rooted.
fn twice_area(a: [i64; 3], b: [i64; 3], c: [i64; 3]) -> u128 {
    let u = [0, 1, 2].map(|k| (b[k] - a[k]) as f64);
    let v = [0, 1, 2].map(|k| (c[k] - a[k]) as f64);
    let cross = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let squared = cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2];
    squared.sqrt().round() as u128
}

/// HMAC-SHA-256 over the concatenation of `parts` (a key of at most 64 bytes).
fn hmac_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for part in parts {
        inner.update(part);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// `H(tag, data)`: HMAC-SHA-256 over the tag, a zero byte and the data.
fn hmac(key: &[u8; 32], tag: &str, data: &[u8]) -> [u8; 32] {
    hmac_parts(key, &[tag.as_bytes(), &[0], data])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `text(a, b, ...)`: fields joined by 0x1f.
fn text(fields: &[String]) -> Vec<u8> {
    fields.join("\u{1f}").into_bytes()
}

struct Hasher<'a> {
    key: &'a [u8; 32],
}

impl Hasher<'_> {
    fn h64(&self, tag: &str, data: &[u8]) -> String {
        hex(&hmac(self.key, tag, data)[..8])
    }
    fn h32(&self, tag: &str, data: &[u8]) -> String {
        hex(&hmac(self.key, tag, data)[..4])
    }
    fn h256(&self, tag: &str, data: &[u8]) -> String {
        hex(&hmac(self.key, tag, data))
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// The number of significant bits.
fn bit_length(v: u128) -> u32 {
    128 - v.leading_zeros()
}

/// Rounds to two significant digits.
fn two_digits(v: u64) -> u64 {
    if v < 100 {
        return v;
    }
    let mut p = 1u64;
    while v / p >= 100 {
        p *= 10;
    }
    (v + p / 2) / p * p
}

impl Fingerprinter {
    /// A fingerprinter with nothing streamed yet, to hand to an
    /// [`Inspector`](crate::Inspector) as its sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Each fingerprinted object's identity (as the meshes carried it) with its key, so a
    /// viewer can find the meshes of the objects a diff names.
    pub fn object_keys(&self, options: &FingerprintOptions) -> Vec<(u64, String)> {
        let key = options.key.unwrap_or(PUBLIC_KEY);
        let mut out: Vec<(u64, String)> = self
            .objects
            .iter()
            .filter(|(_, acc)| acc.n > 0)
            .map(|(&id, _)| (id, hex(&hmac(&key, "object", &id.to_le_bytes())[..8])))
            .collect();
        out.sort();
        out
    }

    /// The fingerprint of the streamed geometry and `report`'s tables.
    pub fn finish(&self, report: &Report, options: &FingerprintOptions) -> Fingerprint {
        let key = options.key.unwrap_or(PUBLIC_KEY);
        let h = Hasher { key: &key };
        let clear = options.clear_names;

        // Layers, sorted by key; objects refer to them by their sorted position.
        let layers_in = &report.structure.layers;
        let mut layer_keys: Vec<(String, usize)> = layers_in
            .iter()
            .enumerate()
            .map(|(i, l)| (h.h64("layer", l.path.as_bytes()), i))
            .collect();
        layer_keys.sort();
        let mut layer_position = vec![-1i64; layers_in.len()];
        for (position, (_, i)) in layer_keys.iter().enumerate() {
            layer_position[*i] = position as i64;
        }

        // Objects.
        let mut objects: Vec<(String, i64, String, String, String, u32)> = Vec::new();
        let mut geometry_digests: Vec<[u8; 8]> = Vec::new();
        let mut layer_members: Vec<Vec<[u8; 16]>> = vec![Vec::new(); layers_in.len()];
        let mut overall_min = [i64::MAX; 3];
        let mut overall_max = [i64::MIN; 3];
        let mut total_a2: u128 = 0;
        let mut triangles: u64 = 0;
        for (&identity, acc) in &self.objects {
            if acc.n == 0 {
                continue;
            }
            let key_bytes = hmac(&key, "object", &identity.to_le_bytes());
            let mut data = Vec::with_capacity(24);
            data.extend_from_slice(&acc.s0.to_le_bytes());
            data.extend_from_slice(&acc.s1.to_le_bytes());
            data.extend_from_slice(&acc.n.to_le_bytes());
            let geometry_bytes = hmac(&key, "geometry", &data);
            // Shape: edge-vector lanes, triangle count and the central second moments
            // m·Σvvᵀ − (Σv)(Σv)ᵀ over the 3n corners, all exact integers.
            let m = (acc.n as i64).wrapping_mul(3);
            let pairs = [(0, 0), (1, 1), (2, 2), (0, 1), (0, 2), (1, 2)];
            let mut shape_data = Vec::with_capacity(24 + 6 * 8);
            shape_data.extend_from_slice(&acc.r0.to_le_bytes());
            shape_data.extend_from_slice(&acc.r1.to_le_bytes());
            shape_data.extend_from_slice(&acc.n.to_le_bytes());
            for (k, (i, j)) in pairs.into_iter().enumerate() {
                let central = m
                    .wrapping_mul(acc.moments[k])
                    .wrapping_sub(acc.sum[i].wrapping_mul(acc.sum[j]));
                shape_data.extend_from_slice(&central.to_le_bytes());
            }
            let shape = h.h64("shape", &shape_data);
            let layer = acc
                .layer
                .and_then(|l| layer_position.get(l).copied())
                .unwrap_or(-1);
            let centre = [0, 1, 2].map(|k| (acc.min[k] + acc.max[k]).div_euclid(2));
            let cells: String = GRIDS
                .iter()
                .map(|&(size, offset)| {
                    let mut d = Vec::with_capacity(32);
                    d.extend_from_slice(&(size as u32).to_le_bytes());
                    d.extend_from_slice(&(offset as u32).to_le_bytes());
                    for c in centre {
                        d.extend_from_slice(&(c + offset).div_euclid(size).to_le_bytes());
                    }
                    h.h32("cell", &d)
                })
                .collect();
            let mut g8 = [0u8; 8];
            g8.copy_from_slice(&geometry_bytes[..8]);
            geometry_digests.push(g8);
            if let Some(l) = acc.layer.filter(|&l| l < layers_in.len()) {
                let mut pair = [0u8; 16];
                pair[..8].copy_from_slice(&key_bytes[..8]);
                pair[8..].copy_from_slice(&g8);
                layer_members[l].push(pair);
            }
            for k in 0..3 {
                overall_min[k] = overall_min[k].min(acc.min[k]);
                overall_max[k] = overall_max[k].max(acc.max[k]);
            }
            total_a2 += acc.a2;
            triangles += acc.n;
            objects.push((
                hex(&key_bytes[..8]),
                layer,
                hex(&g8),
                shape,
                cells,
                bit_length(acc.a2),
            ));
        }
        objects.sort();
        geometry_digests.sort();

        let layers: Vec<LayerEntry> = layer_keys
            .iter()
            .map(|(k, i)| {
                let l = &layers_in[*i];
                let mut members = layer_members[*i].clone();
                members.sort();
                LayerEntry {
                    key: k.clone(),
                    name: clear.then(|| l.path.clone()),
                    props: h.h64(
                        "layer-props",
                        &text(&[
                            opt(l.color.clone()).to_lowercase(),
                            l.visible.to_string(),
                            l.locked.to_string(),
                            opt(l.material.clone()),
                        ]),
                    ),
                    objects: members.len(),
                    content: h.h64("layer-content", &members.concat()),
                }
            })
            .collect();

        let mut materials: Vec<MaterialEntry> = report
            .materials
            .iter()
            .map(|m| {
                let mut textures = m.textures.clone();
                textures.sort();
                MaterialEntry {
                    key: h.h64("material", m.name.as_bytes()),
                    name: clear.then(|| m.name.clone()),
                    params: h.h64(
                        "material-params",
                        &text(&[
                            opt(m.color.clone()).to_lowercase(),
                            m.physically_based.to_string(),
                            opt(m.metallic.map(quantize_n)),
                            opt(m.roughness.map(quantize_n)),
                            opt(m.opacity.map(quantize_n)),
                            textures.join("\u{1e}"),
                        ]),
                    ),
                }
            })
            .collect();
        materials.sort_by(|a, b| a.key.cmp(&b.key));

        let mut blocks: Vec<BlockEntry> = report
            .blocks
            .iter()
            .map(|b| {
                let size = b.size.map(|s| s.map(quantize));
                BlockEntry {
                    key: h.h64("block", b.name.as_bytes()),
                    name: clear.then(|| b.name.clone()),
                    placements: b.placements,
                    members: b.members,
                    shape: h.h64(
                        "block-shape",
                        &text(&[
                            b.members.to_string(),
                            opt(size.map(|s| s[0])),
                            opt(size.map(|s| s[1])),
                            opt(size.map(|s| s[2])),
                        ]),
                    ),
                }
            })
            .collect();
        blocks.sort_by(|a, b| a.key.cmp(&b.key));

        let mut views: Vec<ViewEntry> = report
            .views
            .iter()
            .map(|v| {
                let kind = match v.kind {
                    ViewKind::Viewport => "viewport",
                    ViewKind::Named => "named",
                };
                let projection = match v.projection {
                    Projection::Parallel => "parallel",
                    Projection::Perspective => "perspective",
                    Projection::Other => "other",
                };
                let mut fields = vec![projection.to_string()];
                fields.extend(v.camera.map(|c| quantize(c).to_string()));
                fields.extend(v.direction.map(|c| quantize_n(c).to_string()));
                fields.extend(v.up.map(|c| quantize_n(c).to_string()));
                match v.target {
                    Some(t) => fields.extend(t.map(|c| quantize(c).to_string())),
                    None => fields.extend([String::new(), String::new(), String::new()]),
                }
                match v.frustum {
                    Some(f) => fields.extend(f.map(|c| quantize(c).to_string())),
                    None => fields.extend(std::iter::repeat_n(String::new(), 6)),
                }
                for p in &v.clipping_planes {
                    fields.extend([
                        quantize_n(p[0]).to_string(),
                        quantize_n(p[1]).to_string(),
                        quantize_n(p[2]).to_string(),
                        quantize(p[3]).to_string(),
                    ]);
                }
                ViewEntry {
                    key: h.h64("view", &text(&[kind.to_string(), v.name.clone()])),
                    name: clear.then(|| v.name.clone()),
                    camera: h.h64("view-camera", &text(&fields)),
                }
            })
            .collect();
        views.sort_by(|a, b| a.key.cmp(&b.key));

        let inventory = &report.inventory;
        let size_m = if objects.is_empty() {
            [0, 0, 0]
        } else {
            [0, 1, 2].map(|k| (overall_max[k] - overall_min[k] + 500).div_euclid(1000))
        };
        let summary = Summary {
            unit_system: report.file.unit_system.clone(),
            archive_version: report.file.archive_version,
            size_m,
            area_m2: two_digits(((total_a2 + 1_000_000) / 2_000_000) as u64),
            triangles,
            counts: Counts {
                objects: inventory.objects,
                fingerprinted: objects.len(),
                layers: layers.len(),
                materials: materials.len(),
                blocks: blocks.len(),
                placements: report.blocks.iter().map(|b| b.placements).sum(),
                views: views.len(),
                breps: inventory.breps,
                meshes: inventory.meshes,
                subds: inventory.subds,
                curves: inventory.curves,
                points: inventory.points,
                annotations: inventory.annotations,
                hatches: inventory.hatches,
                lights: inventory.lights,
            },
        };

        let geometry = h.h256("geometry-set", &geometry_digests.concat());
        let mut object_bytes = Vec::with_capacity(objects.len() * 24);
        for (k, layer, g, _, _, _) in &objects {
            object_bytes.extend_from_slice(k.as_bytes());
            object_bytes.extend_from_slice(g.as_bytes());
            object_bytes.extend_from_slice(&layer.to_le_bytes());
        }
        let objects_digest = h.h256("object-set", &object_bytes);
        let mut structure = Vec::new();
        for l in &layers {
            structure.extend_from_slice(l.key.as_bytes());
            structure.extend_from_slice(l.props.as_bytes());
            structure.extend_from_slice(&(l.objects as u64).to_le_bytes());
            structure.extend_from_slice(l.content.as_bytes());
        }
        for m in &materials {
            structure.extend_from_slice(m.key.as_bytes());
            structure.extend_from_slice(m.params.as_bytes());
        }
        for b in &blocks {
            structure.extend_from_slice(b.key.as_bytes());
            structure.extend_from_slice(&(b.placements as u64).to_le_bytes());
            structure.extend_from_slice(&(b.members as u64).to_le_bytes());
            structure.extend_from_slice(b.shape.as_bytes());
        }
        for v in &views {
            structure.extend_from_slice(v.key.as_bytes());
            structure.extend_from_slice(v.camera.as_bytes());
        }
        let structure_digest = h.h256("structure", &structure);
        let mut document = Vec::new();
        document.extend_from_slice(geometry.as_bytes());
        document.extend_from_slice(objects_digest.as_bytes());
        document.extend_from_slice(structure_digest.as_bytes());
        document.extend_from_slice(&serde_json::to_vec(&summary).expect("summary serializes"));
        let document_digest = h.h256("document", &document);

        Fingerprint {
            format: FORMAT.to_string(),
            version: FINGERPRINT_VERSION,
            keyed: key != PUBLIC_KEY,
            key_id: h.h64("key-id", b""),
            names: if clear { "clear" } else { "hashed" }.to_string(),
            summary,
            digests: Digests {
                document: document_digest,
                geometry,
                objects: objects_digest,
                structure: structure_digest,
            },
            layers,
            materials,
            blocks,
            views,
            object_fields: ["key", "layer", "geometry", "shape", "cells", "area_bits"]
                .map(String::from)
                .to_vec(),
            objects,
        }
    }
}

// ── Comparison ───────────────────────────────────────────────────────────────

/// What changed between two fingerprints (see the spec's *Comparing two fingerprints*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FingerprintDiff {
    /// Equal `document` digests.
    pub same: bool,
    /// Equal `geometry` digests.
    pub same_geometry: bool,
    /// Summary fields that differ (`unit_system`, `archive_version`, `size_m`, …, `counts.x`).
    pub summary: Vec<String>,
    /// Objects matched by key.
    pub objects: ObjectChanges,
    /// Layers matched by key.
    pub layers: TableChanges,
    /// Materials matched by key.
    pub materials: TableChanges,
    /// Block definitions matched by key.
    pub blocks: TableChanges,
    /// Views matched by key.
    pub views: TableChanges,
}

/// How far an object's centre went, from the cells both versions share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Distance {
    /// One of the two 0.1 m cells is shared.
    Within0_1m,
    /// One of the two 1 m cells is shared.
    Within1m,
    /// No cell shared.
    Beyond1m,
}

/// An object present in both versions whose geometry changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectChange {
    /// The object's key, the same in both.
    pub key: String,
    /// Same shape: moved without changing form (a translation).
    pub moved: bool,
    /// How far its centre went.
    pub distance: Distance,
    /// Its layer changed too.
    pub layer_changed: bool,
    /// Its area changed by a factor of two or more.
    pub area_changed: bool,
}

/// An object that came back under a new identity (copied, pasted, re-imported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recreated {
    /// Its key in the first version.
    pub before: String,
    /// Its key in the second version.
    pub after: String,
    /// Same geometry (same shape in the same place); otherwise same shape elsewhere.
    pub in_place: bool,
}

/// How the objects of two fingerprints differ, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ObjectChanges {
    /// Keys only in the second version, re-created objects left out.
    pub added: Vec<String>,
    /// Keys only in the first version, re-created objects left out.
    pub removed: Vec<String>,
    /// Objects with the same geometry on the same layer.
    pub unchanged: usize,
    /// Same geometry, another layer.
    pub moved_to_layer: Vec<String>,
    /// Geometry changed (moved or reshaped).
    pub changed: Vec<ObjectChange>,
    /// Removed and added objects matched by geometry, then by shape.
    pub recreated: Vec<Recreated>,
}

/// How the entries of one table differ between two fingerprints, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TableChanges {
    /// Keys only in the second version.
    pub added: Vec<String>,
    /// Keys only in the first version.
    pub removed: Vec<String>,
    /// Keys in both whose other fields differ.
    pub changed: Vec<String>,
}

impl ObjectChanges {
    /// The changed objects that kept their shape (translated).
    pub fn moved(&self) -> impl Iterator<Item = &ObjectChange> {
        self.changed.iter().filter(|c| c.moved)
    }
    /// The changed objects whose shape changed.
    pub fn reshaped(&self) -> impl Iterator<Item = &ObjectChange> {
        self.changed.iter().filter(|c| !c.moved)
    }
}

impl FingerprintDiff {
    /// Number of object and table differences (summary fields and unchanged objects are not
    /// counted).
    pub fn len(&self) -> usize {
        let o = &self.objects;
        let t = |t: &TableChanges| t.added.len() + t.removed.len() + t.changed.len();
        o.added.len()
            + o.removed.len()
            + o.moved_to_layer.len()
            + o.changed.len()
            + o.recreated.len()
            + t(&self.layers)
            + t(&self.materials)
            + t(&self.blocks)
            + t(&self.views)
    }

    /// No object or table differences (see [`len`](Self::len)).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An object row: layer key, geometry, shape, cells, area bits.
struct Row {
    layer: Option<String>,
    geometry: String,
    shape: String,
    cells: String,
    area: u32,
}

/// Compares `a` (before) with `b` (after). Errors when they cannot be compared.
pub fn diff(a: &Fingerprint, b: &Fingerprint) -> Result<FingerprintDiff, String> {
    for f in [a, b] {
        if f.format != FORMAT || f.version != FINGERPRINT_VERSION {
            return Err(format!(
                "unsupported fingerprint {} version {}",
                f.format, f.version
            ));
        }
    }
    if a.key_id != b.key_id {
        return Err("the fingerprints were made with different keys".into());
    }
    let index = |f: &Fingerprint| -> BTreeMap<String, Row> {
        f.objects
            .iter()
            .map(|(k, l, g, shape, c, area)| {
                let layer = usize::try_from(*l)
                    .ok()
                    .and_then(|i| f.layers.get(i))
                    .map(|x| x.key.clone());
                (
                    k.clone(),
                    Row {
                        layer,
                        geometry: g.clone(),
                        shape: shape.clone(),
                        cells: c.clone(),
                        area: *area,
                    },
                )
            })
            .collect()
    };
    let (before, after) = (index(a), index(b));
    let cell = |c: &str, i: usize| c.get(i * 8..i * 8 + 8).map(str::to_string);
    let distance = |x: &Row, y: &Row| {
        let shares = |range: [usize; 2]| {
            range
                .iter()
                .any(|&i| cell(&x.cells, i) == cell(&y.cells, i))
        };
        if shares([0, 1]) {
            Distance::Within0_1m
        } else if shares([2, 3]) {
            Distance::Within1m
        } else {
            Distance::Beyond1m
        }
    };
    let mut objects = ObjectChanges::default();
    let mut removed = Vec::new();
    for (k, x) in &before {
        let Some(y) = after.get(k) else {
            removed.push(k.clone());
            continue;
        };
        if x.geometry == y.geometry {
            if x.layer == y.layer {
                objects.unchanged += 1;
            } else {
                objects.moved_to_layer.push(k.clone());
            }
            continue;
        }
        objects.changed.push(ObjectChange {
            key: k.clone(),
            moved: x.shape == y.shape,
            distance: distance(x, y),
            layer_changed: x.layer != y.layer,
            area_changed: x.area != y.area,
        });
    }
    let mut added: Vec<String> = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();

    // Objects back under a new identity: first same geometry, then same shape, pairing each
    // group in key order.
    for in_place in [true, false] {
        let group =
            |rows: &BTreeMap<String, Row>, keys: &[String]| -> BTreeMap<String, Vec<String>> {
                let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for k in keys {
                    let r = &rows[k];
                    out.entry(if in_place {
                        r.geometry.clone()
                    } else {
                        r.shape.clone()
                    })
                    .or_default()
                    .push(k.clone());
                }
                out
            };
        let (gone, new) = (group(&before, &removed), group(&after, &added));
        let mut matched_before = BTreeSet::new();
        let mut matched_after = BTreeSet::new();
        for (digest, olds) in &gone {
            let Some(news) = new.get(digest) else {
                continue;
            };
            for (o, n) in olds.iter().zip(news) {
                objects.recreated.push(Recreated {
                    before: o.clone(),
                    after: n.clone(),
                    in_place,
                });
                matched_before.insert(o.clone());
                matched_after.insert(n.clone());
            }
        }
        removed.retain(|k| !matched_before.contains(k));
        added.retain(|k| !matched_after.contains(k));
    }
    objects.removed = removed;
    objects.added = added;

    fn table<T>(a: &[T], b: &[T], key: impl Fn(&T) -> &str) -> TableChanges
    where
        T: PartialEq,
    {
        let before: BTreeMap<&str, &T> = a.iter().map(|x| (key(x), x)).collect();
        let after: BTreeMap<&str, &T> = b.iter().map(|x| (key(x), x)).collect();
        let keys: BTreeSet<&str> = before.keys().chain(after.keys()).copied().collect();
        let mut out = TableChanges::default();
        for k in keys {
            match (before.get(k), after.get(k)) {
                (Some(_), None) => out.removed.push(k.to_string()),
                (None, Some(_)) => out.added.push(k.to_string()),
                (Some(x), Some(y)) if x != y => out.changed.push(k.to_string()),
                _ => {}
            }
        }
        out
    }

    let mut summary = Vec::new();
    let (sa, sb) = (
        serde_json::to_value(&a.summary).expect("summary serializes"),
        serde_json::to_value(&b.summary).expect("summary serializes"),
    );
    if let (Some(ma), Some(mb)) = (sa.as_object(), sb.as_object()) {
        for (k, va) in ma {
            match (va.as_object(), mb.get(k).and_then(|v| v.as_object())) {
                (Some(ca), Some(cb)) => {
                    for (c, x) in ca {
                        if cb.get(c) != Some(x) {
                            summary.push(format!("{k}.{c}"));
                        }
                    }
                }
                _ if mb.get(k) != Some(va) => summary.push(k.clone()),
                _ => {}
            }
        }
    }

    Ok(FingerprintDiff {
        same: a.digests.document == b.digests.document,
        same_geometry: a.digests.geometry == b.digests.geometry,
        summary,
        objects,
        layers: table(&a.layers, &b.layers, |l| &l.key),
        materials: table(&a.materials, &b.materials, |m| &m.key),
        blocks: table(&a.blocks, &b.blocks, |b| &b.key),
        views: table(&a.views, &b.views, |v| &v.key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 1: 20-byte key of 0x0b, data "Hi There".
        assert_eq!(
            hex(&hmac_parts(&[0x0b; 20], &[b"Hi There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2: key "Jefe", data "what do ya want for nothing?", split in parts.
        assert_eq!(
            hex(&hmac_parts(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // H(tag, data) is the HMAC of tag, 0x00, data; zero-padding a key does not change it.
        let mut key = [0u8; 32];
        key[..20].fill(0x0b);
        assert_eq!(hmac(&key, "Hi", b"x"), hmac_parts(&[0x0b; 20], &[b"Hi\0x"]));
    }

    #[test]
    fn quantization_rounds_half_away_from_zero() {
        assert_eq!(quantize(0.0015), 2);
        assert_eq!(quantize(-0.0015), -2);
        assert_eq!(quantize(1.2344), 1234);
    }

    #[test]
    fn triangle_hash_ignores_rotation_but_not_orientation() {
        let (a, b, c) = ([0, 0, 0], [1000, 0, 0], [0, 1000, 0]);
        assert_eq!(triangle_hashes(a, b, c), triangle_hashes(b, c, a));
        assert_eq!(triangle_hashes(a, b, c), triangle_hashes(c, a, b));
        assert_ne!(triangle_hashes(a, b, c), triangle_hashes(a, c, b));
        // The shape lanes ignore translation, the position lanes do not.
        let t = |p: [i64; 3]| [p[0] + 7, p[1] - 3, p[2] + 11];
        let (moved_position, moved_shape) = triangle_hashes(t(a), t(b), t(c));
        let (position, shape) = triangle_hashes(a, b, c);
        assert_eq!(moved_shape, shape);
        assert_ne!(moved_position, position);
        assert_eq!(twice_area(a, b, c), 1_000_000);
    }

    #[test]
    fn two_significant_digits() {
        assert_eq!(two_digits(0), 0);
        assert_eq!(two_digits(99), 99);
        assert_eq!(two_digits(1849), 1800);
        assert_eq!(two_digits(1850), 1900);
        assert_eq!(two_digits(123_456), 120_000);
    }

    #[test]
    fn bit_lengths() {
        assert_eq!(bit_length(0), 0);
        assert_eq!(bit_length(1), 1);
        assert_eq!(bit_length(1_000_000), 20);
    }
}
