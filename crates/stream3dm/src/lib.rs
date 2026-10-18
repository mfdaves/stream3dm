//! stream3dm extraction core: decodes a Rhino `.3dm` and builds a [`Report`].
//!
//! No filesystem or platform access, so the same code runs natively and in WebAssembly. Input
//! can be streamed in slices with [`Inspector`], keeping memory bounded for large files. A
//! [`MeshSink`] receives the visible geometry as it streams; [`Fingerprinter`] is one.
//! [`IfcInspector`] reads IFC models into the same report.
//!
//! # Examples
//!
//! A whole file held in memory:
//!
//! ```no_run
//! let bytes = std::fs::read("model.3dm")?;
//! let report = stream3dm::inspect(&bytes)?;
//! println!(
//!     "{} objects, {} layers, units {}",
//!     report.inventory.objects,
//!     report.structure.layers.len(),
//!     report.file.unit_system,
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Streaming in slices: a file with blocks is pushed twice, the first pass gathering the
//! placements ([`inspect_reader`] does the same for any [`Read`] + [`Seek`] source).
//!
//! ```no_run
//! use stream3dm::{Finish, InspectOptions, Inspector};
//! use std::io::{Read, Seek, SeekFrom};
//!
//! let mut file = std::fs::File::open("model.3dm")?;
//! let mut inspector = Inspector::new(InspectOptions::default());
//! let mut slice = vec![0; 1 << 20];
//! let report = loop {
//!     file.seek(SeekFrom::Start(0))?;
//!     loop {
//!         let n = file.read(&mut slice)?;
//!         if n == 0 {
//!             break;
//!         }
//!         inspector.push(&slice[..n])?;
//!     }
//!     match inspector.finish()? {
//!         Finish::Report(report) => break *report,
//!         Finish::Again => continue,
//!     }
//! };
//! println!("{} triangles", report.inventory.mesh_triangles);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Collecting the visible meshes with a [`MeshSink`], kept in an `Rc<RefCell<_>>` so it can be
//! read once the inspector is done:
//!
//! ```no_run
//! use stream3dm::{DocumentInfo, InspectOptions, Inspector, MeshSink, PlacedMesh};
//! use std::{cell::RefCell, rc::Rc};
//!
//! #[derive(Default)]
//! struct Meshes {
//!     layers: Vec<String>,
//!     /// Positions in metres and triangles, per mesh, with its layer path.
//!     meshes: Vec<(Vec<[f64; 3]>, Vec<[u32; 3]>, Option<String>)>,
//! }
//!
//! impl MeshSink for Meshes {
//!     fn document(&mut self, document: &DocumentInfo) {
//!         self.layers = document.layers.iter().map(|l| l.path.clone()).collect();
//!     }
//!     fn mesh(&mut self, mesh: &PlacedMesh) {
//!         let layer = mesh.layer.map(|i| self.layers[i].clone());
//!         self.meshes.push((mesh.positions.to_vec(), mesh.triangles.to_vec(), layer));
//!     }
//! }
//!
//! let sink = Rc::new(RefCell::new(Meshes::default()));
//! let mut inspector = Inspector::with_sink(InspectOptions::default(), Box::new(sink.clone()));
//! let file = std::fs::File::open("model.3dm")?;
//! let report = stream3dm::inspect_reader_with(file, &mut inspector)?;
//! println!("{} meshes, {} objects", sink.borrow().meshes.len(), report.inventory.objects);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Two versions of a model compared through their fingerprints (docs/fingerprint.md), which a
//! service can store instead of the files; both are keyed with the owner's key:
//!
//! ```no_run
//! use stream3dm::{FingerprintOptions, InspectOptions, fingerprint_bytes};
//!
//! let options = FingerprintOptions { key: Some([7; 32]), clear_names: false };
//! let read = |path| -> Result<_, Box<dyn std::error::Error>> {
//!     let bytes = std::fs::read(path)?;
//!     Ok(fingerprint_bytes(&bytes, InspectOptions::default(), &options)?.1)
//! };
//! let (before, after) = (read("v1.3dm")?, read("v2.3dm")?);
//! let diff = stream3dm::fingerprint::diff(&before, &after)?;
//! println!(
//!     "same: {}, {} objects added, {} removed",
//!     diff.same,
//!     diff.objects.added.len(),
//!     diff.objects.removed.len()
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! An IFC file's metadata, without geometry: push the bytes, [`IfcInspector::prepare`], then
//! list the products (a geometry engine's meshes would go to [`IfcInspector::add_product`]
//! before [`IfcInspector::finish`] builds the report):
//!
//! ```no_run
//! let mut ifc = stream3dm::IfcInspector::new();
//! ifc.push(&std::fs::read("model.ifc")?)?;
//! ifc.prepare()?;
//! for p in ifc.products().iter().take(10) {
//!     println!("{} {} {:?} on {:?}", p.id, p.class, p.name, p.material);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![warn(missing_docs)]

