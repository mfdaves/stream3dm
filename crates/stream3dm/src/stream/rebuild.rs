//! Assembles small, valid `.3dm` files from a [`Prefix`] and a batch of object records.

use super::frame::{HEADER_LEN, Prefix, TCODE_CRC, TCODE_END_OF_FILE, TCODE_END_OF_TABLE, Width};

/// Bytes of the end-of-table short chunk.
fn end_of_table_len(width: Width) -> usize {
    width.header_len()
}

/// Bytes of the end-of-file chunk (header + file size).
fn end_of_file_len(width: Width) -> usize {
    width.header_len() + width.bytes()
}

/// Writes the object table chunk: header with recomputed length, the records byte for byte,
/// the end-of-table chunk and, when the original table carried the CRC bit, a CRC over its
/// direct body bytes (none, so zero).
pub(crate) fn object_table(prefix: &Prefix, records: &[u8], out: &mut Vec<u8>) {
    let w = prefix.width;
    let crc = prefix.object_table_typecode & TCODE_CRC != 0;
    let body = records.len() + end_of_table_len(w) + if crc { 4 } else { 0 };
    out.extend_from_slice(&prefix.object_table_typecode.to_le_bytes());
    w.put(out, body as u64);
    out.extend_from_slice(records);
    out.extend_from_slice(&TCODE_END_OF_TABLE.to_le_bytes());
    w.put(out, 0);
    if crc {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
}

/// A complete file: header, the prefix chunks whose typecode is not in `exclude`, an object
/// table with `records`, and an end-of-file chunk holding the file size.
pub(crate) fn mini_file(prefix: &Prefix, exclude: &[u32], records: &[u8]) -> Vec<u8> {
    let w = prefix.width;
    let kept: Vec<_> = prefix
        .spans
        .iter()
        .filter(|s| !exclude.contains(&s.typecode))
        .collect();
    let size = HEADER_LEN
        + kept.iter().map(|s| s.end - s.start).sum::<usize>()
        + w.header_len()
        + records.len()
        + end_of_table_len(w)
        + if prefix.object_table_typecode & TCODE_CRC != 0 {
            4
        } else {
            0
        }
        + end_of_file_len(w);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&prefix.bytes[..HEADER_LEN]);
    for span in kept {
        out.extend_from_slice(&prefix.bytes[span.start..span.end]);
    }
    object_table(prefix, records, &mut out);
    out.extend_from_slice(&TCODE_END_OF_FILE.to_le_bytes());
    w.put(&mut out, w.bytes() as u64);
    w.put(&mut out, (size) as u64);
    debug_assert_eq!(out.len(), size);
    out
}
