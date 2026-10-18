//! Framer and rebuild tests over the public samples (skipped when `samples/` is not fetched).

use super::frame::{Framer, Prefix, Sink, TCODE_OBJECT_TABLE, Width};
use super::rebuild::{mini_file, object_table};
use crate::InspectError;
use std::path::{Path, PathBuf};

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

/// Manifest samples that use the table format (archive 1 is flat and not framed).
fn framed_samples() -> Vec<(String, Vec<u8>)> {
    let Ok(manifest) = std::fs::read_to_string(samples_dir().join("manifest.tsv")) else {
        return Vec::new();
    };
    manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split('\t').next())
        .filter(|p| !p.contains("/V1/"))
        .filter_map(|p| Some((p.to_string(), std::fs::read(samples_dir().join(p)).ok()?)))
        .collect()
}

/// Records everything the framer reports, in order.
#[derive(Default)]
struct Capture {
    prefix: Option<Prefix>,
    table_header: Vec<u8>,
    records: Vec<Vec<u8>>,
    raw: Vec<u8>,
    trailing: Vec<u8>,
    eof: Vec<u8>,
    table_ended: bool,
}

impl Sink for Capture {
    fn prefix(&mut self, prefix: &Prefix, table_header: &[u8]) -> Result<(), InspectError> {
        self.prefix = Some(prefix.clone());
        self.table_header = table_header.to_vec();
        Ok(())
    }
    fn record(&mut self, chunk: &[u8]) -> Result<(), InspectError> {
        self.records.push(chunk.to_vec());
        Ok(())
    }
    fn object_table_end(&mut self) -> Result<(), InspectError> {
        self.table_ended = true;
        Ok(())
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.raw.extend_from_slice(bytes);
    }
    fn trailing(&mut self, chunk: &[u8]) {
        self.trailing.extend_from_slice(chunk);
    }
    fn eof(&mut self, chunk: &[u8]) {
        self.eof = chunk.to_vec();
    }
}

impl Capture {
    fn reassemble(&self) -> Vec<u8> {
        let mut out = self.prefix.as_ref().expect("prefix").bytes.clone();
        out.extend_from_slice(&self.table_header);
        self.records.iter().for_each(|r| out.extend_from_slice(r));
        out.extend_from_slice(&self.raw);
        out.extend_from_slice(&self.trailing);
        out.extend_from_slice(&self.eof);
        out
    }
}

fn capture(bytes: &[u8], slice: usize, keep_trailing: bool) -> Capture {
    let mut framer = Framer::new(keep_trailing, &[]);
    let mut sink = Capture::default();
    for chunk in bytes.chunks(slice) {
        framer.push(chunk, &mut sink).expect("framing");
    }
    framer.finish().expect("complete file");
    sink
}

/// A1: re-emitting every framed chunk reproduces the file byte for byte, for any slicing.
#[test]
fn framing_is_lossless() {
    let samples = framed_samples();
    for (path, bytes) in &samples {
        for slice in [bytes.len().max(1), 65_536, 4_096, 7] {
            if slice == 7 && bytes.len() > 256 * 1024 {
                continue;
            }
            let sink = capture(bytes, slice, true);
            assert!(sink.table_ended, "{path}: object table not closed");
            let out = sink.reassemble();
            assert_eq!(
                out.len(),
                bytes.len(),
                "{path} (slice {slice}): length differs"
            );
            assert!(out == *bytes, "{path} (slice {slice}): bytes differ");
        }
    }
    eprintln!("framed {} samples", samples.len());
}

/// Rebuilding the object table from its records gives the original table bytes.
#[test]
fn rebuilt_object_table_matches_original() {
    for (path, bytes) in framed_samples() {
        let sink = capture(&bytes, bytes.len(), false);
        let prefix = sink.prefix.as_ref().expect("prefix");
        let records: Vec<u8> = sink.records.concat();
        let mut rebuilt = Vec::new();
        object_table(prefix, &records, &mut rebuilt);
        let mut original = sink.table_header.clone();
        original.extend_from_slice(&records);
        original.extend_from_slice(&sink.raw);
        assert!(rebuilt == original, "{path}: rebuilt object table differs");
    }
}

/// A rebuilt file frames back to the same prefix chunks and records; excluded tables vanish.
#[test]
fn mini_file_round_trips() {
    for (path, bytes) in framed_samples() {
        let sink = capture(&bytes, bytes.len(), false);
        let prefix = sink.prefix.as_ref().expect("prefix");
        let records: Vec<u8> = sink.records.concat();
        let excluded = prefix.spans.first().map(|s| s.typecode).unwrap_or(0);
        let rebuilt = mini_file(prefix, &[excluded], &records);
        let again = capture(&rebuilt, rebuilt.len(), false);
        let p2 = again.prefix.as_ref().expect("prefix");
        assert_eq!(again.records, sink.records, "{path}: records differ");
        assert_eq!(p2.width, prefix.width, "{path}: width differs");
        let kept: Vec<u32> = prefix
            .spans
            .iter()
            .map(|s| s.typecode)
            .filter(|t| *t != excluded)
            .collect();
        let got: Vec<u32> = p2.spans.iter().map(|s| s.typecode).collect();
        assert_eq!(got, kept, "{path}: prefix chunks differ");
        // The end-of-file chunk holds the real size.
        let size_field = &again.eof[again.eof.len() - prefix.width.bytes()..];
        let mut size = [0u8; 8];
        size[..size_field.len()].copy_from_slice(size_field);
        assert_eq!(
            u64::from_le_bytes(size),
            rebuilt.len() as u64,
            "{path}: EOF size"
        );
    }
}

