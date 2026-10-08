//! A minimal single-folder PPMd `.7z` writer and reader.
//!
//! Just enough of the 7z container to hand a PPMd stream to `7zz` and to
//! take one back: a signature header with its CRCs, a plain `kHeader` (no
//! `kEncodedHeader`, so archives from `7zz` need `-mhc=off`), one pack
//! stream, one folder with one coder (`03 04 01`, PPMd, 5 property bytes:
//! order then the memory size as a little-endian `u32`) and one file.
//!
//! The only dependency is `crc32fast`, so `tests/differential_binaries.rs`
//! includes this file directly with `#[path]`.

use std::fmt;

/// The 7z signature.
pub const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
/// The PPMd coder id.
pub const PPMD_CODER_ID: [u8; 3] = [0x03, 0x04, 0x01];

const SIGNATURE_HEADER_LEN: usize = 32;

const K_END: u8 = 0x00;
const K_HEADER: u8 = 0x01;
const K_ARCHIVE_PROPERTIES: u8 = 0x02;
const K_ADDITIONAL_STREAMS_INFO: u8 = 0x03;
const K_MAIN_STREAMS_INFO: u8 = 0x04;
const K_FILES_INFO: u8 = 0x05;
const K_PACK_INFO: u8 = 0x06;
const K_UNPACK_INFO: u8 = 0x07;
const K_SUBSTREAMS_INFO: u8 = 0x08;
const K_SIZE: u8 = 0x09;
const K_CRC: u8 = 0x0A;
const K_FOLDER: u8 = 0x0B;
const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
const K_NUM_UNPACK_STREAM: u8 = 0x0D;
const K_NAME: u8 = 0x11;
const K_ENCODED_HEADER: u8 = 0x17;

/// Why an archive was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError(pub String);

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "7z: {}", self.0)
    }
}

impl std::error::Error for FormatError {}

fn bad<T>(what: impl Into<String>) -> Result<T, FormatError> {
    Err(FormatError(what.into()))
}

/// One PPMd stream and what the container says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpmdEntry {
    /// The model order from the coder properties.
    pub order: u8,
    /// The arena size in bytes from the coder properties.
    pub mem: u32,
    /// The raw coded stream.
    pub stream: Vec<u8>,
    /// The decoded size.
    pub unpack_size: u64,
    /// The CRC-32 of the decoded data, when the archive records one.
    pub crc: Option<u32>,
}

/// The 5 PPMd property bytes: order, then the memory size little-endian.
pub fn ppmd_props(order: u8, mem: u32) -> [u8; 5] {
    let m = mem.to_le_bytes();
    [order, m[0], m[1], m[2], m[3]]
}

/// Writes a 7z `NUMBER`.
pub fn write_number(out: &mut Vec<u8>, value: u64) {
    let mut first = 0u8;
    let mut mask = 0x80u8;
    let mut extra = 0;
    while extra < 8 {
        if value < (1u64 << (7 * (extra + 1))) {
            first |= (value >> (8 * extra)) as u8;
            break;
        }
        first |= mask;
        mask >>= 1;
        extra += 1;
    }
    out.push(first);
    for i in 0..extra {
        out.push((value >> (8 * i)) as u8);
    }
}

