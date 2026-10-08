//! Reading the PPMd stream out of an existing RAR 2.9/3.x/4.x archive.
//!
//! Read-only by design: RAR archives can only be authored by RARLAB's `rar`,
//! and nothing here writes one. This is the smallest walk the corpus and the
//! bench driver need: the RAR 1.5-4.x block chain of one or more volumes,
//! the packed data of a member (concatenated across volumes for a split
//! member), the PPMd block header at its start, and the RAR3 escape layer
//! that turns the PPMd symbol stream back into the member's bytes.
//!
//! The block layout and the escape codes follow RARLAB's published unrar
//! source (`arcread.cpp`, `unpack30.cpp`, `model.cpp`), used as a reference
//! for reading only.

/// One file member's packed data and the facts the decoder needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The stored name.
    pub name: String,
    /// The packed bytes, concatenated across every volume the member spans.
    pub packed: Vec<u8>,
    /// The unpacked size.
    pub unpacked_len: u64,
    /// CRC-32 of the unpacked bytes, from the member's last header.
    pub crc32: u32,
    /// The compression method byte (0x30 store .. 0x35 best).
    pub method: u8,
    /// The member continues the previous member's solid state.
    pub solid: bool,
}

const MARKER: &[u8; 7] = b"Rar!\x1A\x07\x00";
const HEAD_FILE: u8 = 0x74;
const HEAD_END: u8 = 0x7B;

/// Reads every file member of a RAR 1.5-4.x volume set, given the volumes'
/// bytes in order. A member split across volumes comes out once, with its
/// packed parts joined.
pub fn members(volumes: &[&[u8]]) -> Result<Vec<Member>, String> {
    let mut out: Vec<Member> = Vec::new();
    let mut open: Option<Member> = None;
    for (index, volume) in volumes.iter().enumerate() {
        if volume.len() < MARKER.len() || &volume[..MARKER.len()] != MARKER {
            return Err(format!("volume {index}: not a RAR 1.5-4.x archive"));
        }
        let mut pos = MARKER.len();
        while pos + 7 <= volume.len() {
            let kind = volume[pos + 2];
            let flags = u16::from_le_bytes([volume[pos + 3], volume[pos + 4]]);
            let head_size = usize::from(u16::from_le_bytes([volume[pos + 5], volume[pos + 6]]));
            if head_size < 7 || pos + head_size > volume.len() {
                return Err(format!(
                    "volume {index}: block at {pos} overruns the volume"
                ));
            }
            let head = &volume[pos..pos + head_size];
            let mut add_size = 0u64;
            if kind == HEAD_FILE {
                if head_size < 32 {
                    return Err(format!("volume {index}: short file header at {pos}"));
                }
                let le32 = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().unwrap());
                let mut pack = u64::from(le32(7));
                let mut unp = u64::from(le32(11));
                let crc = le32(16);
                let method = head[25];
                let name_size = usize::from(u16::from_le_bytes([head[26], head[27]]));
                let mut name_at = 32;
                if flags & 0x100 != 0 {
                    if head_size < 40 {
                        return Err(format!("volume {index}: short large-file header at {pos}"));
                    }
                    pack |= u64::from(le32(32)) << 32;
                    unp |= u64::from(le32(36)) << 32;
                    name_at = 40;
                }
                let name = head
                    .get(name_at..name_at + name_size)
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
                let data_start = pos + head_size;
                let data_end = usize::try_from(pack)
                    .ok()
                    .and_then(|p| data_start.checked_add(p))
                    .filter(|&e| e <= volume.len())
                    .ok_or_else(|| format!("volume {index}: {name}: packed data overruns"))?;
                let data = &volume[data_start..data_end];
                let split_before = flags & 0x01 != 0;
                let split_after = flags & 0x02 != 0;
                let mut member = match (split_before, open.take()) {
                    (true, Some(m)) => m,
                    (true, None) => {
                        return Err(format!("volume {index}: {name} continues an unseen part"));
                    }
                    (false, _) => Member {
                        name,
                        packed: Vec::new(),
                        unpacked_len: unp,
                        crc32: crc,
                        method,
                        solid: flags & 0x10 != 0,
                    },
                };
                member.packed.extend_from_slice(data);
                member.unpacked_len = unp;
                member.crc32 = crc;
                if split_after {
                    open = Some(member);
                } else {
                    out.push(member);
                }
                add_size = pack;
            } else if flags & 0x8000 != 0 && head_size >= 11 {
                add_size = u64::from(u32::from_le_bytes(head[7..11].try_into().unwrap()));
            }
            if kind == HEAD_END {
                break;
            }
            pos = usize::try_from(add_size)
                .ok()
                .and_then(|a| (pos + head_size).checked_add(a))
                .ok_or("block size overflow")?;
        }
    }
    if let Some(m) = open {
        return Err(format!("{}: the last volume is missing", m.name));
    }
    Ok(out)
}

