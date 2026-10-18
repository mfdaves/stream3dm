//! Block definitions from the instance definition table (`0x10000021`).
//!
//! Each record (`0x2000xxxx`) holds an object written as a class chunk (`0x00027ffa`: class
//! UUID `0x0002fffb`, class data `0x0002fffc`, …). `ON_InstanceDefinition` class data has two
//! layouts:
//! - archives up to 50 (and 60 files written that way): a chunk version byte (major 1), the
//!   definition UUID, the member object UUIDs (count, then 16 bytes each), then its name,
//!   description, URL and URL tag as strings;
//! - archive 60 and later: an anonymous chunk (version 1.x) holding the model component
//!   attributes (UUID, name), the definition kind, a unit detail chunk, description, URL and
//!   URL tag, the bounding box, then a flag and the member UUIDs.

use super::cursor::Cursor;
use super::record::chunk;
use super::{Result, uuid_string};
use crate::stream::frame::{TCODE_CRC, Width};

const TCODE_INSTANCE_DEFINITION_TABLE: u32 = 0x1000_0021;
const TCODE_ANONYMOUS: u32 = 0x4000_8000;
const TCODE_TABLE_RECORD: u32 = 0x2000_0000;
const TCODE_CLASS: u32 = 0x0002_7ffa;
const TCODE_CLASS_UUID: u32 = 0x0002_fffb;
const TCODE_CLASS_DATA: u32 = 0x0002_fffc;
/// `ON_InstanceDefinition`'s class id.
const INSTANCE_DEFINITION_CLASS: &str = "26f8bff6-2618-417f-a158-153d64a94989";

/// One block definition: its UUID, name and member object UUIDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Definition {
    pub uuid: String,
    pub name: String,
    pub members: Vec<String>,
}

/// The definitions in the instance definition table chunk `bytes`. Records that cannot be
/// read are skipped.
pub(crate) fn read_table(bytes: &[u8], width: Width) -> Vec<Definition> {
    let mut c = Cursor::new(bytes);
    let Ok(table) = chunk(&mut c, width) else {
        return Vec::new();
    };
    if table.typecode & !TCODE_CRC != TCODE_INSTANCE_DEFINITION_TABLE {
        return Vec::new();
    }
    let mut records = Cursor::new(table.body);
    let mut out = Vec::new();
    while records.remaining() > 0 {
        let Ok(record) = chunk(&mut records, width) else {
            break;
        };
        if record.typecode & 0xf000_0000 != TCODE_TABLE_RECORD {
            continue;
        }
        if let Ok(Some(d)) = read_record(record.body, width) {
            out.push(d);
        }
    }
    out
}

fn read_record(body: &[u8], width: Width) -> Result<Option<Definition>> {
    let mut c = Cursor::new(body);
    while c.remaining() > 0 {
        let class = chunk(&mut c, width)?;
        if class.typecode != TCODE_CLASS {
            continue;
        }
        let mut inner = Cursor::new(class.body);
        let mut uuid = None;
        while inner.remaining() > 0 {
            let part = chunk(&mut inner, width)?;
            match part.typecode {
                TCODE_CLASS_UUID => uuid = Some(uuid_string(&Cursor::new(part.body).uuid()?)),
                TCODE_CLASS_DATA if uuid.as_deref() == Some(INSTANCE_DEFINITION_CLASS) => {
                    return read_data(part.body, width);
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

fn read_data(data: &[u8], width: Width) -> Result<Option<Definition>> {
    if data.first() == Some(&0) {
        return read_v6(data, width).map(Some);
    }
    let mut c = Cursor::new(data);
    let version = c.u8()?;
    if version >> 4 != 1 {
        return Ok(None);
    }
    let uuid = uuid_string(&c.uuid()?);
    let members = members(&mut c)?;
    let name = c.wstring()?;
    Ok(Some(Definition {
        uuid,
        name,
        members,
    }))
}

fn read_v6(data: &[u8], width: Width) -> Result<Definition> {
    let mut outer = Cursor::new(data);
    let mut c = outer.chunk(width, TCODE_ANONYMOUS)?;
    if c.i32()? != 1 {
        return Err(super::ReadError::Unsupported("instance definition version"));
    }
    c.i32()?;
    let component = super::document::component(&mut c, width)?;
    c.u32()?; // definition kind
    c.skip_chunk(width)?; // unit detail
    for _ in 0..3 {
        c.wstring()?; // description, URL, URL tag
    }
    c.skip(48)?; // bounding box
    let members = if c.bool8()? {
        members(&mut c)?
    } else {
        Vec::new()
    };
    Ok(Definition {
        uuid: uuid_string(&component.uuid),
        name: component.name,
        members,
    })
}

fn members(c: &mut Cursor) -> Result<Vec<String>> {
    let count = c.count(16)?;
    (0..count).map(|_| Ok(uuid_string(&c.uuid()?))).collect()
}
