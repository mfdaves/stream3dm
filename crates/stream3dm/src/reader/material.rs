//! Materials (`ON_Material`) from the material table: index, name, diffuse colour, texture
//! file names, and the physically based parameters Rhino 7 and later attach as userdata.
//!
//! Three layouts of the class data:
//! - archives 2 and 3: a version byte (major 1), colours, shine and transparency, three
//!   legacy textures (bitmap, bump, environment: path, mode, index, bump scale), then index,
//!   plug-in UUID, an unused string and the name;
//! - archives 4 and 50 (first byte `0x20`): a chunk with version, UUID, index and name, then
//!   the plug-in UUID, six colours, four reals and a texture array;
//! - archive 60 and later (first byte 0, a chunk typecode): an anonymous chunk holding the
//!   model component attributes, then the same fields as the previous layout.
//!
//! Texture arrays hold `ON_Texture` objects; from version 1.1 each carries a file reference
//! (anonymous chunk: full path, relative path, content hash).

use super::cursor::Cursor;
use super::document::{component, hex_rgba};
use super::record::{Wrapper, embedded};
use super::{ReadError, Result, uuid_string};
use crate::native::{FileRefRec, MaterialRec, PbrRec, TextureRec};
use crate::stream::frame::Width;

const TCODE_ANONYMOUS: u32 = 0x4000_8000;
const TCODE_USERDATA_HEADER: u32 = 0x0002_fff9;
const TEXTURE_CLASS: &str = "d6ff106d-329b-4f29-97e2-fd282a618020";
/// `ON_PhysicallyBasedMaterial` userdata: class and item id.
const PBR_USERDATA: &str = "5694e1ac-40e6-44f4-9ca9-3b6d0e8c4440";
/// openNURBS 6, the only application id the PBR userdata is read for (besides none).
const OPENNURBS6_APPLICATION: &str = "7b0b585d-7a31-45d0-925e-bdd7ddf3e4e3";

pub(crate) fn read(w: &Wrapper, width: Width, archive: u32) -> Result<MaterialRec> {
    let mut m = if matches!(archive, 2 | 3) {
        legacy(w.data)?
    } else {
        current(w.data, width)?
    };
    m.physically_based = w
        .userdata
        .iter()
        .find_map(|body| physically_based(body, width).ok().flatten());
    Ok(m)
}

/// Archives 2 and 3.
fn legacy(data: &[u8]) -> Result<MaterialRec> {
    let mut c = Cursor::new(data);
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported("material version"));
    }
    c.skip(4)?; // ambient
    let diffuse = hex_rgba(c.bytes(4)?);
    c.skip(4 + 4)?; // emission, specular
    c.skip(8 + 8 + 4 + 4 + 20)?; // shine, transparency, unused, wire colour, unused
    for bump in [false, true, false] {
        c.wstring()?; // path (no file reference in these archives)
        c.skip(4 + 4)?;
        if bump {
            c.skip(8)?;
        }
    }
    let index = c.i32()?;
    c.uuid()?; // plug-in
    c.wstring()?;
    let name = c.wstring()?;
    Ok(MaterialRec {
        archive_index: Some(i64::from(index)),
        name: Some(name),
        diffuse: Some(diffuse),
        physically_based: None,
        textures: Vec::new(),
    })
}

/// Archives 4 and later.
fn current(data: &[u8], width: Width) -> Result<MaterialRec> {
    let mut outer = Cursor::new(data);
    let (mut c, index, name) = if data.first() == Some(&0) {
        let mut c = outer.chunk(width, TCODE_ANONYMOUS)?;
        if c.i32()? != 1 {
            return Err(ReadError::Unsupported("material version"));
        }
        c.i32()?;
        let comp = component(&mut c, width)?;
        (c, comp.index, comp.name)
    } else {
        if outer.u8()? != 0x20 {
            return Err(ReadError::Unsupported("material outer version"));
        }
        let at = outer.position();
        let (typecode, value) = outer.chunk_header(width)?;
        let mut c = outer.long_body(at, typecode, value)?;
        if c.i32()? != 1 {
            return Err(ReadError::Unsupported("material version"));
        }
        c.i32()?;
        c.uuid()?;
        let index = c.i32()?;
        let name = c.wstring()?;
        (c, Some(index), name)
    };
    c.uuid()?; // plug-in
    c.skip(4)?; // ambient
    let diffuse = hex_rgba(c.bytes(4)?);
    c.skip(4 * 4)?; // emission, specular, reflection, transparent
    c.skip(4 * 8)?; // index of refraction, reflectivity, shine, transparency
    let mut m = MaterialRec {
        archive_index: index.map(i64::from),
        name: Some(name),
        diffuse: Some(diffuse),
        physically_based: None,
        textures: Vec::new(),
    };
    // A texture array that cannot be read leaves the material without textures.
    if let Ok(textures) = textures(&mut c, width) {
        m.textures = textures;
    }
    Ok(m)
}

