//! Reader tests over the public samples (skipped when `samples/` is not fetched).

use super::record::parts;
use super::{Context, ReadError, archive_version, read_object, uuid_string, writer_stamp};
use crate::InspectError;
use crate::extract::{Tables, views_from_decode};
use crate::native::ObjectKind;
use crate::object::{Counted, differences};
use crate::stream::frame::{Framer, Prefix, Sink};
use crate::stream::rebuild::mini_file;
use crate::stream::{BATCH_EXCLUDED_TABLES, decode, generous_policy};
use cadmpeg_core::decode::DecodePolicy;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

/// A table-format file split into its prefix and object records.
pub(crate) struct Framed {
    pub path: String,
    pub prefix: Prefix,
    pub records: Vec<Vec<u8>>,
}

pub(crate) fn frame_file(path: &str, bytes: &[u8]) -> Framed {
    #[derive(Default)]
    struct Records {
        prefix: Option<Prefix>,
        records: Vec<Vec<u8>>,
    }
    impl Sink for Records {
        fn prefix(&mut self, prefix: &Prefix, _: &[u8]) -> Result<(), InspectError> {
            self.prefix = Some(prefix.clone());
            Ok(())
        }
        fn record(&mut self, chunk: &[u8]) -> Result<(), InspectError> {
            self.records.push(chunk.to_vec());
            Ok(())
        }
        fn object_table_end(&mut self) -> Result<(), InspectError> {
            Ok(())
        }
    }
    let mut sink = Records::default();
    let mut framer = Framer::new(false, &[]);
    framer.push(bytes, &mut sink).expect("frames");
    framer.finish().expect("complete");
    Framed {
        path: path.to_string(),
        prefix: sink.prefix.expect("prefix"),
        records: sink.records,
    }
}

/// Every table-format sample (archive 2 and later).
pub(crate) fn sample_files() -> Vec<Framed> {
    let Ok(manifest) = std::fs::read_to_string(samples_dir().join("manifest.tsv")) else {
        return Vec::new();
    };
    manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split('\t').next())
        .filter(|p| !p.contains("/V1/"))
        .filter_map(|p| Some(frame_file(p, &std::fs::read(samples_dir().join(p)).ok()?)))
        .collect()
}

/// Every object record of every sample splits into its parts with valid CRCs.
#[test]
fn records_split_into_parts() {
    let mut classes: BTreeMap<String, usize> = BTreeMap::new();
    let mut records = 0;
    for f in sample_files() {
        for (i, r) in f.records.iter().enumerate() {
            let p =
                parts(r, f.prefix.width).unwrap_or_else(|e| panic!("{} record {i}: {e:?}", f.path));
            assert!(
                p.object.class_crc_ok,
                "{} record {i}: class UUID CRC mismatch",
                f.path
            );
            *classes.entry(uuid_string(&p.object.class)).or_default() += 1;
            records += 1;
        }
    }
    if records > 0 {
        assert!(
            classes.contains_key("4ed7d4e4-e947-11d3-bfe5-0010830122f0"),
            "meshes seen"
        );
        assert!(
            classes.contains_key("60b5dbc5-e660-11d3-bfe4-0010830122f0"),
            "breps seen"
        );
        eprintln!("{records} records, {} classes", classes.len());
    }
}

pub(crate) fn context(prefix: &Prefix, tables: &Tables) -> Context {
    Context {
        width: prefix.width,
        archive: archive_version(&prefix.bytes),
        stamp: writer_stamp(prefix),
        mm_per_unit: tables.mm_per_unit(),
        // Compared with cadmpeg, which does not tessellate.
        tessellate: false,
    }
}

/// How one record compares between the two decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Same,
    /// cadmpeg could not decode the record (or lost its geometry); ours did.
    Recovered,
    Different,
    /// Ours failed on a record of a class it handles.
    Failed,
    /// Ours leaves the class to cadmpeg.
    Unsupported,
}

