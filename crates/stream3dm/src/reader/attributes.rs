//! Object attributes (`ON_3dmObjectAttributes`) and the user strings stored as attribute
//! userdata.
//!
//! Two layouts, chosen by the major nibble of the leading version byte:
//! - major 1 (archives 2–4): fixed fields, then blocks gated by the minor version;
//! - major 2 (archives 50+): object UUID and layer index, then `u8` item ids in ascending
//!   order, each followed by its value, ending with id 0. An unknown, out-of-order or
//!   too-new item ends the parse, keeping what was read (values carry no length).

use super::cursor::Cursor;
use super::{ReadError, Result};
use crate::stream::frame::Width;

const TCODE_ANONYMOUS: u32 = 0x4000_8000;
const TCODE_USERDATA: u32 = 0x0002_7ffd;
const TCODE_USERDATA_HEADER: u32 = 0x0002_fff9;
const TCODE_CLASS_END: u32 = 0x8002_7fff;

/// `ON_UserStringList` class id (`ce28de29-f4c5-4faa-a50a-c3a6849b6329`) as stored.
const USER_STRING_LIST: [u8; 16] = [
    0x29, 0xde, 0x28, 0xce, 0xc5, 0xf4, 0xaa, 0x4f, 0xa5, 0x0a, 0xc3, 0xa6, 0x84, 0x9b, 0x63, 0x29,
];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Attributes {
    pub uuid: [u8; 16],
    pub layer: i32,
    pub name: String,
    pub material_index: i32,
    pub material_source: u8,
    /// The stored visibility flag (item 11, or derived from the mode in old legacy records).
    pub visible: bool,
    /// `ON::object_mode` in the low nibble.
    pub mode: u8,
    pub groups: Vec<i32>,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            uuid: [0; 16],
            layer: -1,
            name: String::new(),
            material_index: -1,
            material_source: 0,
            visible: true,
            mode: 0,
            groups: Vec::new(),
        }
    }
}

/// Reads the body of an attributes chunk (`0x02008072`, CRC excluded).
pub(crate) fn read(body: &[u8], width: Width) -> Result<Attributes> {
    let mut c = Cursor::new(body);
    let version = c.u8()?;
    let mut a = Attributes::default();
    match version >> 4 {
        2 => tagged(&mut c, version & 0x0f, width, &mut a)?,
        1 => legacy(&mut c, version & 0x0f, width, &mut a)?,
        _ => return Err(ReadError::Unsupported("attributes version")),
    }
    Ok(a)
}

/// Lowest chunk minor version at which each tagged item may appear.
fn minimum_minor(id: u8) -> u8 {
    match id {
        1..=21 => 0,
        22 => 1,
        23..=26 => 2,
        27..=28 => 3,
        29..=32 => 4,
        33 => 5,
        34..=35 => 6,
        36 => 8,
        37 => 9,
        38 => 10,
        39 => 11,
        40 => 12,
        _ => 13,
    }
}

fn tagged(c: &mut Cursor, minor: u8, width: Width, a: &mut Attributes) -> Result<()> {
    a.uuid = c.uuid()?;
    a.layer = c.i32()?;
    let mut last = 0u8;
    loop {
        let id = c.u8()?;
        if id == 0 || id <= last || id > 41 || minor < minimum_minor(id) {
            return Ok(());
        }
        last = id;
        match id {
            1 => a.name = c.wstring()?,
            2 => {
                c.wstring()?;
            }
            3 | 10 | 22 | 30 => {
                c.i32()?;
            }
            4 => a.material_index = c.i32()?,
            5 | 38 | 39 => c.skip_chunk(width)?,
            6 | 7 | 34 => c.skip(4)?,
            8 | 31 | 32 | 33 => c.skip(8)?,
            11 => a.visible = c.bool8()?,
            12 => a.mode = c.u8()?,
            16 => a.material_source = c.u8()?,
            9 | 13 | 14 | 15 | 17 | 19 | 23..=27 | 29 | 35 | 37 | 40 | 41 => {
                c.u8()?;
            }
            18 => a.groups = c.i32_array()?,
            20 => c.skip(16)?,
            21 => c.skip_array(32)?,
            28 => {
                c.u8()?;
                c.skip_chunk(width)?;
            }
            36 => c.skip(128)?,
            _ => unreachable!("ids above 41 end the parse"),
        }
    }
}

