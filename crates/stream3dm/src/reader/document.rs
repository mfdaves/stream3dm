//! Document tables, read from the prefix (the header and every table before the object
//! table): units and tolerances, the saving application, the earth anchor, layers, materials,
//! views, groups, block definitions and lights.
//!
//! Tables are long chunks of records (`0x2000xxxx`). Layer, material, light, group and
//! definition records hold one object in its class wrapper (`0x00027ffa`); the properties and
//! settings tables hold plain values, one record per setting. Lengths in views are converted
//! to millimetres with the file's unit scale, or [`ASSUMED_MM_PER_UNIT`] when it has none, as
//! the object reader does.

use super::cursor::Cursor;
use super::record::{Wrapper, chunk, wrapper};
use super::{Result, idef, material, uuid_string, view};
use crate::extract::ASSUMED_MM_PER_UNIT;
use crate::native::{DefinitionRec, GroupRec, LayerRec, MaterialRec, ViewRec};
use crate::stream::frame::{Prefix, TCODE_CRC, Width};

const TCODE_MATERIAL_TABLE: u32 = 0x1000_0010;
const TCODE_LAYER_TABLE: u32 = 0x1000_0011;
const TCODE_LIGHT_TABLE: u32 = 0x1000_0012;
const TCODE_PROPERTIES_TABLE: u32 = 0x1000_0014;
const TCODE_SETTINGS_TABLE: u32 = 0x1000_0015;
const TCODE_GROUP_TABLE: u32 = 0x1000_0018;
const TCODE_INSTANCE_DEFINITION_TABLE: u32 = 0x1000_0021;
const TCODE_APPLICATION: u32 = 0x2000_8024;
const TCODE_UNITS: u32 = 0x2000_8031;
const TCODE_NAMED_VIEWS: u32 = 0x2000_8036;
const TCODE_SETTINGS_ATTRIBUTES: u32 = 0x2000_8134;
const TCODE_LIGHT_RECORD_ATTRIBUTES: u32 = 0x0200_0061;
const TCODE_ACTIVE_VIEWS: u32 = 0x2000_8037;
const TCODE_CLASS: u32 = 0x0002_7ffa;
const TCODE_ANONYMOUS: u32 = 0x4000_8000;
const TCODE_MODEL_ATTRIBUTES: u32 = 0x4000_8002;

const LAYER_CLASS: &str = "95809813-e985-11d3-bfe5-0010830122f0";
const MATERIAL_CLASS: &str = "60b5dbbc-e660-11d3-bfe4-0010830122f0";
const LIGHT_CLASS: &str = "85a08513-f383-11d3-bfe7-0010830122f0";
const GROUP_CLASS: &str = "721d9f97-3645-44c4-8be6-b2cf697d25ce";
/// Writers up to this openNURBS version stored no layer parent link.
const LAYER_PARENT_STAMP: u32 = 200_505_110;
/// `ON::LengthUnitSystem`: no unit, custom unit, unset.
const UNIT_NONE: i32 = 0;
const UNIT_CUSTOM: i32 = 11;
const UNIT_UNSET: i32 = 255;
/// Millimetres per unit for `ON::LengthUnitSystem` codes 1..=25 (11, custom, has its own).
const UNIT_MM: [f64; 26] = [
    0.0,
    0.001,
    1.0,
    10.0,
    1000.0,
    1_000_000.0,
    0.000_025_4,
    0.0254,
    25.4,
    304.8,
    1_609_344.0,
    0.0,
    0.000_000_1,
    0.000_001,
    100.0,
    10_000.0,
    100_000.0,
    1_000_000_000.0,
    1_000_000_000_000.0,
    914.4,
    0.352_777_777_777_777_8,
    4.233_333_333_333_333,
    1_852_000.0,
    149_597_870_000_000.0,
    9.460_730_472_580_8e18,
    3.085_677_58e19,
];

