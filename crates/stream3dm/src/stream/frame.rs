//! Push-based reader of the `.3dm` chunk structure.
//!
//! Layout: a 32-byte header, then chunks of `u32` typecode + value (4 bytes below archive 50,
//! 8 bytes from 50; little-endian). Typecodes with bit 31 set are short chunks whose value is
//! inline; other chunks carry `value` bytes of body. Tables are long chunks in a fixed order;
//! the object table holds one child chunk per object record and ends with a short
//! end-of-table chunk. The file ends with a long end-of-file chunk.
//!
//! Everything before the object table is kept as the [`Prefix`]; object-table children are
//! handed to the [`Sink`] one at a time; chunks after the object table are skipped without
//! buffering unless `keep_trailing` is set.

use crate::InspectError;

pub(crate) const MAGIC: &[u8] = b"3D Geometry File Format ";
pub(crate) const HEADER_LEN: usize = 32;
pub(crate) const TCODE_SHORT: u32 = 0x8000_0000;
pub(crate) const TCODE_CRC: u32 = 0x0000_8000;
pub(crate) const TCODE_OBJECT_TABLE: u32 = 0x1000_0013;
pub(crate) const TCODE_END_OF_TABLE: u32 = 0xffff_ffff;
pub(crate) const TCODE_END_OF_FILE: u32 = 0x0000_7fff;
/// User tables after the object table are buffered whole up to this size (larger ones, holding
/// embedded files, are skipped).
pub(crate) const MAX_USER_TABLE: u64 = 64 << 20;
const TCODE_OBJECT_RECORD: u32 = 0x2000_8070;
const TCODE_OBJECT_RECORD_TYPE: u32 = 0x8200_0071;
const TCODE_OPENNURBS_CLASS: u32 = 0x0002_7ffa;
const TCODE_OPENNURBS_CLASS_UUID: u32 = 0x0002_fffb;

/// Class UUID of block instance references (`f9cfb638-b9d4-4340-87e3-c56e7865d96a`) as stored:
/// little-endian `u32`, `u16`, `u16`, then eight bytes.
pub(crate) const INSTANCE_REF_CLASS: [u8; 16] = [
    0x38, 0xb6, 0xcf, 0xf9, 0xd4, 0xb9, 0x40, 0x43, 0x87, 0xe3, 0xc5, 0x6e, 0x78, 0x65, 0xd9, 0x6a,
];

/// The class UUID of an object record, read from its class chunk without decoding the object:
/// record header, optional short object-type chunk, class wrapper header, class UUID chunk.
pub(crate) fn record_class(chunk: &[u8], width: Width) -> Option<[u8; 16]> {
    let h = width.header_len();
    let typecode = |at: usize| {
        chunk
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    if typecode(0)? != TCODE_OBJECT_RECORD {
        return None;
    }
    let mut at = h;
    if typecode(at)? == TCODE_OBJECT_RECORD_TYPE {
        at += h;
    }
    if typecode(at)? != TCODE_OPENNURBS_CLASS {
        return None;
    }
    at += h;
    if typecode(at)? != TCODE_OPENNURBS_CLASS_UUID {
        return None;
    }
    at += h;
    chunk.get(at..at + 16)?.try_into().ok()
}

/// Width of a chunk's value field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Width {
    Four,
    Eight,
}

impl Width {
    pub(crate) fn for_archive(archive: u64) -> Self {
        if archive >= 50 {
            Self::Eight
        } else {
            Self::Four
        }
    }

    pub(crate) fn bytes(self) -> usize {
        match self {
            Self::Four => 4,
            Self::Eight => 8,
        }
    }

    pub(crate) fn header_len(self) -> usize {
        4 + self.bytes()
    }

    /// Writes a chunk value (length or inline value) in this width.
    pub(crate) fn put(self, out: &mut Vec<u8>, value: u64) {
        match self {
            Self::Four => out.extend_from_slice(&(value as u32).to_le_bytes()),
            Self::Eight => out.extend_from_slice(&value.to_le_bytes()),
        }
    }
}