fn legacy(c: &mut Cursor, minor: u8, width: Width, a: &mut Attributes) -> Result<()> {
    a.uuid = c.uuid()?;
    a.layer = c.i32()?;
    a.material_index = c.i32()?;
    c.skip(4)?; // colour
    c.skip(2 + 2 + 8 + 8)?; // obsolete line style, thickness, scale
    c.i32()?; // wire density
    a.mode = c.u8()?;
    c.u8()?; // colour source
    c.u8()?; // linetype source
    a.material_source = c.u8()?;
    a.name = c.wstring()?;
    c.wstring()?; // URL
    a.visible = a.mode & 0x0f != 1;
    if minor >= 1 {
        a.groups = c.i32_array()?;
    }
    if minor >= 2 {
        a.visible = c.bool8()?;
    }
    if minor >= 3 {
        c.skip_array(32)?; // display material references
    }
    if minor >= 4 {
        c.skip(4 + 1 + 4 + 1 + 8)?; // decoration, plot colour and weight
    }
    if minor >= 5 {
        c.i32()?; // linetype index
    }
    if minor >= 6 {
        c.u8()?; // space
        c.skip_array(32)?;
    }
    if minor >= 7 {
        c.skip_chunk(width)?; // rendering attributes
    }
    Ok(())
}

/// User strings from an attribute userdata chunk body (`0x02000073`, no CRC): the
/// `ON_UserStringList` records among its userdata records, in order.
pub(crate) fn user_strings(body: &[u8], width: Width) -> Result<Vec<(String, String)>> {
    let mut c = Cursor::new(body);
    let mut out = Vec::new();
    while !c.is_empty() {
        let at = c.position();
        let (typecode, value) = c.chunk_header(width)?;
        if typecode == TCODE_CLASS_END {
            break;
        }
        if typecode & crate::stream::frame::TCODE_SHORT != 0 {
            continue;
        }
        let mut record = c.long_body(at, typecode, value)?;
        if typecode == TCODE_USERDATA {
            userdata_strings(&mut record, width, &mut out)?;
        }
    }
    Ok(out)
}

/// One userdata record: header (inline for major 1, a chunk for major 2), then the payload
/// in an anonymous chunk.
fn userdata_strings(c: &mut Cursor, width: Width, out: &mut Vec<(String, String)>) -> Result<()> {
    let version = c.u8()?;
    let class = match version >> 4 {
        2 => c.chunk(width, TCODE_USERDATA_HEADER)?.uuid()?,
        1 => {
            let class = c.uuid()?;
            c.skip(16 + 4 + 128)?;
            class
        }
        _ => return Ok(()),
    };
    if class != USER_STRING_LIST {
        return Ok(());
    }
    // The outer wrapper may lack its CRC (userdata saved as goo), so take its whole length.
    let at = c.position();
    let (typecode, value) = c.chunk_header(width)?;
    if typecode != TCODE_ANONYMOUS {
        return Err(ReadError::Invalid {
            at,
            what: "userdata payload",
        });
    }
    let mut outer = c.long_body(at, 0, value)?;
    let mut list = outer.chunk(width, TCODE_ANONYMOUS)?;
    if list.i32()? != 1 {
        return Err(ReadError::Unsupported("user string list version"));
    }
    list.i32()?;
    let count = list.i32()?.max(0);
    for _ in 0..count {
        let mut entry = list.chunk(width, TCODE_ANONYMOUS)?;
        if entry.i32()? != 1 {
            return Err(ReadError::Unsupported("user string version"));
        }
        entry.i32()?;
        let key = entry.wstring()?;
        let value = entry.wstring()?;
        if !key.eq_ignore_ascii_case("$temp_object$") {
            out.push((key, value));
        }
    }
    Ok(())
}