/// Every record our reader handles decodes to the same view as cadmpeg's decode of that
/// record alone, except where cadmpeg loses the record and ours recovers it.
#[test]
fn records_match_cadmpeg() {
    let files = sample_files();
    if files.is_empty() {
        eprintln!("samples not fetched; skipped");
        return;
    }
    let mut tally: BTreeMap<(String, Outcome), usize> = BTreeMap::new();
    let mut problems = Vec::new();
    let mut categories: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();
    for f in &files {
        let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default())
            .expect("document decodes");
        let tables = Tables::from_decoded(&document);
        let ctx = context(&f.prefix, &tables);
        for (i, record) in f.records.iter().enumerate() {
            let class = parts(record, ctx.width)
                .map(|p| uuid_string(&p.object.class))
                .unwrap_or_default();
            let ours = read_object(record, i, &ctx);
            let outcome = match &ours {
                Err(ReadError::Unsupported(_)) => Outcome::Unsupported,
                Err(e) => {
                    problems.push(format!(
                        "{} record {i} ({class}): ours failed: {e:?}",
                        f.path
                    ));
                    Outcome::Failed
                }
                Ok(view) => {
                    let file = mini_file(&f.prefix, &BATCH_EXCLUDED_TABLES, record);
                    let theirs = decode(&file, &DecodePolicy::default())
                        .or_else(|_| decode(&file, &generous_policy()))
                        .map(|d| views_from_decode(&d).views);
                    match theirs.as_deref() {
                        Ok([expected]) => {
                            // Where cadmpeg lost part of the record, compare the rest only.
                            let mut adjusted = view.clone();
                            let mut recovered = false;
                            if expected.kind == Counted::Object(ObjectKind::Unlinked) {
                                adjusted.kind = expected.kind;
                                adjusted.meshes.clear();
                                adjusted.instance = expected.instance.clone();
                                (adjusted.faces, adjusted.planar_faces) = (0, 0);
                                recovered = true;
                            }
                            // Layout details and clipping planes are not model geometry;
                            // cadmpeg counts them as a curve and a surface.
                            if adjusted.kind == Counted::Object(ObjectKind::Other)
                                && expected.kind != adjusted.kind
                            {
                                adjusted.kind = expected.kind;
                                recovered = true;
                            }
                            if expected.faces == 0 && adjusted.faces > 0 {
                                (adjusted.faces, adjusted.planar_faces) = (0, 0);
                                recovered = true;
                            }
                            if expected.meshes.is_empty() && !adjusted.meshes.is_empty() {
                                adjusted.meshes.clear();
                                if let Counted::Object(ObjectKind::Brep { .. }) = adjusted.kind {
                                    adjusted.kind = expected.kind;
                                }
                                recovered = true;
                            }
                            let diff = differences(expected, &adjusted);
                            if diff.is_empty() {
                                if recovered {
                                    Outcome::Recovered
                                } else {
                                    Outcome::Same
                                }
                            } else {
                                for d in &diff {
                                    let field = d.split(':').next().unwrap_or_default();
                                    let field: String =
                                        field.chars().filter(|c| !c.is_ascii_digit()).collect();
                                    let entry = categories
                                        .entry((class.clone(), field))
                                        .or_insert((0, String::new()));
                                    entry.0 += 1;
                                    if entry.1.is_empty() {
                                        entry.1 = format!("{} record {i}: {d}", f.path);
                                    }
                                }
                                problems.push(format!(
                                    "{} record {i} ({class}): {}",
                                    f.path,
                                    diff.join("; ")
                                ));
                                Outcome::Different
                            }
                        }
                        _ => Outcome::Recovered,
                    }
                }
            };
            *tally.entry((class, outcome)).or_default() += 1;
        }
    }
    for ((class, outcome), n) in &tally {
        eprintln!("{class} {outcome:?} {n}");
    }
    for ((class, field), (n, example)) in &categories {
        eprintln!("DIFF {class} {field}: {n} e.g. {example}");
    }
    for p in problems
        .iter()
        .filter(|p| p.contains("ours failed"))
        .take(20)
    {
        eprintln!("{p}");
    }
    assert!(problems.is_empty(), "{} records differ", problems.len());
}