/// A top-level chunk kept in the prefix: its typecode and byte range within `Prefix::bytes`.
#[derive(Debug, Clone)]
pub(crate) struct Span {
    pub typecode: u32,
    pub start: usize,
    pub end: usize,
}

/// The file header and every top-level chunk before the object table.
#[derive(Debug, Clone)]
pub(crate) struct Prefix {
    pub width: Width,
    /// Starts with the 32-byte header.
    pub bytes: Vec<u8>,
    pub spans: Vec<Span>,
    /// Typecode of the object table chunk (its CRC bit decides the rebuilt layout).
    pub object_table_typecode: u32,
}

pub(crate) trait Sink {
    /// The prefix is complete; `table_header` is the object table's chunk header as read.
    fn prefix(&mut self, prefix: &Prefix, table_header: &[u8]) -> Result<(), InspectError>;
    /// One complete object-table child chunk (an object record or an unknown chunk).
    fn record(&mut self, chunk: &[u8]) -> Result<(), InspectError>;
    /// The object table ended.
    fn object_table_end(&mut self) -> Result<(), InspectError>;
    /// Structural bytes consumed without interpretation (end-of-table chunk, table CRC).
    fn raw(&mut self, _bytes: &[u8]) {}
    /// A complete chunk after the object table (only with `keep_trailing`).
    fn trailing(&mut self, _chunk: &[u8]) {}
    /// A complete user table chunk after the object table (Rhino's render settings live in
    /// one), when it is at most [`MAX_USER_TABLE`] bytes.
    fn user_table(&mut self, _chunk: &[u8]) {}
    /// The end-of-file chunk.
    fn eof(&mut self, _chunk: &[u8]) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Header,
    TopLevel,
    ObjectTable,
    Done,
    /// Archive 1 has no tables; the input is buffered whole.
    Legacy,
}

pub(crate) struct Framer {
    pending: Vec<u8>,
    /// Read position in `pending`.
    pos: usize,
    /// Absolute file offset of `pending[0]`.
    base: u64,
    state: State,
    width: Width,
    prefix: Option<Prefix>,
    seen_objects: bool,
    /// Absolute offsets of the object table's children end (before any table CRC) and end.
    children_end: u64,
    table_end: u64,
    /// Bytes still to drop from the input (bodies of skipped chunks).
    skip: u64,
    keep_trailing: bool,
    /// Tables before the object table that are skipped instead of kept in the prefix.
    skip_tables: &'static [u32],
}

/// A parsed chunk header.
struct ChunkHeader {
    typecode: u32,
    /// Body length; 0 for short and body-less chunks.
    body: u64,
    /// The raw value field (length or inline value).
    value: i64,
}

impl Framer {
    pub(crate) fn new(keep_trailing: bool, skip_tables: &'static [u32]) -> Self {
        Self {
            pending: Vec::new(),
            pos: 0,
            base: 0,
            state: State::Header,
            width: Width::Eight,
            prefix: None,
            seen_objects: false,
            children_end: 0,
            table_end: 0,
            skip: 0,
            keep_trailing,
            skip_tables,
        }
    }

    pub(crate) fn is_legacy(&self) -> bool {
        self.state == State::Legacy
    }

    /// The buffered input of an archive 1 file.
    pub(crate) fn legacy_bytes(&self) -> &[u8] {
        &self.pending
    }

    /// Absolute offset of the next unread byte.
    pub(crate) fn offset(&self) -> u64 {
        self.base + self.pos as u64 + self.skip
    }

    pub(crate) fn push(
        &mut self,
        mut data: &[u8],
        sink: &mut impl Sink,
    ) -> Result<(), InspectError> {
        match self.state {
            State::Done => return Ok(()),
            State::Legacy => {
                self.pending.extend_from_slice(data);
                return Ok(());
            }
            _ => {}
        }
        if self.skip > 0 {
            let n = self.skip.min(data.len() as u64) as usize;
            data = &data[n..];
            self.skip -= n as u64;
            self.base += n as u64;
        }
        if data.is_empty() {
            return Ok(());
        }
        // Drop consumed bytes before growing the buffer.
        if self.pos > 0 {
            self.pending.drain(..self.pos);
            self.base += self.pos as u64;
            self.pos = 0;
        }
        self.pending.extend_from_slice(data);
        while self.step(sink)? {}
        Ok(())
    }

