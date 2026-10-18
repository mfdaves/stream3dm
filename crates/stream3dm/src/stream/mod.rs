//! Streaming: the input is framed chunk by chunk and decoded as small rebuilt files, so memory
//! does not grow with the file size.
//!
//! Two kinds of rebuilt file are decoded:
//! - the *document*: every table before the object table and an empty object table, once;
//! - *batches*: the batch prefix and a run of object records. The batch prefix omits the
//!   instance-definition table, so block members decode as ordinary objects (placed by
//!   [`Facts`]), and the light table, so only light objects appear per batch.
//!
//! The decoder validates each document as a whole and can reject every record of a class
//! when one record is inconsistent ("candidate rejected", "rejected atomically"). A batch
//! showing such a grouped rejection is bisected until each record gets the outcome it has on
//! its own, so the report does not depend on how records are batched.
//!
//! Block instance records are routed to small batches of their own: the decoder charges every
//! instance for every record in its document, so mixing them with ordinary records grows
//! quadratically. When the file has block definitions, the input is read twice: the first
//! pass decodes only instances, so every placement is known before the second pass places
//! member meshes as they arrive.

pub(crate) mod frame;
pub(crate) mod rebuild;

#[cfg(test)]
mod tests;

use crate::extract::{Facts, Mode, Tables, finalize};
use crate::reader::{self, ReadError};
use crate::sink::MeshSink;
use crate::{InspectError, Report};
#[cfg(feature = "cadmpeg")]
use cadmpeg::{DecodePolicy, Outcome, decode_records, rejections};
use frame::{Framer, INSTANCE_REF_CLASS, Prefix, Sink, record_class};
use rebuild::mini_file;

/// Tables never kept: no report field reads bitmaps.
const SKIPPED_TABLES: [u32; 1] = [0x1000_0016];
/// Tables left out of batch files.
pub(crate) const BATCH_EXCLUDED_TABLES: [u32; 2] = [
    0x1000_0021, // instance definitions
    0x1000_0012, // lights
];
/// Instance records per instance batch.
#[cfg(feature = "cadmpeg")]
const INSTANCE_BATCH_RECORDS: usize = 256;
/// Automatic batch size: at least this multiple of the batch prefix, within these bounds.
const AUTO_BATCH_PREFIX_MULTIPLE: usize = 4;
const AUTO_BATCH_MIN: usize = 1024 * 1024;
const AUTO_BATCH_MAX: usize = 16 * 1024 * 1024;

/// How an [`Inspector`] reads; the default sizes batches from the file's tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectOptions {
    /// Object-record bytes per batch handed to cadmpeg (a larger single record forms its own
    /// batch). `None` picks a size from the shared tables: four times their size, 1–16 MiB. Our
    /// own reader decodes one record at a time, so this matters only for the records it leaves to
    /// cadmpeg, and not at all without the `cadmpeg` feature.
    pub batch_bytes: Option<usize>,
    /// Decode the document tables and every record with cadmpeg instead of our own reader
    /// (for comparisons; ignored without the `cadmpeg` feature).
    pub cadmpeg_only: bool,
    /// Leave Breps without a cached render mesh untessellated (for comparisons).
    pub skip_tessellation: bool,
}

/// The outcome of [`Inspector::finish`].
#[derive(Debug)]
pub enum Finish {
    /// The input is read: the report.
    Report(Box<Report>),
    /// The file has block definitions: push the same input again from the start, then call
    /// `finish` again.
    Again,
}

/// Builds a [`Report`] from a `.3dm` delivered in slices of any size. One inspector reads one
/// file: once [`Inspector::finish`] has given the report, read the next file with a new one.
pub struct Inspector {
    framer: Framer,
    pipeline: Pipeline,
    pushed: u64,
    first_pass_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    First,
    Second,
}

impl Inspector {
    /// An inspector without a sink.
    pub fn new(options: InspectOptions) -> Self {
        Self::build(options, None)
    }

