//! Finding the raw PPMd stream inside a `.7z` file.
//!
//! Not a 7z reader: container decoding belongs to sevenz-turbo. This is the
//! smallest walk that answers one question about one shape of archive, a
//! single file in a single folder compressed with a single `PPMD` coder and
//! an uncompressed header, which is what
//! `7zz a -t7z -mhc=off -m0=PPMd:o=N:mem=M` writes for one input file:
//! which bytes are the PPMd stream, what are the coder's five property bytes,
//! and how long is the output.
//!
//! Parsing the header, rather than reading `7zz l -slt`, matters here because
//! 7-Zip shrinks the requested memory size to fit a small input (`mem=1g`
//! over a 64 KiB file is written as 1 MiB), so the only trustworthy order
//! and memory size are the ones in the coder properties.

/// Where the PPMd stream is in a `.7z` file, and how to decode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackStream {
    /// Offset of the stream's first byte: 32 plus the pack position.
    pub offset: u64,
    /// The stream's length in the file.
    pub packed_len: u64,
    /// What it decodes to, from the folder's unpacked size.
    pub unpacked_len: u64,
    /// The model order, the first property byte.
    pub order: u32,
    /// The sub-allocator size, the next four property bytes, little-endian.
    pub mem_size: u32,
}

const SIGNATURE: &[u8; 6] = b"7z\xBC\xAF\x27\x1C";
const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_MAIN_STREAMS_INFO: u8 = 0x04;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SIZE: u8 = 0x09;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;

/// The 7z method id of PPMd (`k_PPMD` in 7-Zip).
pub const PPMD_CODER_ID: u64 = 0x03_04_01;

/// Reads the pack-stream range and coder properties out of a whole `.7z`.
///
/// Returns `None` for anything that is not the one shape described above,
/// including an encoded (compressed) header and an archive with no streams
/// (7-Zip writes an empty file as a header-only archive).
pub fn pack_stream(data: &[u8]) -> Option<PackStream> {
    if data.len() < 32 || &data[..6] != SIGNATURE {
        return None;
    }
    let next_offset = u64::from_le_bytes(data[12..20].try_into().ok()?);
    let next_size = u64::from_le_bytes(data[20..28].try_into().ok()?);
    let start = usize::try_from(32u64.checked_add(next_offset)?).ok()?;
    let end = start.checked_add(usize::try_from(next_size).ok()?)?;
    let header = data.get(start..end)?;
    let mut r = Reader {
        buf: header,
        pos: 0,
    };
    if r.byte()? != K_HEADER || r.byte()? != K_MAIN_STREAMS_INFO {
        return None;
    }
    if r.byte()? != K_PACK_INFO {
        return None;
    }
    let pack_pos = r.number()?;
    if r.number()? != 1 || r.byte()? != K_SIZE {
        return None;
    }
    let packed_len = r.number()?;
    // An optional pack-stream CRC (kCRC) may precede kEnd; 7-Zip does not
    // write one, and anything else is a shape this walk refuses.
    if r.byte()? != K_END {
        return None;
    }
    if r.byte()? != K_UNPACK_INFO || r.byte()? != K_FOLDER {
        return None;
    }
    if r.number()? != 1 || r.byte()? != 0 || r.number()? != 1 {
        return None;
    }
    let flags = r.byte()?;
    if flags & 0x10 != 0 || flags & 0x20 == 0 {
        return None;
    }
    let mut id: u64 = 0;
    for _ in 0..(flags & 0x0F) {
        id = (id << 8) | u64::from(r.byte()?);
    }
    if id != PPMD_CODER_ID || r.number()? != 5 {
        return None;
    }
    let order = u32::from(r.byte()?);
    let mut mem = [0u8; 4];
    for b in &mut mem {
        *b = r.byte()?;
    }
    if r.byte()? != K_CODERS_UNPACK_SIZE {
        return None;
    }
    let unpacked_len = r.number()?;
    let offset = 32u64.checked_add(pack_pos)?;
    let stream_end = offset.checked_add(packed_len)?;
    if stream_end > data.len() as u64 {
        return None;
    }
    Some(PackStream {
        offset,
        packed_len,
        unpacked_len,
        order,
        mem_size: u32::from_le_bytes(mem),
    })
}

/// The stream bytes [`pack_stream`] located.
pub fn stream_bytes<'a>(data: &'a [u8], stream: &PackStream) -> &'a [u8] {
    &data[stream.offset as usize..(stream.offset + stream.packed_len) as usize]
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// 7z's variable-length number (`ReadNumber` in 7-Zip's `7zIn.c`).
    fn number(&mut self) -> Option<u64> {
        let first = self.byte()?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Some(value | (high << (i * 8)));
            }
            value |= u64::from(self.byte()?) << (i * 8);
            mask >>= 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_encodings() {
        let mut r = Reader {
            buf: &[0x05, 0x81, 0x02, 0xC0, 0x34, 0x12],
            pos: 0,
        };
        assert_eq!(r.number(), Some(5));
        assert_eq!(r.number(), Some(0x102));
        assert_eq!(r.number(), Some(0x1234));
    }

    #[test]
    fn rejects_non_7z() {
        assert_eq!(
            pack_stream(b"not a 7z archive at all, but long enough"),
            None
        );
        assert_eq!(pack_stream(&[]), None);
    }
}
