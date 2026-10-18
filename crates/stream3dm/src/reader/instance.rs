//! Block instances (`ON_InstanceRef::Read`): version byte (major 1), definition UUID, then the
//! placement as a row-major 4×4 matrix (column vectors, translation in the last column) and a
//! cached bounding box.

use super::cursor::Cursor;
use super::{ReadError, Result};
use crate::native::Matrix;

pub(crate) struct InstanceRef {
    pub definition: [u8; 16],
    /// Placement in model units; `None` when a value is not finite.
    pub matrix: Option<Matrix>,
}

pub(crate) fn read(data: &[u8]) -> Result<InstanceRef> {
    let mut c = Cursor::new(data);
    if c.u8()? >> 4 != 1 {
        return Err(ReadError::Unsupported("instance reference version"));
    }
    let definition = c.uuid()?;
    let m = c.xform()?;
    let finite = m.iter().flatten().all(|v| v.is_finite());
    Ok(InstanceRef {
        definition,
        matrix: finite.then_some(m),
    })
}