    /// Checks that the input ended at the end-of-file chunk.
    pub(crate) fn finish(&self) -> Result<(), InspectError> {
        match self.state {
            State::Done | State::Legacy => Ok(()),
            State::Header if self.pending.is_empty() => Err(InspectError::NotRhinoFile),
            State::Header if self.pending.len() < HEADER_LEN => {
                if MAGIC.starts_with(&self.pending[..self.pending.len().min(MAGIC.len())]) {
                    Err(InspectError::Truncated {
                        offset: self.offset(),
                    })
                } else {
                    Err(InspectError::NotRhinoFile)
                }
            }
            State::TopLevel if self.skip == 0 && self.pos == self.pending.len() => {
                Err(InspectError::Framing {
                    offset: self.offset(),
                    reason: "missing end-of-file chunk".to_string(),
                })
            }
            _ => Err(InspectError::Truncated {
                offset: self.offset(),
            }),
        }
    }

    fn available(&self) -> usize {
        self.pending.len() - self.pos
    }

    fn abs(&self) -> u64 {
        self.base + self.pos as u64
    }

    fn framing(&self, reason: impl Into<String>) -> InspectError {
        InspectError::Framing {
            offset: self.abs(),
            reason: reason.into(),
        }
    }

    /// Parses the chunk header at the read position, if enough bytes are buffered.
    fn header(&self) -> Option<ChunkHeader> {
        let w = self.width;
        if self.available() < w.header_len() {
            return None;
        }
        let b = &self.pending[self.pos..];
        let typecode = u32::from_le_bytes(b[0..4].try_into().expect("4 bytes"));
        let short = typecode & TCODE_SHORT != 0;
        let value = match w {
            Width::Four if short => {
                i64::from(i32::from_le_bytes(b[4..8].try_into().expect("4 bytes")))
            }
            Width::Four => i64::from(u32::from_le_bytes(b[4..8].try_into().expect("4 bytes"))),
            Width::Eight => i64::from_le_bytes(b[4..12].try_into().expect("8 bytes")),
        };
        let body = if short || value < 0 { 0 } else { value as u64 };
        Some(ChunkHeader {
            typecode,
            body,
            value,
        })
    }

    /// Bytes of the whole chunk at the read position, if buffered.
    fn whole(&self, h: &ChunkHeader) -> Option<std::ops::Range<usize>> {
        let total = self.width.header_len() as u64 + h.body;
        (self.available() as u64 >= total).then(|| self.pos..self.pos + total as usize)
    }

    /// Advances by one structural element; returns whether progress was made.
    fn step(&mut self, sink: &mut impl Sink) -> Result<bool, InspectError> {
        match self.state {
            State::Header => self.step_header(),
            State::TopLevel => self.step_top(sink),
            State::ObjectTable => self.step_table(sink),
            State::Done | State::Legacy => Ok(false),
        }
    }