    /// An inspector that also hands every visible mesh to `sink` (see [`MeshSink`]).
    pub fn with_sink(options: InspectOptions, sink: Box<dyn MeshSink>) -> Self {
        Self::build(options, Some(sink))
    }

    fn build(options: InspectOptions, sink: Option<Box<dyn MeshSink>>) -> Self {
        Self {
            framer: Framer::new(false, &SKIPPED_TABLES),
            pipeline: Pipeline {
                options,
                pass: Pass::First,
                two_pass: false,
                prefix: None,
                document: None,
                batch_target: AUTO_BATCH_MIN,
                #[cfg(feature = "cadmpeg")]
                objects: Batch::default(),
                #[cfg(feature = "cadmpeg")]
                instances: Batch::default(),
                rejected: 0,
                #[cfg(feature = "cadmpeg")]
                baseline_rejections: None,
                sink,
                render: None,
                reader: None,
                ordinal: 0,
                stats: ReaderStats::default(),
            },
            pushed: 0,
            first_pass_bytes: None,
        }
    }

    /// Reads the next slice of the file (slices in order, of any size). Fails as soon as the
    /// input is found invalid.
    pub fn push(&mut self, data: &[u8]) -> Result<(), InspectError> {
        self.pushed += data.len() as u64;
        self.framer.push(data, &mut self.pipeline)
    }

    /// Bytes pushed in the current pass.
    pub fn bytes_consumed(&self) -> u64 {
        self.pushed
    }

    /// How the records seen so far were decoded.
    pub fn reader_stats(&self) -> ReaderStats {
        self.pipeline.stats
    }

    /// Whether the input so far needs what this build leaves out: records of classes or versions
    /// our reader leaves to cadmpeg. Records our reader fails on are damaged, and cadmpeg reads
    /// them no better, so they do not count (they appear as undecoded objects in the report).
    /// Always false with the `cadmpeg` feature. A caller with both builds can switch as soon as
    /// this turns true.
    pub fn needs_full_reader(&self) -> bool {
        !cfg!(feature = "cadmpeg") && self.pipeline.stats.delegated > 0
    }

    /// Ends the current pass, once every byte is pushed: the report, or [`Finish::Again`] when
    /// the file has blocks and must be pushed once more. The second pass must push the same
    /// bytes.
    pub fn finish(&mut self) -> Result<Finish, InspectError> {
        self.framer.finish()?;
        if self.framer.is_legacy() {
            #[cfg(feature = "cadmpeg")]
            if self.pipeline.options.cadmpeg_only {
                let sink = self.pipeline.sink.take();
                let report = crate::inspect_whole_with(self.framer.legacy_bytes(), sink)?;
                return Ok(Finish::Report(Box::new(report)));
            }
            return Ok(Finish::Report(Box::new(self.legacy_report()?)));
        }
        let p = &mut self.pipeline;
        let Some((tables, facts)) = p.document.as_mut() else {
            return Err(InspectError::Framing {
                offset: self.framer.offset(),
                reason: "no object table".to_string(),
            });
        };
        if p.pass == Pass::First && p.two_pass {
            facts.prepare_placements(tables);
            p.pass = Pass::Second;
            p.ordinal = 0;
            self.first_pass_bytes = Some(self.pushed);
            self.pushed = 0;
            self.framer = Framer::new(false, &SKIPPED_TABLES);
            return Ok(Finish::Again);
        }
        if self.first_pass_bytes.is_some_and(|n| n != self.pushed) {
            return Err(InspectError::Framing {
                offset: self.pushed,
                reason: "the second pass input differs from the first".to_string(),
            });
        }
        let size = self.first_pass_bytes.unwrap_or(self.pushed);
        let (tables, mut facts) = p.document.take().expect("checked above");
        facts.add_rejected_records(p.rejected);
        let mut report = finalize(&tables, facts, size);
        if let Some((sun, ground)) = p.render.take() {
            report.render = crate::extract::render_report(&tables, sun, ground);
        }
        Ok(Finish::Report(Box::new(report)))
    }
}

