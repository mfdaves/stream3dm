//! Streaming tests over the public samples (`samples/fetch.sh`); skipped when absent.
//!
//! - batch invariance: any batch size gives the same report;
//! - slice invariance: any split of the input into pushes gives the same report;
//! - faults: truncated or corrupt input gives a typed error, never a panic;
//! - whole-file parity: streaming matches decoding the whole file at once, except that it may
//!   recover objects the whole-file decode rejects together with an inconsistent neighbour.

use std::path::{Path, PathBuf};
#[cfg(feature = "cadmpeg")]
use stream3dm::inspect_whole;
use stream3dm::{Finish, InspectError, InspectOptions, Inspector, Report, inspect_streaming};

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn samples() -> Vec<(String, Vec<u8>)> {
    let Ok(manifest) = std::fs::read_to_string(samples_dir().join("manifest.tsv")) else {
        return Vec::new();
    };
    manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split('\t').next())
        .filter_map(|p| Some((p.to_string(), std::fs::read(samples_dir().join(p)).ok()?)))
        .collect()
}

fn options(batch_bytes: Option<usize>) -> InspectOptions {
    InspectOptions {
        batch_bytes,
        ..Default::default()
    }
}

/// Pushes `bytes` in slices of the given sizes (cycled), passing twice when asked to.
fn stream_in_slices(bytes: &[u8], sizes: &[usize]) -> Result<Report, InspectError> {
    let mut inspector = Inspector::new(InspectOptions::default());
    loop {
        let (mut at, mut i) = (0, 0);
        while at < bytes.len() {
            let end = (at + sizes[i % sizes.len()]).min(bytes.len());
            inspector.push(&bytes[at..end])?;
            at = end;
            i += 1;
        }
        if let Finish::Report(report) = inspector.finish()? {
            return Ok(*report);
        }
    }
}

/// SplitMix64, for reproducible pseudo-random slice sizes and cut points.
fn split_mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn diff(a: &Report, b: &Report) -> String {
    let (a, b) = (
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap(),
    );
    let mut out = Vec::new();
    walk("", &a, &b, &mut out);
    out.join("; ")
}

fn walk(path: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value::{Array, Object};
    if out.len() >= 8 || a == b {
        return;
    }
    match (a, b) {
        (Object(x), Object(y)) => {
            for (k, v) in x {
                walk(
                    &format!("{path}.{k}"),
                    v,
                    y.get(k).unwrap_or(&serde_json::Value::Null),
                    out,
                );
            }
        }
        (Array(x), Array(y)) if x.len() == y.len() => {
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                walk(&format!("{path}[{i}]"), v, w, out);
            }
        }
        _ => out.push(format!("{path}: {a} vs {b}")),
    }
}

/// B1: batch size never changes the report.
#[test]
fn batch_size_does_not_change_the_report() {
    for (path, bytes) in samples() {
        let Ok(reference) = inspect_streaming(&bytes, options(None)) else {
            continue;
        };
        let mut sizes = vec![Some(64 * 1024), Some(usize::MAX)];
        if bytes.len() <= 512 * 1024 {
            sizes.push(Some(1)); // one record per batch
        }
        for size in sizes {
            let report = inspect_streaming(&bytes, options(size)).expect("decodes");
            assert!(
                report == reference,
                "{path}: batch {size:?} differs: {}",
                diff(&reference, &report)
            );
        }
    }
}

/// A2: how the input is sliced into pushes never changes the report.
#[test]
fn slicing_does_not_change_the_report() {
    for (path, bytes) in samples() {
        let Ok(reference) = inspect_streaming(&bytes, InspectOptions::default()) else {
            continue;
        };
        let mut state = bytes.len() as u64;
        let random: Vec<usize> = (0..64)
            .map(|_| 1 + (split_mix(&mut state) % 100_000) as usize)
            .collect();
        let mut slicings: Vec<Vec<usize>> = vec![vec![4096], vec![1024 * 1024], random];
        if bytes.len() <= 256 * 1024 {
            slicings.push(vec![7]);
        }
        for sizes in slicings {
            let report = stream_in_slices(&bytes, &sizes).expect("decodes");
            assert!(
                report == reference,
                "{path}: slices {:?}… differ: {}",
                &sizes[..sizes.len().min(3)],
                diff(&reference, &report)
            );
        }
    }
}