/// Prints every record of `STREAM3DM_COMPARE_FILE` (any path) where the two decoders disagree.
#[test]
#[ignore]
fn compare_file() {
    let path = std::env::var("STREAM3DM_COMPARE_FILE").unwrap();
    let f = frame_file(&path, &std::fs::read(&path).unwrap());
    let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default()).unwrap();
    let tables = Tables::from_decoded(&document);
    let ctx = context(&f.prefix, &tables);
    for (i, record) in f.records.iter().enumerate() {
        let class = parts(record, ctx.width)
            .map(|p| uuid_string(&p.object.class))
            .unwrap_or_default();
        let Ok(ours) = read_object(record, i, &ctx) else {
            continue;
        };
        let file = mini_file(&f.prefix, &BATCH_EXCLUDED_TABLES, record);
        let theirs = decode(&file, &DecodePolicy::default())
            .or_else(|_| decode(&file, &generous_policy()))
            .map(|d| views_from_decode(&d).views);
        match theirs.as_deref() {
            Ok([expected]) => {
                let diff = differences(expected, &ours);
                if !diff.is_empty() {
                    eprintln!("record {i} ({class}): {}", diff.join("; "));
                }
            }
            other => eprintln!(
                "record {i} ({class}): cadmpeg gave {:?}",
                other.map(<[_]>::len)
            ),
        }
    }
}

/// Times the phases of reading `STREAM3DM_COMPARE_FILE`: framing, decoding records, full report.
#[test]
#[ignore]
fn phase_times() {
    use std::time::Instant;
    let path = std::env::var("STREAM3DM_COMPARE_FILE").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let t = Instant::now();
    let f = frame_file(&path, &bytes);
    eprintln!("frame (with record copies): {:?}", t.elapsed());
    let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default()).unwrap();
    let tables = Tables::from_decoded(&document);
    let ctx = context(&f.prefix, &tables);
    let t = Instant::now();
    let mut n = 0;
    for r in &f.records {
        let p = parts(r, ctx.width).unwrap();
        if let Some(a) = p.attributes {
            n += super::attributes::read(a, ctx.width).map_or(0, |a| a.groups.len());
        }
    }
    eprintln!("parts + attributes: {:?} ({n})", t.elapsed());
    let t = Instant::now();
    let mut triangles = 0;
    for (i, r) in f.records.iter().enumerate() {
        if let Ok(v) = read_object(r, i, &ctx) {
            triangles += v.meshes.iter().map(|m| m.triangle_count).sum::<usize>();
        }
    }
    eprintln!("read_object: {:?} ({triangles} triangles)", t.elapsed());
    let t = Instant::now();
    crate::inspect(&bytes).unwrap();
    eprintln!("inspect: {:?}", t.elapsed());
}

/// Damaged records (bytes flipped, cut short) give an error or a view, never a panic.
#[test]
fn damaged_records_never_panic() {
    let files = sample_files();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut tried = 0;
    for f in files.iter().step_by(3) {
        let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default())
            .expect("document decodes");
        let tables = Tables::from_decoded(&document);
        let ctx = context(&f.prefix, &tables);
        for (i, record) in f.records.iter().enumerate().step_by(7) {
            for _ in 0..12 {
                let mut damaged = record.clone();
                match next() % 3 {
                    0 => {
                        let cut = (next() as usize) % damaged.len().max(1);
                        damaged.truncate(cut);
                    }
                    _ => {
                        for _ in 0..1 + next() % 4 {
                            let at = (next() as usize) % damaged.len();
                            damaged[at] ^= 1 << (next() % 8);
                        }
                    }
                }
                let _ = read_object(&damaged, i, &ctx);
                tried += 1;
            }
        }
    }
    eprintln!("{tried} damaged records read without panicking");
}