    fn step_header(&mut self) -> Result<bool, InspectError> {
        let n = self.available().min(MAGIC.len());
        if !MAGIC.starts_with(&self.pending[self.pos..self.pos + n]) {
            return Err(InspectError::NotRhinoFile);
        }
        if self.available() < HEADER_LEN {
            return Ok(false);
        }
        let field = &self.pending[self.pos + MAGIC.len()..self.pos + HEADER_LEN];
        let text =
            std::str::from_utf8(field).map_err(|_| self.framing("unreadable version field"))?;
        let archive: u64 = text
            .trim_start_matches(' ')
            .parse()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| self.framing(format!("invalid version field {text:?}")))?;
        if archive == 1 {
            self.state = State::Legacy;
            return Ok(false);
        }
        self.width = Width::for_archive(archive);
        self.prefix = Some(Prefix {
            width: self.width,
            bytes: self.pending[self.pos..self.pos + HEADER_LEN].to_vec(),
            spans: Vec::new(),
            object_table_typecode: TCODE_OBJECT_TABLE,
        });
        self.pos += HEADER_LEN;
        self.state = State::TopLevel;
        Ok(true)
    }

    fn step_top(&mut self, sink: &mut impl Sink) -> Result<bool, InspectError> {
        let Some(h) = self.header() else {
            return Ok(false);
        };
        let header_len = self.width.header_len();
        let short = h.typecode & TCODE_SHORT != 0;

        if h.typecode & !TCODE_CRC == TCODE_OBJECT_TABLE && !short && !self.seen_objects {
            let start = self.abs();
            self.table_end = start + header_len as u64 + h.body;
            let crc = if h.typecode & TCODE_CRC != 0 { 4 } else { 0 };
            if h.body < crc {
                return Err(self.framing("object table too short for its CRC"));
            }
            self.children_end = self.table_end - crc;
            let header = self.pending[self.pos..self.pos + header_len].to_vec();
            let prefix = self.prefix.as_mut().expect("header parsed");
            prefix.object_table_typecode = h.typecode;
            sink.prefix(prefix, &header)?;
            self.seen_objects = true;
            self.pos += header_len;
            self.state = State::ObjectTable;
            return Ok(true);
        }

        if h.typecode == TCODE_END_OF_FILE {
            if short || h.value < self.width.bytes() as i64 {
                return Err(self.framing("invalid end-of-file chunk"));
            }
            let Some(range) = self.whole(&h) else {
                return Ok(false);
            };
            sink.eof(&self.pending[range.clone()]);
            self.pos = range.end;
            self.state = State::Done;
            return Ok(true);
        }

        if !self.seen_objects && !self.skip_tables.contains(&h.typecode) {
            let Some(range) = self.whole(&h) else {
                return Ok(false);
            };
            let prefix = self.prefix.as_mut().expect("header parsed");
            let start = prefix.bytes.len();
            prefix.bytes.extend_from_slice(&self.pending[range.clone()]);
            prefix.spans.push(Span {
                typecode: h.typecode,
                start,
                end: prefix.bytes.len(),
            });
            self.pos = range.end;
        } else if self.seen_objects
            && !short
            && h.typecode & !TCODE_CRC == crate::reader::rdk::TCODE_USER_TABLE
            && h.body <= MAX_USER_TABLE
        {
            let Some(range) = self.whole(&h) else {
                return Ok(false);
            };
            sink.user_table(&self.pending[range.clone()]);
            if self.keep_trailing {
                sink.trailing(&self.pending[range.clone()]);
            }
            self.pos = range.end;
        } else if self.seen_objects && self.keep_trailing {
            let Some(range) = self.whole(&h) else {
                return Ok(false);
            };
            sink.trailing(&self.pending[range.clone()]);
            self.pos = range.end;
        } else {
            self.pos += header_len;
            let in_buffer = (self.available() as u64).min(h.body);
            self.pos += in_buffer as usize;
            self.skip = h.body - in_buffer;
            if self.skip > 0 {
                // The rest of the body arrives later and is dropped in `push`.
                self.base += self.pos as u64;
                self.pending.clear();
                self.pos = 0;
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn step_table(&mut self, sink: &mut impl Sink) -> Result<bool, InspectError> {
        let abs = self.abs();
        if abs == self.children_end {
            let crc = (self.table_end - self.children_end) as usize;
            if self.available() < crc {
                return Ok(false);
            }
            sink.raw(&self.pending[self.pos..self.pos + crc]);
            self.pos += crc;
            sink.object_table_end()?;
            self.state = State::TopLevel;
            return Ok(true);
        }
        if abs > self.children_end {
            return Err(self.framing("object table children overrun the table"));
        }
        let Some(h) = self.header() else {
            return Ok(false);
        };
        let total = self.width.header_len() as u64 + h.body;
        if abs + total > self.children_end {
            return Err(self.framing("chunk ends outside the object table"));
        }
        let Some(range) = self.whole(&h) else {
            return Ok(false);
        };
        if h.typecode == TCODE_END_OF_TABLE {
            sink.raw(&self.pending[range.clone()]);
        } else {
            sink.record(&self.pending[range.clone()])?;
        }
        self.pos = range.end;
        Ok(true)
    }
}