/// The first file block's packed data in a single volume, cut at the end of
/// the volume when the header claims more: the lenient walk the hostile
/// fixtures need, since their headers lie. Returns the data's offset, the
/// data and the claimed unpacked size.
pub fn first_file_data_lenient(volume: &[u8]) -> Option<(usize, &[u8], u64)> {
    if volume.get(..MARKER.len())? != MARKER {
        return None;
    }
    let mut pos = MARKER.len();
    while pos + 7 <= volume.len() {
        let kind = volume[pos + 2];
        let flags = u16::from_le_bytes([volume[pos + 3], volume[pos + 4]]);
        let head_size = usize::from(u16::from_le_bytes([volume[pos + 5], volume[pos + 6]]));
        if head_size < 7 {
            return None;
        }
        let le32 = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(
                volume.get(pos + at..pos + at + 4)?.try_into().ok()?,
            ))
        };
        if kind == HEAD_FILE {
            let pack = le32(7)? as usize;
            let unp = u64::from(le32(11)?);
            let start = pos + head_size;
            let end = start.saturating_add(pack).min(volume.len());
            return Some((start, volume.get(start..end)?, unp));
        }
        let add = if flags & 0x8000 != 0 {
            le32(7)? as usize
        } else {
            0
        };
        pos = pos.checked_add(head_size)?.checked_add(add)?;
    }
    None
}

/// A RAR3 PPMd block header: the bytes in front of the range coder's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PpmHeader {
    /// The raw flags byte (bit 7 PPM block, 0x20 reset, 0x40 escape byte
    /// follows, low five bits the order code).
    pub flags: u8,
    /// The model is rebuilt (always set on a member's first block).
    pub reset: bool,
    /// The model order after RAR's mapping (`(code + 1)`, then 16 + 3 per
    /// step above 16), when `reset`.
    pub order: u32,
    /// The sub-allocator size in MiB (`MaxMB + 1`), when `reset`.
    pub mem_mb: u32,
    /// The escape byte, when the header carries one.
    pub esc: Option<u8>,
    /// Header length: where the range coder's data starts.
    pub len: usize,
}

/// Parses the PPMd block header at the start of `data` (a member's packed
/// data begins byte-aligned). `None` when the first block is not a PPMd block
/// or the header is cut short.
pub fn ppm_header(data: &[u8]) -> Option<PpmHeader> {
    let flags = *data.first()?;
    if flags & 0x80 == 0 {
        return None;
    }
    let reset = flags & 0x20 != 0;
    let mut len = 1;
    let mut mem_mb = 0;
    if reset {
        mem_mb = u32::from(*data.get(len)?) + 1;
        len += 1;
    }
    let mut esc = None;
    if flags & 0x40 != 0 {
        esc = Some(*data.get(len)?);
        len += 1;
    }
    let mut order = u32::from(flags & 0x1F) + 1;
    if order > 16 {
        order = 16 + (order - 16) * 3;
    }
    Some(PpmHeader {
        flags,
        reset,
        order,
        mem_mb,
        esc,
        len,
    })
}

/// RAR3's default escape byte (`PPMEscChar` after `UnpInitData30`).
pub const DEFAULT_ESC: u8 = 2;

/// Why the escape layer stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The output reached the requested length.
    Full,
    /// Escape code 2: end of the member's data.
    EndOfFile,
}

/// What the escape layer cannot follow with a PPMd decoder alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// Escape code 0: the stream switches to an LZ block or new tables.
    BlockSwitch,
    /// Escape code 3: an embedded RarVM filter.
    VmFilter,
    /// A match reaches before the start of the output.
    BadDistance,
}

