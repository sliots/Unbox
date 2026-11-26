//! Proves the decompressed length of an LZ4 frame made only of stored
//! (uncompressed) blocks, so its output budget can be exact. The 7-Zip decoder still
//! checks the frame and its content.

use std::io::{Read, Seek, SeekFrom};

/// Scan from the current position (which must be 0). `continue_scan` is polled per
/// block and aborts the scan when it returns `false`.
pub fn scan_stored_frame<S: Read + Seek>(stream: &mut S, physical_size: u64, continue_scan: &mut dyn FnMut() -> bool) -> Option<u64> {
    let mut position = 0u64;
    let read = |stream: &mut S, position: &mut u64, buf: &mut [u8]| -> bool {
        if buf.len() as u64 > physical_size - *position {
            return false;
        }
        if stream.read_exact(buf).is_err() {
            return false;
        }
        *position += buf.len() as u64;
        true
    };
    let skip = |stream: &mut S, position: &mut u64, size: u64| -> bool {
        if size > physical_size - *position {
            return false;
        }
        *position += size;
        matches!(stream.seek(SeekFrom::Start(*position)), Ok(actual) if actual == *position)
    };
    let le = |bytes: &[u8]| bytes.iter().rev().fold(0u64, |acc, b| (acc << 8) | *b as u64);

    let mut header = [0u8; 6];
    if !read(stream, &mut position, &mut header) || le(&header[..4]) != 0x184D_2204 {
        return None;
    }
    let flags = header[4];
    let descriptor = header[5];
    let block_code = (descriptor >> 4) & 7;
    if flags & 0xC3 != 0x40 || descriptor & 0x8F != 0 || !(4..=7).contains(&block_code) {
        return None;
    }
    let max_block = 1u32 << (8 + 2 * block_code as u32);
    let mut declared = None;
    if flags & 0x08 != 0 {
        let mut content = [0u8; 8];
        if !read(stream, &mut position, &mut content) {
            return None;
        }
        declared = Some(le(&content));
    }
    let mut checksum = [0u8; 1];
    if !read(stream, &mut position, &mut checksum) {
        return None;
    }

    let mut total = 0u64;
    for blocks in 0..=65536u32 {
        if !continue_scan() {
            return None;
        }
        let mut length_bytes = [0u8; 4];
        if !read(stream, &mut position, &mut length_bytes) {
            return None;
        }
        let field = le(&length_bytes) as u32;
        if field == 0 {
            if flags & 0x04 != 0 && !skip(stream, &mut position, 4) {
                return None;
            }
            if position != physical_size || declared.is_some_and(|d| d != total) {
                return None;
            }
            return Some(total);
        }
        let length = field & 0x7FFF_FFFF;
        if field & 0x8000_0000 == 0
            || length > max_block
            || blocks == 65536
            || length as u64 > u64::MAX - total
            || !skip(stream, &mut position, length as u64 + if flags & 0x10 != 0 { 4 } else { 0 })
        {
            return None;
        }
        total += length as u64;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(blocks: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![0x04, 0x22, 0x4D, 0x18, 0x40, 0x40, 0x00];
        for block in blocks {
            out.extend_from_slice(&((block.len() as u32) | 0x8000_0000).to_le_bytes());
            out.extend_from_slice(block);
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    #[test]
    fn stored_frame_length() {
        let data = frame(&[b"hello", b" world"]);
        let size = data.len() as u64;
        assert_eq!(scan_stored_frame(&mut Cursor::new(data), size, &mut || true), Some(11));
    }

    #[test]
    fn compressed_block_is_rejected() {
        let mut data = frame(&[b"hello"]);
        data[7 + 3] &= 0x7F;
        let size = data.len() as u64;
        assert_eq!(scan_stored_frame(&mut Cursor::new(data), size, &mut || true), None);
    }
}