#[test]
fn widths_follow_archive_version() {
    assert_eq!(Width::for_archive(2), Width::Four);
    assert_eq!(Width::for_archive(4), Width::Four);
    assert_eq!(Width::for_archive(5), Width::Four);
    assert_eq!(Width::for_archive(50), Width::Eight);
    assert_eq!(Width::for_archive(80), Width::Eight);
    let mut out = Vec::new();
    Width::Four.put(&mut out, 0x0102_0304);
    Width::Eight.put(&mut out, 5);
    assert_eq!(out, [4, 3, 2, 1, 5, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(TCODE_OBJECT_TABLE, 0x1000_0013);
}

#[test]
fn rejects_non_rhino_input() {
    let mut framer = Framer::new(false, &[]);
    let mut sink = Capture::default();
    let err = framer.push(b"PK\x03\x04 definitely a zip file......", &mut sink);
    assert_eq!(err, Err(InspectError::NotRhinoFile));
}

/// Instance records are recognised from their class chunk: `blocks.3dm` has two placements,
/// and samples without blocks have none.
#[test]
fn instance_records_are_recognised() {
    use super::frame::{INSTANCE_REF_CLASS, record_class};
    let count = |path: &str| -> Option<usize> {
        let bytes = std::fs::read(samples_dir().join(path)).ok()?;
        let sink = capture(&bytes, bytes.len(), false);
        let width = sink.prefix.as_ref().expect("prefix").width;
        Some(
            sink.records
                .iter()
                .filter(|r| record_class(r, width) == Some(INSTANCE_REF_CLASS))
                .count(),
        )
    };
    if let Some(n) = count("rhino3dm/tests/models/blocks.3dm") {
        assert_eq!(n, 2);
    }
    if let Some(n) = count("opennurbs/example_files/V7/v7_rhino_logo_nurbs.3dm") {
        assert_eq!(n, 0);
    }
    // Every record of every sample has a readable class chunk.
    for (path, bytes) in framed_samples() {
        let sink = capture(&bytes, bytes.len(), false);
        let width = sink.prefix.as_ref().expect("prefix").width;
        for r in &sink.records {
            assert!(
                record_class(r, width).is_some(),
                "{path}: record without class chunk"
            );
        }
    }
}

/// Deterministic pseudo-random numbers for shuffling (SplitMix64).
pub(crate) fn split_mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Reports do not depend on the order of object records: each sample is rebuilt with its
/// records shuffled (three seeds) and must give the same report, apart from the file size.
#[test]
fn record_order_does_not_change_the_report() {
    use crate::{InspectOptions, inspect_streaming};
    let options = InspectOptions {
        batch_bytes: Some(64 * 1024),
        ..Default::default()
    };
    for (path, bytes) in framed_samples() {
        let sink = capture(&bytes, bytes.len(), false);
        let prefix = sink.prefix.as_ref().expect("prefix");
        let in_order = mini_file(prefix, &[], &sink.records.concat());
        let Ok(mut expected) = inspect_streaming(&in_order, options) else {
            continue; // files the decoder rejects are covered elsewhere
        };
        expected.file.size_bytes = 0;
        for seed in [1u64, 2, 3] {
            let mut state = seed;
            let mut records = sink.records.clone();
            for i in (1..records.len()).rev() {
                records.swap(i, (split_mix(&mut state) % (i as u64 + 1)) as usize);
            }
            let shuffled = mini_file(prefix, &[], &records.concat());
            let mut got = inspect_streaming(&shuffled, options).expect("shuffled file decodes");
            got.file.size_bytes = 0;
            assert!(
                got == expected,
                "{path}: report changes with record order (seed {seed}): {}",
                json_diff(&expected, &got)
            );
        }
    }
}

/// The second pass must see the same input as the first.
#[test]
fn second_pass_must_match_the_first() {
    use crate::{Finish, InspectOptions, Inspector};
    let Ok(blocks) = std::fs::read(samples_dir().join("rhino3dm/tests/models/blocks.3dm")) else {
        return;
    };
    let Ok(other) = std::fs::read(samples_dir().join("rhino3dm/tests/models/groups.3dm")) else {
        return;
    };
    let mut inspector = Inspector::new(InspectOptions::default());
    inspector.push(&blocks).expect("first pass");
    assert!(matches!(inspector.finish(), Ok(Finish::Again)));
    let err = inspector
        .push(&other)
        .err()
        .or_else(|| inspector.finish().err());
    assert!(matches!(err, Some(InspectError::Framing { .. })), "{err:?}");
}

/// The first differing JSON paths of two reports, for failure messages.
pub(crate) fn json_diff(a: &crate::Report, b: &crate::Report) -> String {
    fn walk(path: String, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
        use serde_json::Value::{Array, Object};
        if out.len() >= 12 || a == b {
            return;
        }
        match (a, b) {
            (Object(x), Object(y)) => {
                for (k, v) in x {
                    walk(
                        format!("{path}.{k}"),
                        v,
                        y.get(k).unwrap_or(&serde_json::Value::Null),
                        out,
                    );
                }
            }
            (Array(x), Array(y)) if x.len() == y.len() => {
                for (i, (v, w)) in x.iter().zip(y).enumerate() {
                    walk(format!("{path}[{i}]"), v, w, out);
                }
            }
            _ => out.push(format!("{path}: {a} vs {b}")),
        }
    }
    let mut out = Vec::new();
    walk(
        String::new(),
        &serde_json::to_value(a).expect("serializes"),
        &serde_json::to_value(b).expect("serializes"),
        &mut out,
    );
    out.join("; ")
}
