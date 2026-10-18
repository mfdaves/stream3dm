//! Points (`ON_Point`) and point clouds (`ON_PointCloud`).
//!
//! Layouts (version byte first, major 1):
//! - point: the point;
//! - point cloud: the points (count, then x, y, z each), then a plane, a box, flags and, from
//!   minor 1, normals, colours and point values, none of which a display needs.
//!
//! Writers for archive 2 and earlier store a point cloud as one point record per point.

use super::cursor::Cursor;
use super::{ReadError, Result};

pub(crate) fn read_point(data: &[u8]) -> Result<[f64; 3]> {
    let mut c = Cursor::new(data);
    major_one(&mut c, "point version")?;
    c.point3()
}

pub(crate) fn read_cloud(data: &[u8]) -> Result<Vec<[f64; 3]>> {
    let mut c = Cursor::new(data);
    major_one(&mut c, "point cloud version")?;
    let n = c.count(24)?;
    (0..n).map(|_| c.point3()).collect()
}

fn major_one(c: &mut Cursor, what: &'static str) -> Result<()> {
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported(what));
    }
    Ok(())
}