impl Inspector {
    /// The report of a Rhino 1 file, read whole by [`reader::v1`].
    fn legacy_report(&mut self) -> Result<Report, InspectError> {
        let bytes = self.framer.legacy_bytes();
        let (document, objects) = reader::v1::read(bytes, !self.pipeline.options.skip_tessellation)
            .map_err(|e| InspectError::Framing {
                offset: 0,
                reason: format!("Rhino 1 file: {e:?}"),
            })?;
        let tables = Tables::from_document(document);
        let mut facts = Facts::new(&tables);
        if let Some(sink) = self.pipeline.sink.take() {
            facts.set_sink(&tables, sink);
        }
        for view in &objects {
            facts.absorb_object(&tables, view, Mode::Batch);
            self.pipeline.stats.own += 1;
        }
        Ok(finalize(&tables, facts, bytes.len() as u64))
    }
}

/// Object records waiting to be decoded together.
#[cfg(feature = "cadmpeg")]
#[derive(Default)]
struct Batch {
    bytes: Vec<u8>,
    /// End offset of each record in `bytes`.
    ends: Vec<usize>,
}

#[cfg(feature = "cadmpeg")]
impl Batch {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        self.ends.push(self.bytes.len());
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }
}

struct Pipeline {
    options: InspectOptions,
    pass: Pass,
    two_pass: bool,
    prefix: Option<Prefix>,
    document: Option<(Tables, Facts)>,
    #[cfg_attr(not(feature = "cadmpeg"), allow(dead_code))]
    batch_target: usize,
    #[cfg(feature = "cadmpeg")]
    objects: Batch,
    #[cfg(feature = "cadmpeg")]
    instances: Batch,
    /// Records no decoder could read.
    rejected: usize,
    /// Grouped-rejection signals the batch tables raise on their own (no records); decoded
    /// with the first cadmpeg batch, so files our reader handles entirely never need it.
    #[cfg(feature = "cadmpeg")]
    baseline_rejections: Option<usize>,
    /// Handed to the facts once the document tables are known.
    sink: Option<Box<dyn MeshSink>>,
    /// Rhino's sun and ground plane, from the RDK user table.
    render: Option<(Option<reader::rdk::SunRec>, Option<reader::rdk::GroundRec>)>,
    /// Context for our own record reader, once the prefix is known.
    reader: Option<reader::Context>,
    /// Position of the next record in the object table (per pass).
    ordinal: usize,
    stats: ReaderStats,
}

/// How object records were decoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Records decoded by our own reader.
    pub own: usize,
    /// Records of classes our reader leaves to cadmpeg (or, without it, counts undecoded).
    pub delegated: usize,
    /// Records our reader failed on, handed to cadmpeg (or, without it, counted undecoded).
    pub failed: usize,
}

impl Sink for Pipeline {
    fn prefix(&mut self, prefix: &Prefix, _table_header: &[u8]) -> Result<(), InspectError> {
        if self.pass == Pass::Second {
            return match &self.prefix {
                Some(first) if first.bytes == prefix.bytes => Ok(()),
                _ => Err(InspectError::Framing {
                    offset: 0,
                    reason: "the second pass input differs from the first".to_string(),
                }),
            };
        }
        #[cfg(feature = "cadmpeg")]
        let tables = if self.options.cadmpeg_only {
            let document = decode(&mini_file(prefix, &[], &[]), &DecodePolicy::default())?;
            Tables::from_decoded(&document)
        } else {
            Tables::from_document(reader::document::read(prefix))
        };
        #[cfg(not(feature = "cadmpeg"))]
        let tables = Tables::from_document(reader::document::read(prefix));
        let mut facts = Facts::new(&tables);
        if let Some(sink) = self.sink.take() {
            facts.set_sink(&tables, sink);
        }
        self.two_pass = tables.has_blocks();
        let batch_prefix: usize = prefix
            .spans
            .iter()
            .filter(|s| !BATCH_EXCLUDED_TABLES.contains(&s.typecode))
            .map(|s| s.end - s.start)
            .sum();
        self.batch_target = self.options.batch_bytes.unwrap_or_else(|| {
            (AUTO_BATCH_PREFIX_MULTIPLE * batch_prefix).clamp(AUTO_BATCH_MIN, AUTO_BATCH_MAX)
        });
        self.reader = Some(reader::Context {
            width: prefix.width,
            archive: reader::archive_version(&prefix.bytes),
            stamp: reader::writer_stamp(prefix),
            mm_per_unit: Some(tables.geometry_mm_per_unit()),
            tessellate: !self.options.skip_tessellation,
        });
        self.prefix = Some(prefix.clone());
        self.document = Some((tables, facts));
        Ok(())
    }