/// Everything the report needs from outside the object table.
#[derive(Debug, Default)]
pub(crate) struct Document {
    pub archive_version: Option<u32>,
    pub application: Option<String>,
    pub application_details: Option<String>,
    /// `standard:N` (`ON::LengthUnitSystem` N), `custom:<name>`, `none` or `unset`.
    pub unit_system: Option<String>,
    pub mm_per_unit: Option<f64>,
    pub absolute_tolerance_mm: Option<f64>,
    pub layers: Vec<LayerRec>,
    pub materials: Vec<MaterialRec>,
    pub views: Vec<ViewRec>,
    pub groups: Vec<GroupRec>,
    pub definitions: Vec<DefinitionRec>,
    pub lights: usize,
    /// The lights read in full (`lights` counts every light record).
    pub light_list: Vec<LightRec>,
    pub earth_anchor: Option<EarthAnchorRec>,
    /// Records that could not be read.
    pub unparsed: usize,
}

/// An `ON_Light` and its record's attributes, in model units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LightRec {
    pub name: String,
    /// `ON::light_style`.
    pub style: i32,
    pub on: bool,
    pub intensity: f64,
    pub watts: f64,
    pub diffuse: [u8; 3],
    pub direction: [f64; 3],
    pub location: [f64; 3],
    pub spot_angle_deg: f64,
    pub length: [f64; 3],
    pub width: [f64; 3],
    pub hotspot: Option<f64>,
    pub layer: i32,
    pub visible: bool,
}

/// `ON_EarthAnchorPoint`, when the file sets a latitude and longitude.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EarthAnchorRec {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    pub north: [f64; 3],
    pub east: [f64; 3],
}

/// Reads the document tables of `prefix`. A record that cannot be read is counted in
/// `unparsed` and skipped; a table that cannot be framed ends where it broke.
pub(crate) fn read(prefix: &Prefix) -> Document {
    let width = prefix.width;
    let archive = super::archive_version(&prefix.bytes);
    let stamp = super::writer_stamp(prefix);
    let mut doc = Document {
        archive_version: Some(archive),
        ..Document::default()
    };
    let tables: Vec<(u32, &[u8])> = prefix
        .spans
        .iter()
        .filter_map(|s| Some((s.typecode & !TCODE_CRC, prefix.bytes.get(s.start..s.end)?)))
        .collect();
    let mut unparsed = 0;
    // Units first: views need the scale. (Framing failures are counted in the second pass.)
    for &(typecode, bytes) in &tables {
        if typecode == TCODE_SETTINGS_TABLE {
            for_each_record(bytes, width, &mut 0, |t, body, _| {
                if t == TCODE_UNITS && units(body, &mut doc).is_err() {
                    unparsed += 1;
                }
            });
        }
    }
    let scale = doc.mm_per_unit.unwrap_or(ASSUMED_MM_PER_UNIT);
    for &(typecode, bytes) in &tables {
        match typecode {
            TCODE_PROPERTIES_TABLE => for_each_record(bytes, width, &mut unparsed, |t, body, u| {
                if t == TCODE_APPLICATION && application(body, &mut doc).is_err() {
                    *u += 1;
                }
            }),
            TCODE_SETTINGS_TABLE => for_each_record(bytes, width, &mut unparsed, |t, body, u| {
                if t == TCODE_SETTINGS_ATTRIBUTES {
                    match earth_anchor(body, width) {
                        Ok(anchor) => doc.earth_anchor = anchor,
                        Err(_) => *u += 1,
                    }
                    return;
                }
                let kind = match t {
                    TCODE_NAMED_VIEWS => "named",
                    TCODE_ACTIVE_VIEWS => "active",
                    _ => return,
                };
                let (views, failed) = view::read_list(body, width, kind, scale);
                doc.views.extend(views);
                *u += failed;
            }),
            TCODE_LAYER_TABLE => {
                for_each_object(bytes, width, &mut unparsed, |w, u| {
                    match class_is(w, LAYER_CLASS).then(|| layer(w.data, stamp)) {
                        Some(Ok(l)) => doc.layers.push(l),
                        Some(Err(_)) => *u += 1,
                        None => {}
                    }
                })
            }
            TCODE_MATERIAL_TABLE => for_each_object(bytes, width, &mut unparsed, |w, u| {
                match class_is(w, MATERIAL_CLASS).then(|| material::read(w, width, archive)) {
                    Some(Ok(m)) => doc.materials.push(m),
                    Some(Err(_)) => *u += 1,
                    None => {}
                }
            }),
            TCODE_GROUP_TABLE => {
                for_each_object(bytes, width, &mut unparsed, |w, u| {
                    match class_is(w, GROUP_CLASS).then(|| group(w.data)) {
                        Some(Ok(g)) => doc.groups.push(g),
                        Some(Err(_)) => *u += 1,
                        None => {}
                    }
                })
            }
            TCODE_LIGHT_TABLE => {
                for_each_record(bytes, width, &mut unparsed, |typecode, body, u| {
                    if typecode & 0xf000_0000 != 0x2000_0000 {
                        return;
                    }
                    let Ok(Some(w)) = class_object(body, width) else {
                        return;
                    };
                    if !class_is(&w, LIGHT_CLASS) {
                        return;
                    }
                    doc.lights += 1;
                    match light(w.data) {
                        Ok(mut l) => {
                            if let Some(a) = light_attributes(body, width) {
                                l.layer = a.layer;
                                l.visible = a.visible;
                            }
                            doc.light_list.push(l);
                        }
                        Err(_) => *u += 1,
                    }
                })
            }
            TCODE_INSTANCE_DEFINITION_TABLE => {
                for d in idef::read_table(bytes, width) {
                    doc.definitions.push(DefinitionRec {
                        name: Some(d.name),
                        source_uuid: Some(d.uuid),
                        member_object_ids: d.members,
                    });
                }
            }
            _ => {}
        }
    }
    doc.unparsed = unparsed;
    doc
}