/// Counts surface and edge-curve classes inside the Breps of `STREAM3DM_COMPARE_FILE`.
#[test]
#[ignore]
fn brep_class_histogram() {
    use super::cursor::Cursor;
    use super::record::embedded;
    let path = std::env::var("STREAM3DM_COMPARE_FILE").unwrap();
    let f = frame_file(&path, &std::fs::read(&path).unwrap());
    let width = f.prefix.width;
    let mut surfaces: BTreeMap<String, usize> = BTreeMap::new();
    let mut curves: BTreeMap<String, usize> = BTreeMap::new();
    let mut breps = 0;
    let mut meshed = 0;
    let slots = |body: Cursor, out: &mut BTreeMap<String, usize>| {
        let mut c = body;
        c.u8().unwrap();
        let n = c.i32().unwrap();
        for _ in 0..n {
            if c.i32().unwrap() == 1
                && let Some(w) = embedded(&mut c, width).unwrap()
            {
                let name = match super::classes::kind(&w.class) {
                    Some(k) => format!("{k:?} {}", uuid_string(&w.class)),
                    None => uuid_string(&w.class),
                };
                *out.entry(name).or_default() += 1;
            }
        }
    };
    for r in &f.records {
        let p = parts(r, width).unwrap();
        if super::classes::kind(&p.object.class) != Some(super::classes::ClassKind::Brep) {
            continue;
        }
        let mut c = Cursor::new(p.object.data);
        if c.u8().unwrap() >> 4 != 3 {
            continue;
        }
        breps += 1;
        c.chunk(width, 0x4000_8000).unwrap();
        slots(c.chunk(width, 0x4000_8000).unwrap(), &mut curves);
        slots(c.chunk(width, 0x4000_8000).unwrap(), &mut surfaces);
        for _ in 0..5 {
            c.chunk(width, 0x4000_8000).unwrap();
        }
        c.skip(48).unwrap();
        let mut m = c.chunk(width, 0x4000_8000).unwrap();
        if m.u8().is_ok_and(|b| b != 0) {
            meshed += 1;
        }
    }
    eprintln!("{breps} Breps, {meshed} with a first-face mesh");
    eprintln!("surfaces: {surfaces:#?}");
    eprintln!("edge curves: {curves:#?}");
}