mod context;
mod extract;
pub mod fingerprint;
mod geometry;
mod ifc;
mod native;
mod object;
mod reader;
mod report;
mod sink;
mod stream;
mod subdivide;

pub use context::{
    CONTEXT_VERSION, Context, ContextBlock, ContextLayer, ContextMaterial, ContextUserString,
    ContextView, context,
};
pub use extract::SCHEMA_VERSION;
pub use fingerprint::{Fingerprint, FingerprintDiff, FingerprintOptions, Fingerprinter};
pub use ifc::{IfcInspector, IfcProduct, ProductPart};
pub use report::*;
pub use sink::{
    DocumentInfo, LayerInfo, MaterialInfo, MeshSink, Pick, PlacedMesh, PlacedWire, Tee,
};
pub use stream::{Finish, InspectOptions, Inspector, ReaderStats};

#[cfg(feature = "cadmpeg")]
use extract::{Facts, Mode, Tables, finalize};
use std::fmt;
use std::io::{Read, Seek, SeekFrom};

/// Slice size used when reading from a [`Read`] source.
const READ_SLICE: usize = 1024 * 1024;

/// Why a file could not be read: a `.3dm`, or an IFC file for [`IfcInspector`] (which reports
/// what is wrong with it as [`InspectError::Decode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectError {
    /// The bytes do not start with the `.3dm` file signature.
    NotRhinoFile,
    /// The input ended before the end-of-file chunk.
    Truncated {
        /// Bytes read when the input ended.
        offset: u64,
    },
    /// The chunk structure is invalid at `offset`.
    Framing {
        /// Byte offset in the input where the problem was found.
        offset: u64,
        /// What is wrong, in English.
        reason: String,
    },
    /// The decoder rejected the file.
    Decode(String),
    /// Reading the input failed.
    Io(String),
    /// The file needs a decoder this build leaves out (built without the `cadmpeg` feature).
    Unsupported(String),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRhinoFile => f.write_str("not a Rhino .3dm file"),
            Self::Truncated { offset } => write!(f, "file truncated at byte {offset}"),
            Self::Framing { offset, reason } => {
                write!(f, "invalid chunk at byte {offset}: {reason}")
            }
            Self::Decode(message) => write!(f, "decode failed: {message}"),
            Self::Io(message) => write!(f, "read failed: {message}"),
            Self::Unsupported(what) => write!(f, "not supported by this build: {what}"),
        }
    }
}

impl std::error::Error for InspectError {}

/// Inspects a whole file held in memory (streamed internally in batches).
pub fn inspect(bytes: &[u8]) -> Result<Report, InspectError> {
    inspect_streaming(bytes, InspectOptions::default())
}

/// Streams `bytes` through an [`Inspector`] (pushing them twice when the file has blocks).
pub fn inspect_streaming(bytes: &[u8], options: InspectOptions) -> Result<Report, InspectError> {
    let mut inspector = Inspector::new(options);
    loop {
        inspector.push(bytes)?;
        if let Finish::Report(report) = inspector.finish()? {
            return Ok(*report);
        }
    }
}