    fn record(&mut self, chunk: &[u8]) -> Result<(), InspectError> {
        let width = self.prefix.as_ref().expect("prefix precedes records").width;
        let ordinal = self.ordinal;
        self.ordinal += 1;
        let instance = record_class(chunk, width) == Some(INSTANCE_REF_CLASS);
        match (self.pass, instance) {
            (Pass::First, false) if self.two_pass => return Ok(()),
            (Pass::Second, true) => return Ok(()),
            _ => {}
        }
        let ctx = self.reader.expect("prefix precedes records");
        let own = if self.options.cadmpeg_only && cfg!(feature = "cadmpeg") {
            Err(ReadError::Unsupported("disabled"))
        } else {
            reader::read_object(chunk, ordinal, &ctx)
        };
        match own {
            Ok(view) => {
                self.stats.own += 1;
                let (tables, facts) = self.document.as_mut().expect("document precedes records");
                facts.absorb_object(tables, &view, Mode::Batch);
                return Ok(());
            }
            Err(ReadError::Unsupported(_)) => self.stats.delegated += 1,
            Err(_) => self.stats.failed += 1,
        }
        #[cfg(feature = "cadmpeg")]
        self.queue(chunk, instance);
        #[cfg(not(feature = "cadmpeg"))]
        self.undecoded(chunk, ordinal, &ctx);
        Ok(())
    }

    fn object_table_end(&mut self) -> Result<(), InspectError> {
        #[cfg(feature = "cadmpeg")]
        {
            self.flush_instances();
            self.flush_objects();
        }
        Ok(())
    }

    fn user_table(&mut self, chunk: &[u8]) {
        let Some(width) = self.prefix.as_ref().map(|p| p.width) else {
            return;
        };
        if let Some(found) = reader::rdk::render_settings(chunk, width) {
            self.render = Some(found);
        }
    }
}

impl Pipeline {
    /// Counts a record our reader cannot decode as an undecoded object, keeping its attributes.
    #[cfg(not(feature = "cadmpeg"))]
    fn undecoded(&mut self, chunk: &[u8], ordinal: usize, ctx: &reader::Context) {
        let (tables, facts) = self.document.as_mut().expect("document precedes records");
        match reader::read_undecoded(chunk, ordinal, ctx) {
            Ok((view, class)) => {
                facts.add_retained(&class);
                facts.absorb_object(tables, &view, Mode::Batch);
            }
            Err(_) => self.rejected += 1,
        }
    }
}

#[cfg(feature = "cadmpeg")]
impl Pipeline {
    /// Queues a record for cadmpeg: instances in small batches of their own.
    fn queue(&mut self, chunk: &[u8], instance: bool) {
        if instance {
            self.instances.push(chunk);
            if self.instances.ends.len() >= INSTANCE_BATCH_RECORDS
                || self.instances.bytes.len() >= self.batch_target
            {
                self.flush_instances();
            }
        } else {
            self.objects.push(chunk);
            if self.objects.bytes.len() >= self.batch_target {
                self.flush_objects();
            }
        }
    }

    fn flush_instances(&mut self) {
        let batch = std::mem::take(&mut self.instances);
        self.decode_batch(&batch);
        self.instances = batch;
        self.instances.clear();
    }