/// A texture array (anonymous chunk, version 1.x, count, `ON_Texture` objects).
fn textures(c: &mut Cursor, width: Width) -> Result<Vec<TextureRec>> {
    let mut array = c.chunk(width, TCODE_ANONYMOUS)?;
    if array.i32()? != 1 {
        return Err(ReadError::Unsupported("texture array version"));
    }
    array.i32()?;
    let count = array.count(1)?;
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let Some(w) = embedded(&mut array, width)? else {
            continue;
        };
        if uuid_string(&w.class) != TEXTURE_CLASS {
            return Err(ReadError::Invalid {
                at: array.position(),
                what: "texture class",
            });
        }
        out.push(TextureRec {
            file_reference: texture_file(w.data, width)?,
        });
    }
    Ok(out)
}

/// `ON_Texture`: anonymous chunk (version 1.x) of fixed fields, then the file reference from
/// version 1.1.
fn texture_file(data: &[u8], width: Width) -> Result<Option<FileRefRec>> {
    let mut outer = Cursor::new(data);
    let mut c = outer.chunk(width, TCODE_ANONYMOUS)?;
    if c.i32()? != 1 {
        return Err(ReadError::Unsupported("texture version"));
    }
    let minor = c.i32()?;
    c.uuid()?;
    c.u32()?; // mapping channel
    c.wstring()?; // legacy path
    c.bool8()?;
    c.skip(4 * 4 + 3 * 4)?; // type, mode, filters, wrap
    c.skip(16 * 8)?; // UVW transform
    c.skip(4 + 4)?; // border and transparent colours
    c.uuid()?; // transparency texture
    c.skip(2 * 8 + 5 * 8 + 4 + 4 * 8 + 4)?; // bump scale, alpha and RGB blending, blend order
    if minor < 1 {
        return Ok(None);
    }
    let mut r = c.chunk(width, TCODE_ANONYMOUS)?;
    if r.i32()? != 1 {
        return Err(ReadError::Unsupported("file reference version"));
    }
    r.i32()?;
    let full_path = r.wstring()?;
    let relative_path = r.wstring()?;
    Ok(Some(FileRefRec {
        full_path: Some(full_path),
        relative_path: Some(relative_path),
    }))
}

/// The physically based parameters from one userdata record body, when it is
/// `ON_PhysicallyBasedMaterial` userdata (payload: anonymous chunk of version 1.1 or 1.2).
fn physically_based(body: &[u8], width: Width) -> Result<Option<PbrRec>> {
    let mut c = Cursor::new(body);
    let version = c.u8()?;
    let (class, item, application) = match version >> 4 {
        1 => {
            let class = c.uuid()?;
            let item = c.uuid()?;
            c.skip(4 + 128)?;
            (class, item, None)
        }
        2 => {
            let mut h = c.chunk(width, TCODE_USERDATA_HEADER)?;
            let class = h.uuid()?;
            let item = h.uuid()?;
            h.skip(4 + 128)?;
            let application = if version & 0x0f >= 1 {
                Some(h.uuid()?)
            } else {
                None
            };
            (class, item, application)
        }
        _ => return Ok(None),
    };
    if uuid_string(&class) != PBR_USERDATA
        || uuid_string(&item) != PBR_USERDATA
        || application.is_some_and(|a| uuid_string(&a) != OPENNURBS6_APPLICATION)
    {
        return Ok(None);
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
    let mut payload = c.long_body(at, 0, value)?;
    let mut p = payload.chunk(width, TCODE_ANONYMOUS)?;
    let (major, minor) = (p.i32()?, p.i32()?);
    if major != 1 || !matches!(minor, 1 | 2) {
        return Err(ReadError::Unsupported("physically based material version"));
    }
    let base = colour(&mut p)?;
    p.i32()?; // BRDF
    p.f64()?; // subsurface
    colour(&mut p)?;
    p.f64()?; // subsurface scattering radius
    let metallic = p.f64()?;
    p.skip(2 * 8)?; // specular, specular tint
    let roughness = p.f64()?;
    p.skip(7 * 8)?; // anisotropy, sheen, clearcoat, opacity IOR
    let opacity = p.f64()?;
    if !(base.iter().all(|v| v.is_finite())
        && metallic.is_finite()
        && roughness.is_finite()
        && opacity.is_finite())
    {
        return Err(ReadError::Invalid {
            at: 0,
            what: "physically based value",
        });
    }
    Ok(Some(PbrRec {
        base_color: Some(base.to_vec()),
        metallic: Some(metallic),
        roughness: Some(roughness),
        opacity: Some(opacity),
    }))
}

/// Four `f32` colour components.
fn colour(p: &mut Cursor) -> Result<[f64; 4]> {
    let mut out = [0.0; 4];
    for v in &mut out {
        *v = f64::from(f32::from_bits(p.u32()?));
    }
    Ok(out)
}