/// Area and summed (area-weighted) normal of a mesh.
fn area_normal(m: &super::mesh::Mesh) -> (f64, [f64; 3]) {
    use super::surface::{cross, sub};
    let mut a = 0.0;
    let mut n = [0.0; 3];
    for t in &m.triangles {
        let [p, q, r] = t.map(|i| m.positions_mm[i as usize]);
        let c = cross(sub(q, p), sub(r, p));
        a += 0.5 * (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
        n = [n[0] + c[0], n[1] + c[1], n[2] + c[2]];
    }
    (a, n)
}

/// Our tessellation of flat faces against Rhino's cached render meshes of the same faces, on
/// every sample Brep that has them:
/// - every vertex on the boundary of Rhino's mesh lies on our boundary (so no loop or hole is
///   missing or misplaced);
/// - the areas differ by no more than the strips between the two boundaries can hold (Rhino's
///   meshes are often coarse, so its chords cut inside curves);
/// - both meshes face the same side.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "minutes unoptimized; run with cargo test --release"
)]
fn tessellation_matches_rhino_meshes() {
    use super::surface::dot;
    let (mut compared, mut tight) = (0, 0);
    let mut worst: (f64, String) = (0.0, String::new());
    let mut problems = Vec::new();
    let mut curved: Vec<CurvedCheck> = Vec::new();
    for f in sample_files() {
        let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default())
            .expect("document decodes");
        let tables = Tables::from_decoded(&document);
        let ctx = context(&f.prefix, &tables);
        if ctx.mm_per_unit.is_none() {
            continue;
        }
        for (i, record) in f.records.iter().enumerate() {
            let p = parts(record, ctx.width).unwrap();
            if super::classes::kind(&p.object.class) != Some(super::classes::ClassKind::Brep) {
                continue;
            }
            let Ok(faces) = super::brep::faces_both_ways(p.object.data, &ctx) else {
                continue;
            };
            for (k, face) in faces.iter().enumerate() {
                let (Some(cached), Some(ours)) = (&face.cached, &face.ours) else {
                    continue;
                };
                if !face.flat {
                    curved.push(curved_check(cached, ours, &f.path, i, k));
                    continue;
                }
                compared += 1;
                let (ca, cn) = area_normal(cached);
                let (oa, on) = area_normal(ours);
                let rhino = boundary_rings(cached);
                let mine = boundary_rings(ours);
                let size = extent(&ours.positions_mm);
                let off = rhino
                    .iter()
                    .flatten()
                    .map(|&q| ring_distance(q, &mine))
                    .fold(0.0, f64::max);
                let strips = strip_area(&rhino, &mine) + strip_area(&mine, &rhino);
                let difference = (oa - ca).abs();
                if difference <= 1e-6 * ca {
                    tight += 1;
                }
                let relative = difference / ca.max(f64::MIN_POSITIVE);
                if relative > worst.0 {
                    worst = (
                        relative,
                        format!(
                            "{} record {i} face {k}: {oa:.6} vs Rhino {ca:.6}, Rhino {} vertices, ours {}",
                            f.path,
                            cached.positions_mm.len(),
                            ours.positions_mm.len()
                        ),
                    );
                }
                let facing = dot(cn, on);
                // Rhino meshes each face from its 2D trims, which may stray from the shared
                // edges by the edge tolerance stored in the file.
                let allowed = (1e-3 * size).max(1.01 * face.edge_tolerance);
                if off > allowed || difference > 1.5 * strips + 1e-6 * ca || facing <= 0.0 {
                    problems.push(format!(
                        "{} record {i} face {k}: area {oa:.6} vs Rhino {ca:.6}, strips {strips:.3e}, \
                         boundary offset {off:.2e} (edge tolerance {:.2e}, size {size:.2e}), \
                         facing {facing:.2e}",
                        f.path, face.edge_tolerance
                    ));
                }
            }
        }
    }
    eprintln!(
        "{compared} flat faces compared: {tight} with Rhino's area to 1e-6, largest difference \
         {:.2} % ({})",
        worst.0 * 100.0,
        worst.1
    );
    for p in problems.iter().take(30) {
        eprintln!("{p}");
    }
    report_curved(&curved);
    if !curved.is_empty() {
        // Regression bounds for curved faces (both meshes approximate the surface; Rhino's
        // are often coarse): typical faces within 1 %, nearly all within 10 %, and every
        // accepted face facing Rhino's side over most of its area.
        let mut areas: Vec<f64> = curved.iter().map(|c| c.area).collect();
        areas.sort_by(f64::total_cmp);
        let at = |q: f64| areas[((areas.len() - 1) as f64 * q) as usize];
        assert!(at(0.5) < 0.01, "median curved area difference {}", at(0.5));
        assert!(
            at(0.95) < 0.10,
            "95th percentile curved area difference {}",
            at(0.95)
        );
        let worst = curved
            .iter()
            .min_by(|a, b| a.facing.total_cmp(&b.facing))
            .expect("compared");
        assert!(
            worst.facing > 0.85,
            "{} faces Rhino's side over only {}",
            worst.label,
            worst.facing
        );
    }
    if compared > 0 {
        assert!(
            problems.is_empty(),
            "{} faces differ from Rhino's meshes",
            problems.len()
        );
    }
}

/// One curved face compared with Rhino's mesh of it.
struct CurvedCheck {
    label: String,
    /// Relative area difference.
    area: f64,
    /// Largest distance from a vertex of either mesh to the other mesh, relative to size.
    distance: f64,
    /// Share of our area facing the same side as Rhino's nearest triangle.
    facing: f64,
}

fn curved_check(
    cached: &super::mesh::Mesh,
    ours: &super::mesh::Mesh,
    path: &str,
    record: usize,
    face: usize,
) -> CurvedCheck {
    use super::surface::{cross, dot, sub};
    let (ca, _) = area_normal(cached);
    let (oa, _) = area_normal(ours);
    let size = extent(&ours.positions_mm).max(extent(&cached.positions_mm));
    let distance = mesh_distance(ours, cached).max(mesh_distance(cached, ours)) / size;
    // For each of our triangles, compare its normal with the nearest Rhino triangle's.
    let (mut agree, mut total) = (0.0, 0.0);
    for t in &ours.triangles {
        let [a, b, c] = t.map(|i| ours.positions_mm[i as usize]);
        let n = cross(sub(b, a), sub(c, a));
        let len = dot(n, n).sqrt();
        let centre = [0, 1, 2].map(|k| (a[k] + b[k] + c[k]) / 3.0);
        if let Some(m) = nearest_triangle_normal(cached, centre) {
            total += len;
            if dot(n, m) > 0.0 {
                agree += len;
            }
        }
    }
    CurvedCheck {
        label: format!("{path} record {record} face {face}"),
        area: (oa - ca).abs() / ca.max(f64::MIN_POSITIVE),
        distance,
        facing: if total > 0.0 { agree / total } else { 0.0 },
    }
}

