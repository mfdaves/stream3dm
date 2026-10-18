//! Compressed buffers (`ReadCompressedBuffer`): `u32` uncompressed size (always 4 bytes);
//! when non-zero, `u32` CRC32 of the uncompressed bytes and a method byte, then either the raw
//! bytes (method 0) or one anonymous chunk holding a zlib stream (method 1).

use super::cursor::Cursor;
use super::{ReadError, Result};
use crate::stream::frame::Width;

const TCODE_ANONYMOUS: u32 = 0x4000_8000;

/// Reads a buffer whose uncompressed size must be `expected` (or 0 when absent). Returns
/// `None` for an absent buffer. CRC mismatches are not errors (openNURBS keeps the data).
pub(crate) fn read(c: &mut Cursor, width: Width, expected: usize) -> Result<Option<Vec<u8>>> {
    let at = c.position();
    let size = c.u32()? as usize;
    if size == 0 {
        return Ok(None);
    }
    if size != expected {
        return Err(ReadError::Invalid {
            at,
            what: "buffer size",
        });
    }
    c.u32()?; // CRC32 of the uncompressed bytes
    match c.u8()? {
        0 => Ok(Some(c.bytes(size)?.to_vec())),
        1 => {
            let stream = c.chunk(width, TCODE_ANONYMOUS)?;
            let input = stream.rest();
            // Deflate expands at most about 1032:1; a larger claim is corrupt, and refusing it
            // keeps a bad size from allocating gigabytes.
            if size > input.len().saturating_mul(1032).saturating_add(64) {
                return Err(ReadError::Invalid {
                    at,
                    what: "buffer size exceeds what its stream can hold",
                });
            }
            let mut out = vec![0u8; size];
            let n = miniz_oxide::inflate::decompress_slice_iter_to_slice(
                &mut out,
                std::iter::once(input),
                true,
                true,
            )
            .map_err(|_| ReadError::Invalid {
                at,
                what: "zlib stream",
            })?;
            if n != size {
                return Err(ReadError::Invalid {
                    at,
                    what: "inflated size",
                });
            }
            Ok(Some(out))
        }
        _ => Err(ReadError::Invalid {
            at,
            what: "buffer method",
        }),
    }
}

/// Skips a buffer without inflating it.
pub(crate) fn skip(c: &mut Cursor, width: Width) -> Result<()> {
    let at = c.position();
    let size = c.u32()? as usize;
    if size == 0 {
        return Ok(());
    }
    c.u32()?;
    match c.u8()? {
        0 => c.skip(size),
        1 => c.chunk(width, TCODE_ANONYMOUS).map(|_| ()),
        _ => Err(ReadError::Invalid {
            at,
            what: "buffer method",
        }),
    }
}