/// A3: truncated, corrupt or foreign input fails with a typed error, never a panic.
#[test]
fn faults_are_typed_errors() {
    let files = [
        "rhino3dm/tests/models/blocks.3dm",
        "rhino3dm/tests/models/object_UserStrings.3dm",
        "opennurbs/example_files/V2/v2_my_brep.3dm",
        "opennurbs/example_files/V4/v4_Wheel_PG.3dm",
        "opennurbs/example_files/V7/v7_rhino_logo_nurbs.3dm",
        "rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh8_NamedViews.3dm",
    ];
    assert_eq!(
        inspect_streaming(&[], options(None)).err(),
        Some(InspectError::NotRhinoFile)
    );
    assert_eq!(
        inspect_streaming(b"PK\x03\x04 a zip, not a model", options(None)).err(),
        Some(InspectError::NotRhinoFile)
    );
    for path in files {
        let Ok(bytes) = std::fs::read(samples_dir().join(path)) else {
            continue;
        };
        let reference = inspect_streaming(&bytes, options(None)).expect("decodes");

        // Cut at 40 points, including inside the header and the first chunk header.
        let mut state = 7;
        let mut cuts: Vec<usize> = vec![1, 10, 31, 33, 40];
        cuts.extend((0..35).map(|_| (split_mix(&mut state) as usize) % bytes.len()));
        for cut in cuts {
            match inspect_streaming(&bytes[..cut], options(Some(64 * 1024))) {
                Err(InspectError::Truncated { .. } | InspectError::Framing { .. }) => {}
                other => panic!("{path} cut at {cut}: {other:?}"),
            }
        }

        // Bytes after the end-of-file chunk are ignored, as the decoder does.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0xab; 100]);
        let mut report = inspect_streaming(&padded, options(None)).expect("decodes");
        report.file.size_bytes -= 100;
        assert!(
            report == reference,
            "{path}: trailing bytes change the report"
        );

        // A first-table length pointing far past the end of the file.
        let mut corrupt = bytes.clone();
        let wide = std::str::from_utf8(&bytes[24..32])
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap()
            >= 50;
        let length_at = 32 + 4;
        if wide {
            corrupt[length_at..length_at + 8].copy_from_slice(&(1u64 << 40).to_le_bytes());
        } else {
            corrupt[length_at..length_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        match inspect_streaming(&corrupt, options(None)) {
            Err(InspectError::Truncated { .. } | InspectError::Framing { .. }) => {}
            other => panic!("{path} with corrupt length: {other:?}"),
        }
    }
}

/// C1: streaming (with cadmpeg decoding every record) matches the whole-file decode for files
/// without block definitions. Where
/// the whole-file decode rejects records together with an inconsistent neighbour, streaming
/// recovers them: it may report fewer undecoded objects, never more.
#[cfg(feature = "cadmpeg")]
#[test]
fn streaming_matches_whole_file_decoding() {
    let mut recovered = Vec::new();
    for (path, bytes) in samples() {
        let cadmpeg_only = InspectOptions {
            cadmpeg_only: true,
            ..Default::default()
        };
        let (Ok(stream), Ok(whole)) = (
            inspect_streaming(&bytes, cadmpeg_only),
            inspect_whole(&bytes),
        ) else {
            continue;
        };
        if !whole.blocks.is_empty() || !stream.blocks.is_empty() {
            // The whole-file decode places blocks itself (see the synthetic house); streaming
            // also reads definition tables cadmpeg leaves out (Rhino 4, see the former gaps
            // in tests/samples.rs).
            continue;
        }
        assert!(
            stream.inventory.undecoded <= whole.inventory.undecoded,
            "{path}: streaming lost objects: {}",
            diff(&whole, &stream)
        );
        if stream.inventory.undecoded < whole.inventory.undecoded {
            recovered.push(format!(
                "{path} ({})",
                whole.inventory.undecoded - stream.inventory.undecoded
            ));
            // Document-level content is unaffected; only object-derived figures change.
            let layers = |r: &Report| {
                r.structure
                    .layers
                    .iter()
                    .map(|l| l.path.clone())
                    .collect::<Vec<_>>()
            };
            let materials = |r: &Report| {
                r.materials
                    .iter()
                    .map(|m| m.name.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(layers(&stream), layers(&whole), "{path}");
            assert_eq!(materials(&stream), materials(&whole), "{path}");
            assert_eq!(stream.views, whole.views, "{path}");
            assert_eq!(stream.file, whole.file, "{path}");
            continue;
        }
        assert!(stream == whole, "{path}: {}", diff(&whole, &stream));
    }
    eprintln!("objects recovered by streaming: {}", recovered.join(", "));
}

/// Files whose reports differ between our record reader and cadmpeg, the report fields that
/// may differ, and why. Everything else must be identical.
#[cfg(feature = "cadmpeg")]
const DECODER_DIFFERENCES: &[(&str, &[&str], &str)] = &[
    (
        "opennurbs/example_files/V4/v4_DVDCase_Solid.3dm",
        &[".style.planar_face_share"],
        "cadmpeg rejects the topology of some Breps and counts no faces for them",
    ),
    (
        "opennurbs/example_files/V4/v4_PerfumeBottle_Solid.3dm",
        &[".style.planar_face_share"],
        "cadmpeg rejects the topology of some Breps and counts no faces for them",
    ),
    (
        "opennurbs/example_files/V4/v4_Prarie settle.3dm",
        &[".style.planar_face_share"],
        "cadmpeg rejects the topology of some Breps and counts no faces for them",
    ),
    (
        "opennurbs/example_files/V4/v4_SaltAndPepper.3dm",
        &[".style.planar_face_share"],
        "cadmpeg rejects the topology of some Breps and counts no faces for them",
    ),
    (
        "opennurbs/example_files/V5/v5_example_file.3dm",
        &[".diagnostics", ".style.planar_face_share", ".inventory"],
        "cadmpeg retains one Brep and the text and leaders undecoded, and counts the layout \
         detail as a curve",
    ),
    (
        "rhino3dm/tests/models/textEntities_r8.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains a text dot and the text undecoded",
    ),
    (
        "rhino-developer-samples/rhino3dm/models/Rhino_Logo_Mesh7.3dm",
        &[""],
        "the only mesh has faces with out-of-range indices: cadmpeg drops the mesh, ours the faces",
    ),
    (
        "opennurbs/example_files/V4/v4_example_file.3dm",
        &[""],
        "cadmpeg reads neither Rhino 4's block instance class nor its definition table; ours \
         places the blocks",
    ),
    (
        "opennurbs/example_files/V4/v4_WishBone.3dm",
        &[""],
        "no unit system and Rhino 4's alternative Brep class: cadmpeg transfers no geometry",
    ),
    (
        "opennurbs/example_files/V5/v5_disk_brake.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "opennurbs/example_files/V5/v5_my_mesh.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "opennurbs/example_files/V5/v5_my_surfaces.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "opennurbs/example_files/V5/v5_my_trimmed_surface.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "rhino-developer-samples/rhino3dm/models/MeshBox_r5.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "rhino-developer-samples/rhino3dm/models/MeshSphere_r5.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "rhino-developer-samples/rhino3dm/models/nurbsSurface_r5.3dm",
        &[""],
        "no unit system: cadmpeg transfers no geometry, ours reads it at 1 unit = 1 mm",
    ),
    (
        "opennurbs/example_files/V2/v2_Patch.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "opennurbs/example_files/V2/v2_Stearman.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "opennurbs/example_files/V2/v2_T-Joint1.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "opennurbs/example_files/V6/V6_annotation.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "opennurbs/example_files/V8/v8_caps_and_joins.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "rhino-developer-samples/rhino3dm/models/textEntities_r8.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains text, leaders or V2 annotations undecoded; ours counts them as annotations",
    ),
    (
        "opennurbs/example_files/V8/v8_section_hatch.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg counts clipping planes as surfaces without a render mesh; ours as other objects",
    ),
    (
        "opennurbs/example_files/V8/v8_selective_clipping.3dm",
        &[".diagnostics", ".inventory"],
        "cadmpeg retains the text undecoded and counts clipping planes as surfaces without a render \
         mesh; ours counts annotations and other objects",
    ),
];

/// Our record reader against cadmpeg, report by report (see [`DECODER_DIFFERENCES`]).
#[cfg(feature = "cadmpeg")]
#[test]
fn own_reader_matches_cadmpeg_reports() {
    let mut unexpected = Vec::new();
    for (path, bytes) in samples() {
        // Rhino 1 files: cadmpeg's decode keeps neither layers nor trimmed surfaces; ours is
        // checked against their Rhino 2 conversions instead (tests/rhino1.rs).
        if path.contains("/V1/") {
            continue;
        }
        let cadmpeg_only = InspectOptions {
            cadmpeg_only: true,
            ..Default::default()
        };
        // cadmpeg does not tessellate; compare decoding only.
        let own_only = InspectOptions {
            skip_tessellation: true,
            ..Default::default()
        };
        let (Ok(own), Ok(theirs)) = (
            inspect_streaming(&bytes, own_only),
            inspect_streaming(&bytes, cadmpeg_only),
        ) else {
            panic!("{path}: one decoder failed");
        };
        if own == theirs {
            continue;
        }
        let allowed = DECODER_DIFFERENCES
            .iter()
            .find(|(p, _, _)| *p == path)
            .map_or(&[][..], |(_, fields, _)| *fields);
        let (a, b) = (
            serde_json::to_value(&theirs).unwrap(),
            serde_json::to_value(&own).unwrap(),
        );
        let mut entries = Vec::new();
        walk("", &a, &b, &mut entries);
        for entry in entries {
            let field = entry.split(": ").next().unwrap_or_default();
            if !allowed.iter().any(|prefix| field.starts_with(prefix)) {
                unexpected.push(format!("{path}: {entry}"));
            }
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}