/// Largest distance from a vertex of `from` to the nearest vertex or triangle centre of `to`.
fn mesh_distance(from: &super::mesh::Mesh, to: &super::mesh::Mesh) -> f64 {
    let targets: Vec<[f64; 3]> = to
        .positions_mm
        .iter()
        .copied()
        .chain(to.triangles.iter().map(|t| {
            let [a, b, c] = t.map(|i| to.positions_mm[i as usize]);
            [0, 1, 2].map(|k| (a[k] + b[k] + c[k]) / 3.0)
        }))
        .collect();
    from.positions_mm
        .iter()
        .map(|p| {
            targets
                .iter()
                .map(|q| (0..3).map(|k| (p[k] - q[k]).powi(2)).sum::<f64>())
                .fold(f64::INFINITY, f64::min)
                .sqrt()
        })
        .fold(0.0, f64::max)
}

fn nearest_triangle_normal(m: &super::mesh::Mesh, p: [f64; 3]) -> Option<[f64; 3]> {
    use super::surface::{cross, sub};
    m.triangles
        .iter()
        .map(|t| {
            let [a, b, c] = t.map(|i| m.positions_mm[i as usize]);
            let centre = [0, 1, 2].map(|k| (a[k] + b[k] + c[k]) / 3.0);
            let d = (0..3).map(|k| (p[k] - centre[k]).powi(2)).sum::<f64>();
            (d, cross(sub(b, a), sub(c, a)))
        })
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .map(|(_, n)| n)
}

fn report_curved(checks: &[CurvedCheck]) {
    if checks.is_empty() {
        return;
    }
    let mut by_area: Vec<&CurvedCheck> = checks.iter().collect();
    by_area.sort_by(|a, b| b.area.total_cmp(&a.area));
    let quantile = |q: f64, key: &dyn Fn(&CurvedCheck) -> f64| {
        let mut v: Vec<f64> = checks.iter().map(key).collect();
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * q) as usize]
    };
    eprintln!(
        "{} curved faces compared: area difference median {:.3} %, 95th {:.3} %, max {:.3} %; \
         distance median {:.2e}, max {:.2e} of size; facing median {:.3}, min {:.3}",
        checks.len(),
        quantile(0.5, &|c| c.area) * 100.0,
        quantile(0.95, &|c| c.area) * 100.0,
        quantile(1.0, &|c| c.area) * 100.0,
        quantile(0.5, &|c| c.distance),
        quantile(1.0, &|c| c.distance),
        quantile(0.5, &|c| c.facing),
        quantile(0.0, &|c| c.facing),
    );
    for c in by_area.iter().take(8) {
        eprintln!(
            "  {}: area {:.3} %, distance {:.2e}, facing {:.3}",
            c.label,
            c.area * 100.0,
            c.distance,
            c.facing
        );
    }
}

fn extent(points: &[[f64; 3]]) -> f64 {
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for p in points {
        for i in 0..3 {
            lo[i] = lo[i].min(p[i]);
            hi[i] = hi[i].max(p[i]);
        }
    }
    (0..3).map(|i| (hi[i] - lo[i]).powi(2)).sum::<f64>().sqrt()
}

/// The boundary loops of a mesh (edges used by one triangle) as point rings.
fn boundary_rings(m: &super::mesh::Mesh) -> Vec<Vec<[f64; 3]>> {
    use std::collections::HashMap;
    let mut uses: HashMap<(u32, u32), usize> = HashMap::new();
    for t in &m.triangles {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            *uses.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    let mut next: HashMap<u32, u32> = HashMap::new();
    for t in &m.triangles {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            if uses[&(a.min(b), a.max(b))] == 1 {
                next.insert(a, b);
            }
        }
    }
    let mut rings = Vec::new();
    while let Some(&start) = next.keys().min() {
        let mut ring = vec![m.positions_mm[start as usize]];
        let mut v = next.remove(&start).unwrap();
        while v != start {
            ring.push(m.positions_mm[v as usize]);
            match next.remove(&v) {
                Some(n) => v = n,
                None => break,
            }
        }
        rings.push(ring);
    }
    rings
}