/// The RAR3 escape layer over a PPMd symbol stream (`Unpack::Unpack29`'s
/// PPM branch): the escape byte followed by 1 is a literal escape byte, 2 ends
/// the data, 4 is a long match (three distance bytes, one length byte), 5 a
/// run (one length byte, distance 1); 0 and 3 hand over to machinery outside
/// PPMd and are refused.
pub struct Unescaper {
    esc: u8,
    limit: usize,
    /// The decoded bytes so far.
    pub out: Vec<u8>,
    state: State,
    /// Symbols consumed so far.
    pub symbols: u64,
}

enum State {
    Normal,
    AfterEsc,
    Long { left: u8, distance: u32 },
    Run,
}

impl Unescaper {
    /// A layer producing at most `limit` bytes, with escape byte `esc`.
    pub fn new(esc: u8, limit: usize) -> Self {
        Self {
            esc,
            limit,
            out: Vec::with_capacity(limit),
            state: State::Normal,
            symbols: 0,
        }
    }

    /// Feeds one symbol. `Ok(Some(stop))` once the layer is done.
    pub fn push(&mut self, sym: u8) -> Result<Option<Stop>, Unsupported> {
        self.symbols += 1;
        match self.state {
            State::Normal => {
                if sym == self.esc {
                    self.state = State::AfterEsc;
                } else {
                    self.out.push(sym);
                }
            }
            State::AfterEsc => {
                self.state = State::Normal;
                match sym {
                    0 => return Err(Unsupported::BlockSwitch),
                    2 => return Ok(Some(Stop::EndOfFile)),
                    3 => return Err(Unsupported::VmFilter),
                    4 => {
                        self.state = State::Long {
                            left: 3,
                            distance: 0,
                        };
                    }
                    5 => self.state = State::Run,
                    _ => self.out.push(self.esc),
                }
            }
            State::Long { left, distance } => {
                if left > 0 {
                    self.state = State::Long {
                        left: left - 1,
                        distance: (distance << 8) | u32::from(sym),
                    };
                } else {
                    self.state = State::Normal;
                    self.copy(u32::from(sym) + 32, distance + 2)?;
                }
            }
            State::Run => {
                self.state = State::Normal;
                self.copy(u32::from(sym) + 4, 1)?;
            }
        }
        if self.out.len() >= self.limit {
            self.out.truncate(self.limit);
            return Ok(Some(Stop::Full));
        }
        Ok(None)
    }

    fn copy(&mut self, length: u32, distance: u32) -> Result<(), Unsupported> {
        let distance = distance as usize;
        if distance == 0 || distance > self.out.len() {
            return Err(Unsupported::BadDistance);
        }
        for _ in 0..length {
            let b = self.out[self.out.len() - distance];
            self.out.push(b);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_mapping() {
        // Order code 5 -> order 6, reset, 15 -> 16 MiB, no escape byte.
        let h = ppm_header(&[0x80 | 0x20 | 5, 15, 0xAA]).unwrap();
        assert_eq!(
            (h.reset, h.order, h.mem_mb, h.esc, h.len),
            (true, 6, 16, None, 2)
        );
        // Order code 31 -> 32 -> 16 + 16 * 3 = 64, escape byte present.
        let h = ppm_header(&[0x80 | 0x40 | 0x20 | 31, 0, 7]).unwrap();
        assert_eq!((h.order, h.mem_mb, h.esc, h.len), (64, 1, Some(7), 3));
        assert_eq!(ppm_header(&[0x05]), None);
        assert_eq!(ppm_header(&[0xA0]), None);
    }

    #[test]
    fn escape_codes() {
        let mut u = Unescaper::new(2, 100);
        for s in [b'a', b'b', 2, 1, 2, 5, 0, 2, 4, 0, 0, 0, 0] {
            assert_eq!(u.push(s), Ok(None));
        }
        // "ab" + literal 2 + run of 4 of the literal + 32-byte match at
        // distance 2.
        assert_eq!(&u.out[..7], &[b'a', b'b', 2, 2, 2, 2, 2]);
        assert_eq!(u.out.len(), 7 + 32);
        assert_eq!(u.push(2), Ok(None));
        assert_eq!(u.push(2), Ok(Some(Stop::EndOfFile)));
        let mut u = Unescaper::new(2, 10);
        assert_eq!(u.push(2), Ok(None));
        assert_eq!(u.push(0), Err(Unsupported::BlockSwitch));
    }
}