    fn flush_objects(&mut self) {
        let batch = std::mem::take(&mut self.objects);
        self.decode_batch(&batch);
        self.objects = batch;
        self.objects.clear();
    }

    fn decode_batch(&mut self, batch: &Batch) {
        if batch.ends.is_empty() {
            return;
        }
        let prefix = self.prefix.as_ref().expect("prefix precedes records");
        let baseline_rejections = *self.baseline_rejections.get_or_insert_with(|| {
            decode(
                &mini_file(prefix, &BATCH_EXCLUDED_TABLES, &[]),
                &DecodePolicy::default(),
            )
            .map_or(0, |baseline| rejections(&baseline))
        });
        let (tables, facts) = self.document.as_mut().expect("document precedes records");
        let mut outcome = Outcome {
            rejected: &mut self.rejected,
            baseline_rejections,
        };
        decode_records(
            prefix,
            tables,
            facts,
            batch,
            0..batch.ends.len(),
            &mut outcome,
        );
    }
}

#[cfg(feature = "cadmpeg")]
mod cadmpeg {
    use super::*;
    use cadmpeg_codec_rhino::RhinoCodec;
    pub(crate) use cadmpeg_core::decode::DecodePolicy;
    use cadmpeg_core::decode::ResourceLimits;
    use cadmpeg_ir::report::Severity;
    use cadmpeg_ir::{Codec, DecodeOptions, DecodeResult};
    use std::io::Cursor;

    pub(super) struct Outcome<'a> {
        pub(super) rejected: &'a mut usize,
        pub(super) baseline_rejections: usize,
    }

    /// Losses that signal the decoder rejected records as a group.
    pub(super) fn rejections(decoded: &DecodeResult) -> usize {
        decoded
            .report()
            .losses
            .iter()
            .filter(|l| {
                matches!(l.severity, Severity::Error | Severity::Blocking)
                    || l.message.contains("candidate rejected")
                    || l.message.contains("rejected atomically")
            })
            .count()
    }

    /// Decodes records `range` of `batch` as one file. A failure (after a retry with raised
    /// limits) or a grouped rejection splits the range in two, so a problem record costs a
    /// logarithmic number of decodes and every record ends with the outcome it has alone.
    pub(super) fn decode_records(
        prefix: &Prefix,
        tables: &Tables,
        facts: &mut Facts,
        batch: &Batch,
        range: std::ops::Range<usize>,
        outcome: &mut Outcome,
    ) {
        let start = if range.start == 0 {
            0
        } else {
            batch.ends[range.start - 1]
        };
        let end = batch.ends[range.end - 1];
        let file = mini_file(prefix, &BATCH_EXCLUDED_TABLES, &batch.bytes[start..end]);
        let decoded =
            decode(&file, &DecodePolicy::default()).or_else(|_| decode(&file, &generous_policy()));
        drop(file);
        match decoded {
            Ok(decoded)
                if range.len() == 1 || rejections(&decoded) <= outcome.baseline_rejections =>
            {
                facts.absorb(tables, &decoded, Mode::Batch)
            }
            Err(_) if range.len() == 1 => *outcome.rejected += 1,
            _ => {
                let mid = range.start + range.len() / 2;
                decode_records(prefix, tables, facts, batch, range.start..mid, outcome);
                decode_records(prefix, tables, facts, batch, mid..range.end, outcome);
            }
        }
    }

    pub(crate) fn decode(
        bytes: &[u8],
        policy: &DecodePolicy,
    ) -> Result<DecodeResult, InspectError> {
        let options = DecodeOptions {
            container_only: false,
            policy: *policy,
        };
        RhinoCodec
            .decode(&mut Cursor::new(bytes), &options)
            .map_err(|e| InspectError::Decode(e.to_string()))
    }

    /// Desktop limits with entity, collection and work ceilings raised 16×, for single files the
    /// default limits reject (the decoder applies them per document).
    pub(crate) fn generous_policy() -> DecodePolicy {
        let desktop = ResourceLimits::desktop();
        DecodePolicy {
            limits: ResourceLimits {
                max_entities: desktop.max_entities * 16,
                max_collection_items: desktop.max_collection_items * 16,
                max_work_units: desktop.max_work_units * 16,
                ..desktop
            },
            ..DecodePolicy::desktop()
        }
    }
}
#[cfg(feature = "cadmpeg")]
pub(crate) use cadmpeg::{decode, generous_policy};