fn segment_distance(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    use super::surface::{dot, sub};
    let e = sub(b, a);
    let ee = dot(e, e);
    let s = if ee > 0.0 {
        (dot(sub(p, a), e) / ee).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let q = [a[0] + s * e[0], a[1] + s * e[1], a[2] + s * e[2]];
    dot(sub(p, q), sub(p, q)).sqrt()
}

/// Distance from `p` to the nearest edge of `rings`.
fn ring_distance(p: [f64; 3], rings: &[Vec<[f64; 3]>]) -> f64 {
    rings
        .iter()
        .flat_map(|r| (0..r.len()).map(move |i| (r[i], r[(i + 1) % r.len()])))
        .map(|(a, b)| segment_distance(p, a, b))
        .fold(f64::INFINITY, f64::min)
}

/// Area the strips between `from`'s edges and the `to` boundary can hold: each edge's length
/// times its midpoint's distance from `to`.
fn strip_area(from: &[Vec<[f64; 3]>], to: &[Vec<[f64; 3]>]) -> f64 {
    from.iter()
        .flat_map(|r| (0..r.len()).map(move |i| (r[i], r[(i + 1) % r.len()])))
        .map(|(a, b)| {
            let m = [0, 1, 2].map(|i| (a[i] + b[i]) / 2.0);
            let len = (0..3).map(|i| (b[i] - a[i]).powi(2)).sum::<f64>().sqrt();
            len * ring_distance(m, to)
        })
        .sum()
}

/// Why faces of the Breps in `STREAM3DM_COMPARE_FILE` (any path) are not tessellated.
#[test]
#[ignore]
fn tessellation_skip_reasons() {
    let path = std::env::var("STREAM3DM_COMPARE_FILE").unwrap();
    let f = frame_file(&path, &std::fs::read(&path).unwrap());
    let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default()).unwrap();
    let tables = Tables::from_decoded(&document);
    let ctx = context(&f.prefix, &tables);
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (i, r) in f.records.iter().enumerate() {
        let p = parts(r, ctx.width).unwrap();
        if super::classes::kind(&p.object.class) != Some(super::classes::ClassKind::Brep) {
            continue;
        }
        match super::brep::skip_reasons(p.object.data, &ctx) {
            Ok(faces) => {
                for (k, (reason, surface)) in faces.iter().enumerate() {
                    let key = match reason {
                        Some(r) => format!("{r:?}: {surface}"),
                        None => "drawn".to_string(),
                    };
                    *reasons.entry(key.clone()).or_default() += 1;
                    let e = examples.entry(key).or_default();
                    if e.len() < 5 {
                        e.push(format!("record {i} face {k}"));
                    }
                }
            }
            Err(e) => *reasons.entry(format!("topology: {e:?}")).or_default() += 1,
        }
    }
    eprintln!("{reasons:#?}");
    eprintln!("{examples:#?}");
}

#[test]
#[ignore]
fn debug_curved_face() {
    let path = std::env::var("STREAM3DM_DEBUG_FILE").unwrap();
    let index: usize = std::env::var("STREAM3DM_DEBUG_RECORD")
        .unwrap()
        .parse()
        .unwrap();
    let face: usize = std::env::var("STREAM3DM_DEBUG_FACE")
        .unwrap()
        .parse()
        .unwrap();
    let f = frame_file(&path, &std::fs::read(samples_dir().join(&path)).unwrap());
    let document = decode(&mini_file(&f.prefix, &[], &[]), &DecodePolicy::default()).unwrap();
    let tables = Tables::from_decoded(&document);
    let ctx = context(&f.prefix, &tables);
    let p = parts(&f.records[index], ctx.width).unwrap();
    eprintln!(
        "{}",
        super::brep::describe_face(p.object.data, &ctx, face).unwrap()
    );
    let faces = super::brep::faces_both_ways(p.object.data, &ctx).unwrap();
    let check = &faces[face];
    if let (Some(c), Some(o)) = (&check.cached, &check.ours) {
        eprintln!(
            "Rhino {} v {} t area {:.4}; ours {} v {} t area {:.4}",
            c.positions_mm.len(),
            c.triangles.len(),
            area_normal(c).0,
            o.positions_mm.len(),
            o.triangles.len(),
            area_normal(o).0
        );
    }
}