/// Calls `f` with the typecode and body of each record of the table chunk `bytes`.
fn for_each_record(
    bytes: &[u8],
    width: Width,
    unparsed: &mut usize,
    mut f: impl FnMut(u32, &[u8], &mut usize),
) {
    let mut c = Cursor::new(bytes);
    let Ok(table) = chunk(&mut c, width) else {
        *unparsed += 1;
        return;
    };
    let mut records = Cursor::new(table.body);
    while !records.is_empty() {
        let Ok(record) = chunk(&mut records, width) else {
            *unparsed += 1;
            return;
        };
        f(record.typecode, record.body, unparsed);
    }
}

/// Calls `f` with the object of each record of the table chunk `bytes` (records without a
/// readable object are counted in `unparsed`).
fn for_each_object(
    bytes: &[u8],
    width: Width,
    unparsed: &mut usize,
    mut f: impl FnMut(&Wrapper, &mut usize),
) {
    for_each_record(bytes, width, unparsed, |typecode, body, u| {
        if typecode & 0xf000_0000 != 0x2000_0000 {
            return;
        }
        match class_object(body, width) {
            Ok(Some(w)) => f(&w, u),
            Ok(None) => {}
            Err(_) => *u += 1,
        }
    });
}

/// The object in a table record: its first class wrapper chunk.
pub(crate) fn class_object(body: &[u8], width: Width) -> Result<Option<Wrapper<'_>>> {
    let mut c = Cursor::new(body);
    while !c.is_empty() {
        let part = chunk(&mut c, width)?;
        if part.typecode == TCODE_CLASS {
            return wrapper(part.body, width).map(Some);
        }
    }
    Ok(None)
}

fn class_is(w: &Wrapper, class: &str) -> bool {
    uuid_string(&w.class) == class
}