/// Rhino's sun and ground plane in a whole file, for the paths that do not stream it.
#[cfg(feature = "cadmpeg")]
pub(crate) fn render_settings_of(
    bytes: &[u8],
) -> Option<(Option<reader::rdk::SunRec>, Option<reader::rdk::GroundRec>)> {
    struct Find {
        width: Option<frame::Width>,
        found: Option<(Option<reader::rdk::SunRec>, Option<reader::rdk::GroundRec>)>,
    }
    impl Sink for Find {
        fn prefix(&mut self, prefix: &Prefix, _: &[u8]) -> Result<(), InspectError> {
            self.width = Some(prefix.width);
            Ok(())
        }
        fn record(&mut self, _: &[u8]) -> Result<(), InspectError> {
            Ok(())
        }
        fn object_table_end(&mut self) -> Result<(), InspectError> {
            Ok(())
        }
        fn user_table(&mut self, chunk: &[u8]) {
            if let Some(found) = self
                .width
                .and_then(|w| reader::rdk::render_settings(chunk, w))
            {
                self.found = Some(found);
            }
        }
    }
    let mut find = Find {
        width: None,
        found: None,
    };
    Framer::new(false, &[]).push(bytes, &mut find).ok()?;
    find.found
}

/// The tables of `bytes` and its first `records` object records as a small `.3dm` (for
/// looking inside files too large to decode whole).
/// The same file with its object records in a seeded random order (every other chunk kept).
pub(crate) fn shuffle(bytes: &[u8], seed: u64) -> Result<Vec<u8>, InspectError> {
    struct Collect {
        prefix: Option<Prefix>,
        records: Vec<Vec<u8>>,
    }
    impl Sink for Collect {
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
    let mut collect = Collect {
        prefix: None,
        records: Vec::new(),
    };
    let mut framer = Framer::new(false, &[]);
    framer.push(bytes, &mut collect)?;
    framer.finish()?;
    let prefix = collect.prefix.ok_or(InspectError::Framing {
        offset: 0,
        reason: "no object table".to_string(),
    })?;
    // Fisher-Yates with SplitMix64.
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let records = &mut collect.records;
    for i in (1..records.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        records.swap(i, j);
    }
    Ok(mini_file(&prefix, &[], &records.concat()))
}

pub(crate) fn sample(bytes: &[u8], records: usize) -> Result<Vec<u8>, InspectError> {
    struct Take {
        prefix: Option<Prefix>,
        records: Vec<u8>,
        left: usize,
    }
    impl Sink for Take {
        fn prefix(&mut self, prefix: &Prefix, _: &[u8]) -> Result<(), InspectError> {
            self.prefix = Some(prefix.clone());
            Ok(())
        }
        fn record(&mut self, chunk: &[u8]) -> Result<(), InspectError> {
            if self.left > 0 {
                self.records.extend_from_slice(chunk);
                self.left -= 1;
            }
            Ok(())
        }
        fn object_table_end(&mut self) -> Result<(), InspectError> {
            Ok(())
        }
    }
    let mut take = Take {
        prefix: None,
        records: Vec::new(),
        left: records,
    };
    let mut framer = Framer::new(false, &[]);
    framer.push(bytes, &mut take)?;
    framer.finish()?;
    let prefix = take.prefix.ok_or(InspectError::Framing {
        offset: 0,
        reason: "no object table".to_string(),
    })?;
    Ok(mini_file(&prefix, &[], &take.records))
}