/// Streams a seekable source in 1 MiB slices, reading it twice when the file has blocks.
pub fn inspect_reader<R: Read + Seek>(
    reader: R,
    options: InspectOptions,
) -> Result<Report, InspectError> {
    inspect_reader_with(reader, &mut Inspector::new(options))
}

/// [`inspect_reader`] with a prepared inspector (for example one with a [`MeshSink`]).
pub fn inspect_reader_with<R: Read + Seek>(
    mut reader: R,
    inspector: &mut Inspector,
) -> Result<Report, InspectError> {
    let io = |e: std::io::Error| InspectError::Io(e.to_string());
    let mut buffer = vec![0u8; READ_SLICE];
    loop {
        reader.seek(SeekFrom::Start(0)).map_err(io)?;
        loop {
            let n = reader.read(&mut buffer).map_err(io)?;
            if n == 0 {
                break;
            }
            inspector.push(&buffer[..n])?;
        }
        if let Finish::Report(report) = inspector.finish()? {
            return Ok(*report);
        }
    }
}

/// Inspects a seekable source and computes its fingerprint (see [`fingerprint`]) in the same
/// pass.
pub fn fingerprint_reader<R: Read + Seek>(
    reader: R,
    options: InspectOptions,
    fingerprint_options: &FingerprintOptions,
) -> Result<(Report, Fingerprint), InspectError> {
    let sink = std::rc::Rc::new(std::cell::RefCell::new(Fingerprinter::new()));
    let mut inspector = Inspector::with_sink(options, Box::new(sink.clone()));
    let report = inspect_reader_with(reader, &mut inspector)?;
    drop(inspector);
    let fingerprint = sink.borrow().finish(&report, fingerprint_options);
    Ok((report, fingerprint))
}

/// [`fingerprint_reader`] over bytes in memory.
pub fn fingerprint_bytes(
    bytes: &[u8],
    options: InspectOptions,
    fingerprint_options: &FingerprintOptions,
) -> Result<(Report, Fingerprint), InspectError> {
    fingerprint_reader(std::io::Cursor::new(bytes), options, fingerprint_options)
}

/// The same `.3dm` with its object records in a seeded random order (for invariance tests).
#[doc(hidden)]
pub fn shuffled_file(bytes: &[u8], seed: u64) -> Result<Vec<u8>, InspectError> {
    stream::shuffle(bytes, seed)
}

/// The tables and first `records` object records of a `.3dm`, as a small `.3dm`.
#[doc(hidden)]
pub fn sample_file(bytes: &[u8], records: usize) -> Result<Vec<u8>, InspectError> {
    stream::sample(bytes, records)
}

/// Decodes the whole file at once with cadmpeg, with raised decoder limits: how
/// [`InspectOptions::cadmpeg_only`] reads archive 1 files (which have no tables to stream), and
/// the reference the streaming path is tested against.
#[cfg(feature = "cadmpeg")]
#[doc(hidden)]
pub fn inspect_whole(bytes: &[u8]) -> Result<Report, InspectError> {
    inspect_whole_with(bytes, None)
}

/// [`inspect_whole`], handing the visible meshes to `sink` (archive 1 files read with
/// `cadmpeg_only` through an [`Inspector`] with a sink).
#[cfg(feature = "cadmpeg")]
pub(crate) fn inspect_whole_with(
    bytes: &[u8],
    sink: Option<Box<dyn MeshSink>>,
) -> Result<Report, InspectError> {
    let policy = stream::generous_policy();
    if !bytes.starts_with(stream::frame::MAGIC) {
        return Err(InspectError::NotRhinoFile);
    }
    let decoded = stream::decode(bytes, &policy)?;
    let tables = Tables::from_decoded(&decoded);
    let mut facts = Facts::new(&tables);
    if let Some(sink) = sink {
        facts.set_sink(&tables, sink);
    }
    facts.absorb(&tables, &decoded, Mode::Whole);
    facts.prepare_placements(&tables);
    let mut report = finalize(&tables, facts, bytes.len() as u64);
    if let Some((sun, ground)) = stream::render_settings_of(bytes) {
        report.render = extract::render_report(&tables, sun, ground);
    }
    Ok(report)
}