/// `ON_3dmUnitsAndTolerances`: version (1, or 100–199), unit code, absolute tolerance, the
/// angular and relative tolerances (in that order from version 100), display mode and
/// precision (101), custom unit scale in metres and name (102).
/// The units and tolerances record (also Rhino 1's units chunk, which has its version-1 layout).
pub(crate) fn units(body: &[u8], doc: &mut Document) -> Result<()> {
    let mut c = Cursor::new(body);
    let version = c.i32()?;
    if version != 1 && !(100..200).contains(&version) {
        return Err(super::ReadError::Unsupported("units version"));
    }
    let code = c.i32()?;
    let absolute = c.f64()?;
    c.skip(16)?;
    if version >= 101 {
        c.skip(8)?;
    }
    let custom = if version >= 102 {
        let metres = c.f64()?;
        Some((metres, c.wstring()?))
    } else {
        None
    };
    let (system, mm) = match code {
        UNIT_NONE => ("none".to_string(), None),
        UNIT_UNSET => ("unset".to_string(), None),
        UNIT_CUSTOM => {
            let (metres, name) = custom.ok_or(super::ReadError::Invalid {
                at: 0,
                what: "custom unit without scale",
            })?;
            let mm = metres * 1000.0;
            if !(mm.is_finite() && mm > 0.0) {
                return Err(super::ReadError::Invalid {
                    at: 0,
                    what: "custom unit scale",
                });
            }
            (format!("custom:{name}"), Some(mm))
        }
        1..=25 => (format!("standard:{code}"), Some(UNIT_MM[code as usize])),
        _ => {
            return Err(super::ReadError::Invalid {
                at: 4,
                what: "unit code",
            });
        }
    };
    doc.unit_system = Some(system);
    doc.mm_per_unit = mm;
    doc.absolute_tolerance_mm = mm
        .map(|mm| absolute * mm)
        .filter(|t| t.is_finite() && *t > 0.0);
    Ok(())
}

/// `ON_Light`: version byte (major 1); on, style, intensity, watts, ambient, diffuse and
/// specular colours (4 bytes each), direction, location, spot angle (degrees), spot exponent,
/// attenuation, shadow intensity, index, id, name; length and width from minor 1; hotspot from
/// minor 2.
fn light(data: &[u8]) -> Result<LightRec> {
    let mut c = Cursor::new(data);
    let version = c.u8()?;
    if version >> 4 != 1 {
        return Err(super::ReadError::Unsupported("light version"));
    }
    let on = c.i32()? != 0;
    let style = c.i32()?;
    let intensity = c.f64()?;
    let watts = c.f64()?;
    c.skip(4)?; // ambient
    let d = c.bytes(4)?;
    let diffuse = [d[0], d[1], d[2]];
    c.skip(4)?; // specular
    let direction = c.point3()?;
    let location = c.point3()?;
    let spot_angle_deg = c.f64()?;
    let _spot_exponent = c.f64()?;
    c.skip(24 + 8 + 4 + 16)?; // attenuation, shadow intensity, index, id
    let name = c.wstring()?;
    let (mut length, mut width) = ([0.0; 3], [0.0; 3]);
    if version & 0x0f >= 1 {
        length = c.point3()?;
        width = c.point3()?;
    }
    let hotspot = if version & 0x0f >= 2 {
        Some(c.f64()?)
    } else {
        None
    };
    let finite = |v: &[f64]| v.iter().all(|x| x.is_finite());
    if !finite(&direction)
        || !finite(&location)
        || !finite(&length)
        || !finite(&width)
        || !intensity.is_finite()
    {
        return Err(super::ReadError::Invalid {
            at: 0,
            what: "light values",
        });
    }
    Ok(LightRec {
        name,
        style,
        on,
        intensity,
        watts: if watts.is_finite() { watts } else { 0.0 },
        diffuse,
        direction,
        location,
        spot_angle_deg: if spot_angle_deg.is_finite() {
            spot_angle_deg
        } else {
            0.0
        },
        length,
        width,
        // openNURBS writes its unset value (-1.23e308) when there is no hotspot.
        hotspot: hotspot.filter(|h| h.is_finite() && h.abs() < 1e300),
        layer: -1,
        visible: true,
    })
}