/// Wraps one PPMd stream in a single-file `.7z` archive. `unpack` is the
/// decoded data, needed for its size and CRC; `name` is the file name.
pub fn write_archive(
    order: u8,
    mem: u32,
    stream: &[u8],
    unpack: &[u8],
    name: &str,
) -> Result<Vec<u8>, FormatError> {
    if unpack.is_empty() {
        // 7-Zip stores an empty file as an empty stream with no folder.
        return bad("an empty file has no PPMd folder");
    }
    let mut h = vec![K_HEADER, K_MAIN_STREAMS_INFO];

    h.push(K_PACK_INFO);
    write_number(&mut h, 0); // pack position
    write_number(&mut h, 1); // pack streams
    h.push(K_SIZE);
    write_number(&mut h, stream.len() as u64);
    h.push(K_END);

    h.push(K_UNPACK_INFO);
    h.push(K_FOLDER);
    write_number(&mut h, 1); // folders
    h.push(0); // not external
    write_number(&mut h, 1); // coders
    h.push(0x20 | PPMD_CODER_ID.len() as u8); // simple coder with properties
    h.extend_from_slice(&PPMD_CODER_ID);
    write_number(&mut h, 5);
    h.extend_from_slice(&ppmd_props(order, mem));
    h.push(K_CODERS_UNPACK_SIZE);
    write_number(&mut h, unpack.len() as u64);
    h.push(K_CRC);
    h.push(1); // all defined
    h.extend_from_slice(&crc32fast::hash(unpack).to_le_bytes());
    h.push(K_END);

    h.push(K_SUBSTREAMS_INFO);
    h.push(K_END);
    h.push(K_END); // main streams info

    h.push(K_FILES_INFO);
    write_number(&mut h, 1);
    let mut utf16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    utf16.extend_from_slice(&[0, 0]);
    h.push(K_NAME);
    write_number(&mut h, 1 + utf16.len() as u64);
    h.push(0); // not external
    h.extend_from_slice(&utf16);
    h.push(K_END); // files info
    h.push(K_END); // header

    let mut tail = Vec::with_capacity(20);
    tail.extend_from_slice(&(stream.len() as u64).to_le_bytes()); // next header offset
    tail.extend_from_slice(&(h.len() as u64).to_le_bytes());
    tail.extend_from_slice(&crc32fast::hash(&h).to_le_bytes());

    let mut out = Vec::with_capacity(SIGNATURE_HEADER_LEN + stream.len() + h.len());
    out.extend_from_slice(&SIGNATURE);
    out.extend_from_slice(&[0, 4]);
    out.extend_from_slice(&crc32fast::hash(&tail).to_le_bytes());
    out.extend_from_slice(&tail);
    out.extend_from_slice(stream);
    out.extend_from_slice(&h);
    Ok(out)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8, FormatError> {
        let Some(&b) = self.data.get(self.pos) else {
            return bad("header ends early");
        };
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let Some(end) = end else {
            return bad("header ends early");
        };
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn number(&mut self) -> Result<u64, FormatError> {
        let first = self.byte()?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Ok(value | (high << (8 * i)));
            }
            value |= u64::from(self.byte()?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    fn expect(&mut self, id: u8, what: &str) -> Result<(), FormatError> {
        let b = self.byte()?;
        if b == id {
            Ok(())
        } else {
            bad(format!("expected {what} (0x{id:02X}), found 0x{b:02X}"))
        }
    }

    /// A `kCRC` digest list for `count` items, returning the first digest.
    fn digests(&mut self, count: u64) -> Result<Option<u32>, FormatError> {
        if self.byte()? == 0 {
            return bad("partially defined CRCs");
        }
        let mut first = None;
        for _ in 0..count {
            let crc = self.u32()?;
            first.get_or_insert(crc);
        }
        Ok(first)
    }
}

/// Reads the PPMd stream out of a single-file, single-folder `.7z` with a
/// plain header.
pub fn read_archive(archive: &[u8]) -> Result<PpmdEntry, FormatError> {
    if archive.len() < SIGNATURE_HEADER_LEN || archive[..6] != SIGNATURE {
        return bad("not a 7z archive");
    }
    let tail = &archive[12..32];
    let start_crc = u32::from_le_bytes([archive[8], archive[9], archive[10], archive[11]]);
    if crc32fast::hash(tail) != start_crc {
        return bad("start header CRC mismatch");
    }
    let offset = u64::from_le_bytes(tail[0..8].try_into().unwrap_or_default());
    let size = u64::from_le_bytes(tail[8..16].try_into().unwrap_or_default());
    let header_crc = u32::from_le_bytes(tail[16..20].try_into().unwrap_or_default());
    let start = usize::try_from(offset)
        .ok()
        .and_then(|o| o.checked_add(SIGNATURE_HEADER_LEN));
    let end = start
        .zip(usize::try_from(size).ok())
        .and_then(|(s, n)| s.checked_add(n));
    let (Some(start), Some(end)) = (start, end) else {
        return bad("next header out of range");
    };
    let Some(header) = archive.get(start..end) else {
        return bad("next header out of range");
    };
    if crc32fast::hash(header) != header_crc {
        return bad("next header CRC mismatch");
    }

    let mut c = Cursor {
        data: header,
        pos: 0,
    };
    match c.byte()? {
        K_HEADER => {}
        K_ENCODED_HEADER => return bad("encoded header (create the archive with -mhc=off)"),
        b => return bad(format!("unknown header id 0x{b:02X}")),
    }
    let mut id = c.byte()?;
    if id == K_ARCHIVE_PROPERTIES || id == K_ADDITIONAL_STREAMS_INFO {
        return bad("archive properties and additional streams are not supported");
    }
    if id != K_MAIN_STREAMS_INFO {
        return bad("no main streams (an empty archive or an empty file)");
    }

    // Pack info: one stream.
    c.expect(K_PACK_INFO, "kPackInfo")?;
    let pack_pos = c.number()?;
    if c.number()? != 1 {
        return bad("more than one pack stream");
    }
    c.expect(K_SIZE, "kSize")?;
    let pack_size = c.number()?;
    id = c.byte()?;
    if id == K_CRC {
        c.digests(1)?;
        id = c.byte()?;
    }
    if id != K_END {
        return bad("unexpected pack info property");
    }

    // Unpack info: one folder, one PPMd coder.
    c.expect(K_UNPACK_INFO, "kUnPackInfo")?;
    c.expect(K_FOLDER, "kFolder")?;
    if c.number()? != 1 {
        return bad("more than one folder");
    }
    if c.byte()? != 0 {
        return bad("external folders");
    }
    if c.number()? != 1 {
        return bad("more than one coder");
    }
    let flags = c.byte()?;
    if flags & 0xD0 != 0 {
        return bad("complex or reserved coder flags");
    }
    let coder_id = c.take(usize::from(flags & 0x0F))?;
    if coder_id != PPMD_CODER_ID {
        return bad(format!("coder {coder_id:02X?} is not PPMd"));
    }
    if flags & 0x20 == 0 {
        return bad("PPMd coder without properties");
    }
    let props_len = c.number()?;
    if props_len != 5 {
        return bad(format!("PPMd properties are {props_len} bytes, not 5"));
    }
    let props = c.take(5)?;
    let order = props[0];
    let mem = u32::from_le_bytes([props[1], props[2], props[3], props[4]]);
    c.expect(K_CODERS_UNPACK_SIZE, "kCodersUnPackSize")?;
    let unpack_size = c.number()?;
    let mut crc = None;
    id = c.byte()?;
    if id == K_CRC {
        crc = c.digests(1)?;
        id = c.byte()?;
    }
    if id != K_END {
        return bad("unexpected unpack info property");
    }

    // Substreams: one file per folder; its CRC may live here.
    id = c.byte()?;
    if id == K_SUBSTREAMS_INFO {
        loop {
            match c.byte()? {
                K_END => break,
                K_NUM_UNPACK_STREAM => {
                    if c.number()? != 1 {
                        return bad("more than one file in the folder");
                    }
                }
                K_SIZE => {} // no sizes for a single substream
                K_CRC => {
                    if let Some(sub) = c.digests(1)? {
                        crc = Some(sub);
                    }
                }
                b => return bad(format!("unexpected substream property 0x{b:02X}")),
            }
        }
        id = c.byte()?;
    }
    if id != K_END {
        return bad("unexpected main streams property");
    }

    let begin = usize::try_from(pack_pos)
        .ok()
        .and_then(|p| p.checked_add(SIGNATURE_HEADER_LEN));
    let finish = begin
        .zip(usize::try_from(pack_size).ok())
        .and_then(|(b, n)| b.checked_add(n));
    let (Some(begin), Some(finish)) = (begin, finish) else {
        return bad("pack stream out of range");
    };
    let Some(stream) = archive.get(begin..finish) else {
        return bad("pack stream out of range");
    };
    Ok(PpmdEntry {
        order,
        mem,
        stream: stream.to_vec(),
        unpack_size,
        crc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip() {
        for v in [
            0u64,
            1,
            0x7F,
            0x80,
            0x3FFF,
            0x4000,
            0x1F_FFFF,
            0x20_0000,
            1 << 40,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            write_number(&mut out, v);
            let mut c = Cursor { data: &out, pos: 0 };
            assert_eq!(c.number(), Ok(v), "{v:#x}");
            assert_eq!(c.pos, out.len(), "{v:#x}");
        }
    }

    #[test]
    fn writer_and_reader_agree() {
        let stream = [0u8, 1, 2, 3, 4, 5, 6, 7, 8];
        let data = b"talsen quovar";
        let archive = write_archive(6, 1 << 20, &stream, data, "talsen.txt").expect("writes");
        let entry = read_archive(&archive).expect("reads");
        assert_eq!(entry.order, 6);
        assert_eq!(entry.mem, 1 << 20);
        assert_eq!(entry.stream, stream);
        assert_eq!(entry.unpack_size, data.len() as u64);
        assert_eq!(entry.crc, Some(crc32fast::hash(data)));
    }

    #[test]
    #[cfg_attr(miri, ignore = "no unsafe code; slow under Miri")]
    fn reader_rejects_damage_without_panicking() {
        let archive = write_archive(6, 1 << 20, &[0, 9, 9], b"kith", "kith.txt").expect("writes");
        for i in 0..archive.len() {
            let mut bent = archive.clone();
            bent[i] ^= 0x41;
            let _ = read_archive(&bent);
            let _ = read_archive(&archive[..i]);
        }
    }

    #[test]
    fn writer_refuses_an_empty_file() {
        assert!(write_archive(6, 1 << 20, &[0], b"", "e").is_err());
    }
}
