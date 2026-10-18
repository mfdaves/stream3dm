//! Views (`ON_3dmView`) from the settings table: the named views (`0x20008036`) and the
//! viewports of the saved layout (`0x20008037`). Each list holds a count, then one view record
//! (`0x2000803b`) per view, whose children carry the viewport (`0x2000823b`), the name
//! (`0x20008a3b`), the target (`0x2000883b`) and the view attributes (`0x20008c3b`, clipping
//! planes from version 1.4).

use super::cursor::Cursor;
use super::record::chunk;
use super::{ReadError, Result};
use crate::native::{ClippingPlaneRec, ViewAttributesRec, ViewRec, ViewportRec};
use crate::stream::frame::{TCODE_SHORT, Width};

const TCODE_VIEW_RECORD: u32 = 0x2000_803b;
const TCODE_VIEW_VIEWPORT: u32 = 0x2000_823b;
const TCODE_VIEW_TARGET: u32 = 0x2000_883b;
const TCODE_VIEW_NAME: u32 = 0x2000_8a3b;
const TCODE_VIEW_ATTRIBUTES: u32 = 0x2000_8c3b;
const TCODE_ANONYMOUS: u32 = 0x4000_8000;
/// Lists longer than this are treated as corrupt.
const MAX_VIEWS: usize = 1 << 16;

/// The views of one list body; `kind` is `named` or `active`, `scale` converts model units to
/// millimetres. Also returns how many view records could not be read.
pub(crate) fn read_list(
    body: &[u8],
    width: Width,
    kind: &str,
    scale: f64,
) -> (Vec<ViewRec>, usize) {
    let mut c = Cursor::new(body);
    let Ok(count) = c.i32() else {
        return (Vec::new(), 1);
    };
    let Some(count) = usize::try_from(count).ok().filter(|n| *n <= MAX_VIEWS) else {
        return (Vec::new(), 1);
    };
    let mut views = Vec::new();
    let mut failed = 0;
    for index in 0..count {
        let Ok(record) = chunk(&mut c, width) else {
            failed += 1;
            break;
        };
        if record.typecode != TCODE_VIEW_RECORD {
            failed += 1;
            break;
        }
        match view(record.body, width, scale) {
            Ok(mut v) => {
                v.list_kind = Some(kind.to_string());
                v.list_index = Some(index as i64);
                views.push(v);
            }
            Err(_) => failed += 1,
        }
    }
    (views, failed)
}

fn view(body: &[u8], width: Width, scale: f64) -> Result<ViewRec> {
    let mut v = ViewRec {
        name: Some(String::new()),
        list_kind: None,
        list_index: None,
        viewport: None,
        target_millimeters: None,
        attributes: None,
    };
    let mut c = Cursor::new(body);
    while !c.is_empty() {
        let child = chunk(&mut c, width)?;
        if child.typecode & TCODE_SHORT != 0 {
            continue;
        }
        let mut b = Cursor::new(child.body);
        match child.typecode {
            // A viewport that cannot be read leaves the view without a camera.
            TCODE_VIEW_VIEWPORT => v.viewport = viewport(&mut b, scale).ok(),
            TCODE_VIEW_NAME => v.name = Some(b.wstring()?),
            TCODE_VIEW_TARGET => v.target_millimeters = Some(scaled(b.point3()?, scale)?.to_vec()),
            TCODE_VIEW_ATTRIBUTES => v.attributes = Some(attributes(&mut b, width, scale)?),
            _ => {}
        }
    }
    Ok(v)
}

/// `ON_Viewport` (version 1.x): validity flags, projection, camera location, direction, up
/// and frame axes, frustum (left, right, bottom, top, near, far), port; then by minor version
/// the viewport UUID (1), five locks (2) and the target point (3).
fn viewport(c: &mut Cursor, scale: f64) -> Result<ViewportRec> {
    let version = c.u8()?;
    if version >> 4 != 1 {
        return Err(ReadError::Unsupported("viewport version"));
    }
    let minor = version & 0x0f;
    c.skip(3 * 4)?; // camera, frustum and port validity
    let projection = c.i32()?;
    let camera = scaled(c.point3()?, scale)?;
    let direction = finite(c.point3()?)?;
    let up = finite(c.point3()?)?;
    for _ in 0..3 {
        finite(c.point3()?)?; // camera frame axes
    }
    let mut frustum = [0.0; 6];
    for v in &mut frustum {
        *v = c.f64()? * scale;
    }
    if !frustum.iter().all(|v| v.is_finite()) {
        return Err(ReadError::Invalid {
            at: c.position(),
            what: "viewport frustum",
        });
    }
    c.skip(6 * 4)?; // port
    if minor >= 1 {
        c.uuid()?;
    }
    if minor >= 2 {
        c.skip(5)?; // locks
    }
    let target = if minor >= 3 {
        Some(scaled(c.point3()?, scale)?.to_vec())
    } else {
        None
    };
    Ok(ViewportRec {
        projection: Some(i64::from(projection)),
        camera_location_mm: Some(camera.to_vec()),
        camera_direction: Some(direction.to_vec()),
        camera_up: Some(up.to_vec()),
        frustum_mm: Some(frustum.to_vec()),
        target_millimeters: target,
    })
}

fn finite(p: [f64; 3]) -> Result<[f64; 3]> {
    scaled(p, 1.0)
}

/// The view attributes: clipping planes (version 1.4) with their equation in millimetres.
fn attributes(c: &mut Cursor, width: Width, scale: f64) -> Result<ViewAttributesRec> {
    let version = c.u8()?;
    let minor = version & 0x0f;
    if version >> 4 != 1 || minor < 1 {
        return Err(ReadError::Unsupported("view attributes version"));
    }
    c.i32()?; // view type
    c.skip(8 + 8)?; // page size
    c.uuid()?; // obsolete parent
    c.skip(6 * 8)?; // bounds
    let mut out = ViewAttributesRec {
        clipping_planes: Vec::new(),
    };
    if minor >= 2 {
        c.uuid()?; // display mode
        c.chunk(width, TCODE_ANONYMOUS)?; // page settings
    }
    if minor >= 3 {
        c.bool8()?; // projection locked
    }
    if minor >= 4 {
        let count = c.count(1)?;
        for _ in 0..count {
            let mut p = c.chunk(width, TCODE_ANONYMOUS)?;
            if p.i32()? != 1 {
                return Err(ReadError::Unsupported("clipping plane version"));
            }
            p.i32()?;
            let (a, b, cc, d) = (p.f64()?, p.f64()?, p.f64()?, p.f64()?);
            p.uuid()?;
            let enabled = p.bool8()?;
            let d = d * scale;
            if ![a, b, cc, d].iter().all(|v| v.is_finite()) {
                return Err(ReadError::Invalid {
                    at: 0,
                    what: "clipping plane equation",
                });
            }
            out.clipping_planes.push(ClippingPlaneRec {
                enabled: Some(enabled),
                equation_mm: Some(vec![a, b, cc, d]),
            });
        }
    }
    Ok(out)
}

/// A point scaled to millimetres, refused when a coordinate is not finite.
fn scaled(p: [f64; 3], scale: f64) -> Result<[f64; 3]> {
    let p = p.map(|v| v * scale);
    if p.iter().all(|v| v.is_finite()) {
        Ok(p)
    } else {
        Err(ReadError::Invalid {
            at: 0,
            what: "coordinate",
        })
    }
}