/// The attributes chunk of a light record (`0x02008061`), when it has one that reads.
fn light_attributes(body: &[u8], width: Width) -> Option<super::attributes::Attributes> {
    let mut c = Cursor::new(body);
    while !c.is_empty() {
        let part = chunk(&mut c, width).ok()?;
        if part.typecode & !TCODE_CRC == TCODE_LIGHT_RECORD_ATTRIBUTES {
            return super::attributes::read(part.body, width).ok();
        }
    }
    None
}

/// The earth anchor in the settings attributes (`ON_3dmSettings`, minor 3 and later): the
/// version byte, linetype scale, colour, three ints, the page units (minor 1), a UUID
/// (minor 2), the model base point, then the anchor in an anonymous chunk: major and minor
/// ints, latitude, longitude, elevation (metres), model base point, north and east. None when
/// absent or unset (openNURBS's unset value, or latitude and longitude both zero).
fn earth_anchor(body: &[u8], width: Width) -> Result<Option<EarthAnchorRec>> {
    let mut c = Cursor::new(body);
    let version = c.u8()?;
    if version >> 4 != 1 {
        return Err(super::ReadError::Unsupported("settings attributes version"));
    }
    let minor = version & 0x0f;
    if minor < 3 {
        return Ok(None);
    }
    c.skip(8 + 4 + 12)?;
    c.skip_chunk(width)?;
    c.skip(16 + 24)?;
    let anchor = chunk(&mut c, width)?;
    let mut a = Cursor::new(anchor.body);
    if a.i32()? != 1 {
        return Err(super::ReadError::Unsupported("earth anchor version"));
    }
    a.skip(4)?;
    let latitude = a.f64()?;
    let longitude = a.f64()?;
    let elevation_m = a.f64()?;
    a.skip(24)?;
    let north = a.point3()?;
    let east = a.point3()?;
    let set = |v: f64| v.is_finite() && v.abs() < 1e300;
    if !set(latitude)
        || !set(longitude)
        || latitude.abs() > 90.0
        || longitude.abs() > 360.0
        || (latitude == 0.0 && longitude == 0.0)
    {
        return Ok(None);
    }
    let unit = |v: [f64; 3], fallback: [f64; 3]| {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if l.is_finite() && l > 1e-12 {
            [v[0] / l, v[1] / l, v[2] / l]
        } else {
            fallback
        }
    };
    Ok(Some(EarthAnchorRec {
        latitude,
        longitude,
        elevation_m: if elevation_m.is_finite() {
            elevation_m
        } else {
            0.0
        },
        north: unit(north, [0.0, 1.0, 0.0]),
        east: unit(east, [1.0, 0.0, 0.0]),
    }))
}

/// `ON_3dmApplication`: version byte, name, URL, details.
fn application(body: &[u8], doc: &mut Document) -> Result<()> {
    let mut c = Cursor::new(body);
    c.u8()?;
    let name = c.wstring()?;
    c.wstring()?;
    let details = c.wstring()?;
    doc.application = Some(name);
    doc.application_details = Some(details);
    Ok(())
}

