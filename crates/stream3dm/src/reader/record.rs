//! The parts of an object record: class UUID, class data and attributes.
//!
//! A record (`0x20008070`) holds, in order: the object type (short `0x82000071`), the class
//! wrapper (`0x00027ffa`: class UUID `0x0002fffb`, class data `0x0002fffc`, userdata
//! `0x00027ffd`…, class end `0x80027fff`), optional attributes (`0x02008072`), attribute userdata
//! (`0x02000073`) and history (`0x02008074`), and the end of record (`0x8200007f`).

use super::cursor::Cursor;
use super::{ReadError, Result};
use crate::stream::frame::{TCODE_CRC, TCODE_SHORT, Width};

const TCODE_OBJECT_RECORD: u32 = 0x2000_8070;
const TCODE_OBJECT_TYPE: u32 = 0x8200_0071;
const TCODE_CLASS: u32 = 0x0002_7ffa;
const TCODE_CLASS_UUID: u32 = 0x0002_fffb;
const TCODE_CLASS_DATA: u32 = 0x0002_fffc;
const TCODE_CLASS_USERDATA: u32 = 0x0002_7ffd;
const TCODE_CLASS_END: u32 = 0x8002_7fff;
const TCODE_ATTRIBUTES: u32 = 0x0200_8072;
const TCODE_ATTRIBUTE_USERDATA: u32 = 0x0200_0073;
const TCODE_END_OF_RECORD: u32 = 0x8200_007f;

/// One chunk inside a buffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Chunk<'a> {
    pub typecode: u32,
    /// The value of a short chunk, or the length of a long one.
    pub value: i64,
    /// The body without its trailing CRC (empty for short chunks).
    pub body: &'a [u8],
    /// The stored CRC32, when the chunk has one. It covers only the chunk's own bytes, not
    /// child chunks nested in it, so it can be checked directly only on leaf chunks.
    pub crc: Option<u32>,
}

impl Chunk<'_> {
    /// Whether the stored CRC matches, for a chunk known to have no children.
    pub(crate) fn leaf_crc_ok(&self) -> bool {
        self.crc.is_none_or(|stored| crc32(self.body) == stored)
    }
}

/// Reads the chunk at the cursor.
pub(crate) fn chunk<'a>(c: &mut Cursor<'a>, width: Width) -> Result<Chunk<'a>> {
    let at = c.position();
    let typecode = c.u32()?;
    let short = typecode & TCODE_SHORT != 0;
    let value = match width {
        Width::Four if short => i64::from(c.i32()?),
        Width::Four => i64::from(c.u32()?),
        Width::Eight => c.i64()?,
    };
    if short || value < 0 {
        return Ok(Chunk {
            typecode,
            value,
            body: &[],
            crc: None,
        });
    }
    let len = usize::try_from(value).map_err(|_| ReadError::Invalid {
        at,
        what: "chunk length",
    })?;
    let body = c.bytes(len)?;
    if typecode & TCODE_CRC != 0 {
        if body.len() < 4 {
            return Err(ReadError::Invalid {
                at,
                what: "chunk too short for its CRC",
            });
        }
        let (data, crc) = body.split_at(body.len() - 4);
        let stored = u32::from_le_bytes(crc.try_into().expect("4 bytes"));
        Ok(Chunk {
            typecode,
            value,
            body: data,
            crc: Some(stored),
        })
    } else {
        Ok(Chunk {
            typecode,
            value,
            body,
            crc: None,
        })
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

/// An object serialized with its class wrapper (`0x00027ffa`), at the top of a record or
/// embedded in another object's data.
#[derive(Debug, Clone, Default)]
pub(crate) struct Wrapper<'a> {
    /// Nil for a null object (the wrapper then holds only the UUID chunk).
    pub class: [u8; 16],
    /// The object's own serialization (without CRC).
    pub data: &'a [u8],
    /// Bodies of the userdata records (`0x00027ffd`) attached to the object.
    pub userdata: Vec<&'a [u8]>,
    /// Whether the class UUID chunk's CRC matched (the only leaf chunk checked here).
    pub class_crc_ok: bool,
}

/// Splits the body of a class wrapper chunk.
pub(crate) fn wrapper(body: &[u8], width: Width) -> Result<Wrapper<'_>> {
    let mut c = Cursor::new(body);
    let mut out = Wrapper::default();
    let mut class_seen = false;
    while !c.is_empty() {
        let part = chunk(&mut c, width)?;
        match part.typecode {
            TCODE_CLASS_UUID => {
                out.class = Cursor::new(part.body).uuid()?;
                out.class_crc_ok = part.leaf_crc_ok();
                class_seen = true;
            }
            TCODE_CLASS_DATA => out.data = part.body,
            TCODE_CLASS_USERDATA => out.userdata.push(part.body),
            TCODE_CLASS_END => break,
            _ => {}
        }
    }
    if !class_seen {
        return Err(ReadError::Invalid {
            at: 0,
            what: "object without class UUID",
        });
    }
    Ok(out)
}

/// Reads an embedded object (a class wrapper chunk) at the cursor; `None` for a null object.
pub(crate) fn embedded<'a>(c: &mut Cursor<'a>, width: Width) -> Result<Option<Wrapper<'a>>> {
    let body = c.chunk(width, TCODE_CLASS)?;
    let w = wrapper(body.rest(), width)?;
    Ok((w.class != [0; 16]).then_some(w))
}

/// What an object record holds.
#[derive(Debug, Clone)]
pub(crate) struct RecordParts<'a> {
    pub object_type: Option<i64>,
    pub object: Wrapper<'a>,
    pub attributes: Option<&'a [u8]>,
    pub attribute_userdata: Option<&'a [u8]>,
}

/// Splits an object record (the whole `0x20008070` chunk) into its parts.
pub(crate) fn parts(record: &[u8], width: Width) -> Result<RecordParts<'_>> {
    let mut c = Cursor::new(record);
    let outer = chunk(&mut c, width)?;
    if outer.typecode != TCODE_OBJECT_RECORD {
        return Err(ReadError::Invalid {
            at: 0,
            what: "not an object record",
        });
    }
    let mut inner = Cursor::new(outer.body);
    let mut out = RecordParts {
        object_type: None,
        object: Wrapper::default(),
        attributes: None,
        attribute_userdata: None,
    };
    let mut class_seen = false;
    while !inner.is_empty() {
        let ch = chunk(&mut inner, width)?;
        match ch.typecode {
            TCODE_OBJECT_TYPE => out.object_type = Some(ch.value),
            TCODE_CLASS => {
                out.object = wrapper(ch.body, width)?;
                class_seen = true;
            }
            TCODE_ATTRIBUTES => out.attributes = Some(ch.body),
            TCODE_ATTRIBUTE_USERDATA => out.attribute_userdata = Some(ch.body),
            TCODE_END_OF_RECORD => break,
            _ => {}
        }
    }
    if !class_seen {
        return Err(ReadError::Invalid {
            at: 0,
            what: "record without class UUID",
        });
    }
    Ok(out)
}
