//! A bounds-checked little-endian reader over a byte slice.

use super::{ReadError, Result};
use crate::stream::frame::{TCODE_CRC, TCODE_SHORT, Width};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(ReadError::Eof { at: self.pos })?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// The bytes not read yet (the cursor does not advance).
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.bytes(N)?.try_into().expect("length checked"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub(crate) fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array()?))
    }

    pub(crate) fn uuid(&mut self) -> Result<[u8; 16]> {
        self.array()
    }

    /// A one-byte boolean; any non-zero value is true (old writers stored other values).
    pub(crate) fn bool8(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn point3(&mut self) -> Result<[f64; 3]> {
        Ok([self.f64()?, self.f64()?, self.f64()?])
    }

    pub(crate) fn xform(&mut self) -> Result<[[f64; 4]; 4]> {
        let mut m = [[0.0; 4]; 4];
        for row in &mut m {
            for v in row.iter_mut() {
                *v = self.f64()?;
            }
        }
        Ok(m)
    }

    /// `ON_wString`: `u32` count of UTF-16 units including the terminating zero, then the
    /// units. Decoded lossily; the last unit is dropped unchecked, as openNURBS does.
    pub(crate) fn wstring(&mut self) -> Result<String> {
        let at = self.pos;
        let n = self.u32()? as usize;
        if n == 0 {
            return Ok(String::new());
        }
        if n & 0x0f00_0000 != 0 || n.saturating_mul(2) > self.remaining() {
            return Err(ReadError::Invalid {
                at,
                what: "string length",
            });
        }
        let units: Vec<u16> = self
            .bytes(2 * n)?
            .chunks_exact(2)
            .take(n - 1)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }

    /// `ReadArray` of `i32`: a signed count (≤ 0 is empty), then the values.
    pub(crate) fn i32_array(&mut self) -> Result<Vec<i32>> {
        let at = self.pos;
        let n = self.i32()?.max(0) as usize;
        if n.saturating_mul(4) > self.remaining() {
            return Err(ReadError::Invalid {
                at,
                what: "array length",
            });
        }
        (0..n).map(|_| self.i32()).collect()
    }

    /// Skips a `ReadArray` of `item`-byte elements.
    pub(crate) fn skip_array(&mut self, item: usize) -> Result<()> {
        let at = self.pos;
        let n = self.i32()?.max(0) as usize;
        let len = n.checked_mul(item).ok_or(ReadError::Invalid {
            at,
            what: "array length",
        })?;
        self.skip(len)
    }

    /// A chunk header: typecode and value (length of a long chunk, or a short chunk's value).
    pub(crate) fn chunk_header(&mut self, width: Width) -> Result<(u32, i64)> {
        let typecode = self.u32()?;
        let value = match width {
            Width::Four if typecode & TCODE_SHORT != 0 => i64::from(self.i32()?),
            Width::Four => i64::from(self.u32()?),
            Width::Eight => self.i64()?,
        };
        Ok((typecode, value))
    }

    /// A long chunk with typecode `expected`; returns its body without the trailing CRC when the
    /// typecode carries one.
    pub(crate) fn chunk(&mut self, width: Width, expected: u32) -> Result<Cursor<'a>> {
        let at = self.pos;
        let (typecode, value) = self.chunk_header(width)?;
        if typecode != expected || typecode & TCODE_SHORT != 0 {
            return Err(ReadError::Invalid {
                at,
                what: "unexpected chunk",
            });
        }
        self.long_body(at, typecode, value)
    }

    /// The body of a long chunk whose header was just read.
    pub(crate) fn long_body(&mut self, at: usize, typecode: u32, value: i64) -> Result<Cursor<'a>> {
        let len = usize::try_from(value).map_err(|_| ReadError::Invalid {
            at,
            what: "chunk length",
        })?;
        let body = self.bytes(len)?;
        if typecode & TCODE_CRC != 0 {
            let data = body
                .len()
                .checked_sub(4)
                .map(|n| &body[..n])
                .ok_or(ReadError::Invalid {
                    at,
                    what: "chunk too short for its CRC",
                })?;
            Ok(Cursor::new(data))
        } else {
            Ok(Cursor::new(body))
        }
    }

    /// Skips one chunk of any kind.
    pub(crate) fn skip_chunk(&mut self, width: Width) -> Result<()> {
        let at = self.pos;
        let (typecode, value) = self.chunk_header(width)?;
        if typecode & TCODE_SHORT == 0 {
            self.long_body(at, typecode, value)?;
        }
        Ok(())
    }

    /// An element count, rejected when the remaining bytes cannot hold `count × item` bytes
    /// (so a corrupt count never triggers a huge allocation).
    pub(crate) fn count(&mut self, item: usize) -> Result<usize> {
        let at = self.pos;
        let n = self.i32()?;
        let n = usize::try_from(n).map_err(|_| ReadError::Invalid {
            at,
            what: "negative count",
        })?;
        if n.checked_mul(item).is_none_or(|b| b > self.remaining()) {
            return Err(ReadError::Invalid {
                at,
                what: "count exceeds data",
            });
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_values_and_stops_at_the_end() {
        let data = [
            1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f,
        ];
        let mut c = Cursor::new(&data);
        assert_eq!(c.u32(), Ok(1));
        assert_eq!(c.i32(), Ok(-1));
        assert_eq!(c.f64(), Ok(1.0));
        assert!(c.is_empty());
        assert_eq!(c.u8(), Err(ReadError::Eof { at: 16 }));
    }

    #[test]
    fn counts_must_fit_the_data() {
        let data = [3, 0, 0, 0, 1, 2, 3, 4, 5, 6];
        assert_eq!(Cursor::new(&data).count(2), Ok(3));
        assert!(matches!(
            Cursor::new(&data).count(4),
            Err(ReadError::Invalid { .. })
        ));
        let negative = (-1i32).to_le_bytes();
        assert!(matches!(
            Cursor::new(&negative).count(1),
            Err(ReadError::Invalid { .. })
        ));
    }
}