/// `ON_Layer` (version 1.x): mode, index, IGES level, material index, an obsolete index,
/// colour, obsolete line style, name; then by minor version: visibility (1), linetype (2),
/// plot colour and weight (3), locked (4), UUID (5), parent UUID (6, written by openNURBS
/// after 2005-05-11). Later fields are not needed.
fn layer(data: &[u8], stamp: u32) -> Result<LayerRec> {
    let mut c = Cursor::new(data);
    let version = c.u8()?;
    let minor = version & 0x0f;
    if version >> 4 != 1 {
        return Err(super::ReadError::Unsupported("layer version"));
    }
    let mode = c.i32()?;
    let index = c.i32()?;
    c.i32()?; // IGES level
    let material_index = c.i32()?;
    c.i32()?;
    let color = c.bytes(4)?;
    c.skip(2 + 2 + 8 + 8)?;
    let name = c.wstring()?;
    let visible = if minor >= 1 { c.bool8()? } else { mode != 1 };
    if minor >= 2 {
        c.i32()?;
    }
    if minor >= 3 {
        c.skip(4 + 8)?;
    }
    let locked = if minor >= 4 { c.bool8()? } else { mode == 2 };
    let id = if minor >= 5 { Some(c.uuid()?) } else { None };
    let parent = if minor >= 6 && stamp > LAYER_PARENT_STAMP {
        Some(c.uuid()?)
    } else {
        None
    };
    let nil = [0u8; 16];
    Ok(LayerRec {
        archive_index: Some(i64::from(index)),
        name: Some(name),
        color: Some(hex_rgba(color)),
        visible: Some(visible),
        locked: Some(locked),
        material_index: Some(i64::from(material_index)),
        parent_uuid: parent.filter(|p| *p != nil).map(|p| uuid_string(&p)),
        source_uuid: id.filter(|p| *p != nil).map(|p| uuid_string(&p)),
    })
}

/// `ON_Group`: version byte, index, name, UUID (minor 1).
fn group(data: &[u8]) -> Result<GroupRec> {
    let mut c = Cursor::new(data);
    if c.u8()? >> 4 != 1 {
        return Err(super::ReadError::Unsupported("group version"));
    }
    let index = c.i32()?;
    let name = c.wstring()?;
    Ok(GroupRec {
        archive_index: Some(i64::from(index)),
        name: Some(name),
    })
}

/// `ON_Color` bytes (red, green, blue, alpha) as `rrggbbaa`.
pub(crate) fn hex_rgba(bytes: &[u8]) -> String {
    bytes.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Identity of a model component (archive 60 and later): its index, UUID and name.
#[derive(Debug, Default)]
pub(crate) struct Component {
    pub index: Option<i32>,
    pub uuid: [u8; 16],
    pub name: String,
}

/// Reads the model component attributes chunk at the cursor: `0x40008002` with one status
/// byte per field (1 = value follows), or an anonymous chunk with a bit mask.
pub(crate) fn component(c: &mut Cursor, width: Width) -> Result<Component> {
    let at = c.position();
    let (typecode, value) = c.chunk_header(width)?;
    if typecode != TCODE_MODEL_ATTRIBUTES && typecode != TCODE_ANONYMOUS {
        return Err(super::ReadError::Invalid {
            at,
            what: "model component attributes",
        });
    }
    let mut v = c.long_body(at, typecode, value)?;
    if v.i32()? != 1 {
        return Err(super::ReadError::Unsupported("model component version"));
    }
    v.i32()?;
    let mut out = Component::default();
    if typecode == TCODE_ANONYMOUS {
        let bits = v.u32()?;
        if bits & 1 != 0 {
            out.uuid = v.uuid()?;
        }
        if bits & 2 != 0 {
            v.skip(16)?;
        }
        if bits & 4 != 0 {
            out.index = Some(v.i32()?);
        }
        if bits & 8 != 0 {
            out.name = v.wstring()?;
        }
        return Ok(out);
    }
    if v.u8()? == 1 {
        v.skip(12)?; // serial number
    }
    if v.u8()? == 1 {
        out.uuid = v.uuid()?;
    }
    if v.u8()? == 1 {
        v.skip(4)?; // component type
    }
    if v.u8()? == 1 {
        out.index = Some(v.i32()?);
    }
    if v.u8()? == 1 {
        out.name = v.wstring()?;
    }
    Ok(out)
}