/// The report fields that come from the document tables, as JSON.
fn document_fields(tables: &Tables) -> serde_json::Value {
    let facts = crate::extract::Facts::new(tables);
    let report = crate::extract::finalize(tables, facts, 0);
    let mut v = serde_json::to_value(&report).expect("serializes");
    serde_json::json!({
        "file": v["file"].take(),
        "layers": v["structure"]["layers"].take(),
        "groups": v["structure"]["groups"].take(),
        "blocks": v["blocks"].take(),
        "materials": v["materials"].take(),
        "views": v["views"].take(),
        "lights": v["inventory"]["lights"].take(),
        "light_list": v["lights"].take(),
    })
}

/// Paths of the differing leaves of two JSON values.
fn json_differences(
    a: &serde_json::Value,
    b: &serde_json::Value,
    path: String,
    out: &mut Vec<String>,
) {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                let (p, q) = (
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                );
                json_differences(p, q, format!("{path}.{k}"), out);
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                json_differences(p, q, format!("{path}[{i}]"), out);
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            out.push(format!("{path}: {} vs {} items", x.len(), y.len()))
        }
        _ if a != b => out.push(format!("{path}: {a} vs {b}")),
        _ => {}
    }
}

/// Files of the private folder (never committed), when present.
pub(crate) fn private_files() -> Vec<Framed> {
    let mut out = Vec::new();
    let mut stack = vec![samples_dir().join("private")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "3dm")
                && let Ok(bytes) = std::fs::read(&p)
                && archive_version(&bytes) > 1
            {
                out.push(frame_file(&p.display().to_string(), &bytes));
            }
        }
    }
    out
}

/// Files where our document tables differ from cadmpeg's, the fields allowed to differ, and why.
const DOCUMENT_DIFFERENCES: &[(&str, &str, &str)] = &[
    (
        "opennurbs/example_files/V5/v5_ring.3dm",
        ".file.earth_anchor",
        "cadmpeg skips the earth anchor's values (ours is checked against rhino3dm in tests/lights.rs)",
    ),
    (
        "opennurbs/example_files/V4/v4_example_file.3dm",
        ".blocks",
        "cadmpeg does not read Rhino 4's instance definition table",
    ),
    (
        "opennurbs/example_files/V4/v4_WishBone.3dm",
        ".views",
        "no unit system: cadmpeg drops the views, ours reads them at 1 unit = 1 mm",
    ),
    (
        "opennurbs/example_files/V5/v5_disk_brake.3dm",
        ".views",
        "no unit system: cadmpeg drops the views, ours reads them at 1 unit = 1 mm",
    ),
];

/// Our document tables give the same report fields as cadmpeg's, on every sample and private
/// file, except [`DOCUMENT_DIFFERENCES`].
#[test]
fn document_tables_match_cadmpeg() {
    let mut unexpected = Vec::new();
    let files: Vec<Framed> = sample_files().into_iter().chain(private_files()).collect();
    for f in &files {
        let ours = Tables::from_document(super::document::read(&f.prefix));
        let theirs = Tables::from_decoded(
            &decode(&mini_file(&f.prefix, &[], &[]), &generous_policy()).expect("decodes"),
        );
        let mut diffs = Vec::new();
        json_differences(
            &document_fields(&ours),
            &document_fields(&theirs),
            String::new(),
            &mut diffs,
        );
        let allowed: Vec<&str> = DOCUMENT_DIFFERENCES
            .iter()
            .filter(|(p, _, _)| f.path.ends_with(p))
            .map(|(_, field, _)| *field)
            .collect();
        unexpected.extend(
            diffs
                .into_iter()
                .filter(|d| !allowed.iter().any(|a| d.starts_with(a)))
                .map(|d| format!("{}: {d}", f.path)),
        );
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}
