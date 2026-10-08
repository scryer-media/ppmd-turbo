//! The byte layout each target reads, and its inverse for the seeds.
//!
//! Layouts are plain prefixes rather than `arbitrary` structures so a seed is
//! a header followed by a real stream, and libFuzzer's mutations land on the
//! stream bytes. `structure_7z` is the exception; see [`crate::mutate`].

use crate::params::{
    INVALID_MEMS, INVALID_ORDERS, INVALID_RAR_MEM_MB, MAX_DECODE_MEM, MAX_ROUNDTRIP_MEM, mem_from,
    mem_sel, order_from, order_sel, rar_mem_mb_from,
};

/// `decode_7z` and `decode_differential_7z`.
///
/// Layout: `order_sel, mem_exp_sel, mem_low (u16 LE), flags, size (u16 LE)`,
/// then the stream. Flags: bit 0 the size is known (else decode to the end
/// marker), bit 1 use an invalid order, bit 2 use an invalid memory size.
#[derive(Debug, Clone, Copy)]
pub struct Decode7z<'a> {
    /// Model order.
    pub order: u32,
    /// Arena size in bytes.
    pub mem: u32,
    /// The output size, when the container would know it.
    pub known: Option<usize>,
    /// The raw coder bytes.
    pub stream: &'a [u8],
}

/// Header length of [`Decode7z`].
pub const DECODE_7Z_HEADER: usize = 7;

impl<'a> Decode7z<'a> {
    /// Parses `data`; `None` when it is shorter than the header.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let (h, stream) = data.split_first_chunk::<DECODE_7Z_HEADER>()?;
        let flags = h[4];
        let order = if flags & 2 != 0 {
            INVALID_ORDERS[usize::from(h[0] & 3)]
        } else {
            order_from(h[0])
        };
        let mem = if flags & 4 != 0 {
            INVALID_MEMS[usize::from(h[1] & 3)]
        } else {
            mem_from(h[1], u16::from_le_bytes([h[2], h[3]]), MAX_DECODE_MEM)
        };
        let known = (flags & 1 != 0).then(|| usize::from(u16::from_le_bytes([h[5], h[6]])));
        Some(Self {
            order,
            mem,
            known,
            stream,
        })
    }

    /// A seed with legal parameters.
    pub fn seed(order: u32, mem: u32, known: Option<u16>, stream: &[u8]) -> Vec<u8> {
        let (e, low) = mem_sel(mem);
        let low = low.to_le_bytes();
        let size = known.unwrap_or(0).to_le_bytes();
        let flags = u8::from(known.is_some());
        let mut v = vec![order_sel(order), e, low[0], low[1], flags, size[0], size[1]];
        v.extend_from_slice(stream);
        v
    }

    /// A seed with an invalid order (`bad_order`) and/or memory size
    /// (`bad_mem`), each an index into the invalid lists.
    pub fn seed_invalid(bad_order: Option<u8>, bad_mem: Option<u8>, stream: &[u8]) -> Vec<u8> {
        let flags = (u8::from(bad_order.is_some()) << 1) | (u8::from(bad_mem.is_some()) << 2);
        let mut v = vec![
            bad_order.unwrap_or(4),
            bad_mem.unwrap_or(5),
            0,
            0,
            flags,
            0,
            0,
        ];
        v.extend_from_slice(stream);
        v
    }
}

/// One block of `decode_rar`.
#[derive(Debug, Clone, Copy)]
pub struct RarBlock<'a> {
    /// Start a fresh `RarDecoder` before this block (a new non-solid member).
    pub fresh: bool,
    /// The header's reset flag: rebuild the model.
    pub reset: bool,
    /// Model order after RAR's mapping.
    pub order: u32,
    /// Arena size in MiB.
    pub mem_mb: u32,
    /// The most symbols to decode.
    pub unpacked_remaining: u64,
    /// The range coder's bytes.
    pub rc_data: &'a [u8],
}

/// Header length of one [`RarBlock`].
pub const RAR_BLOCK_HEADER: usize = 7;

/// The most blocks one `decode_rar` input runs.
pub const MAX_RAR_BLOCKS: usize = 64;

impl<'a> RarBlock<'a> {
    /// Parses a block sequence. Each block: `flags, order_sel, mem_sel,
    /// unpacked (u16 LE), len (u16 LE)`, then `len` bytes (clipped to what is
    /// left). Flags: bit 0 reset, bit 1 invalid order, bit 2 invalid arena,
    /// bit 3 claim `u64::MAX` symbols remain, bit 4 fresh decoder.
    pub fn parse_all(mut data: &'a [u8]) -> Vec<Self> {
        let mut blocks = Vec::new();
        while blocks.len() < MAX_RAR_BLOCKS {
            let Some((h, rest)) = data.split_first_chunk::<RAR_BLOCK_HEADER>() else {
                break;
            };
            let flags = h[0];
            let len = usize::from(u16::from_le_bytes([h[5], h[6]])).min(rest.len());
            let (rc_data, rest) = rest.split_at(len);
            data = rest;
            blocks.push(Self {
                fresh: flags & 16 != 0,
                reset: flags & 1 != 0,
                order: if flags & 2 != 0 {
                    INVALID_ORDERS[usize::from(h[1] & 3)]
                } else {
                    order_from(h[1])
                },
                mem_mb: if flags & 4 != 0 {
                    INVALID_RAR_MEM_MB[usize::from(h[2]) % INVALID_RAR_MEM_MB.len()]
                } else {
                    rar_mem_mb_from(h[2])
                },
                unpacked_remaining: if flags & 8 != 0 {
                    u64::MAX
                } else {
                    u64::from(u16::from_le_bytes([h[3], h[4]]))
                },
                rc_data,
            });
        }
        blocks
    }

    /// One block of a seed, with legal parameters.
    pub fn seed(
        fresh: bool,
        reset: bool,
        order: u32,
        mem_mb: u32,
        unpacked: Option<u16>,
        rc_data: &[u8],
    ) -> Vec<u8> {
        assert!((1..=crate::params::MAX_RAR_MEM_MB).contains(&mem_mb));
        let flags = u8::from(reset) | (u8::from(unpacked.is_none()) << 3) | (u8::from(fresh) << 4);
        let len = u16::try_from(rc_data.len()).expect("seed block fits u16");
        let un = unpacked.unwrap_or(0).to_le_bytes();
        let len = len.to_le_bytes();
        let mut v = vec![
            flags,
            order_sel(order),
            (mem_mb - 1) as u8,
            un[0],
            un[1],
            len[0],
            len[1],
        ];
        v.extend_from_slice(rc_data);
        v
    }
}

/// `roundtrip_7z`.
///
/// Layout: `order_sel, mem_exp_sel, mem_low (u16 LE), flags`, then the
/// payload. Flags: bit 0 write an end marker.
#[derive(Debug, Clone, Copy)]
pub struct Roundtrip7z<'a> {
    /// Model order.
    pub order: u32,
    /// Arena size in bytes.
    pub mem: u32,
    /// Finish with an end marker.
    pub end_marker: bool,
    /// What to encode.
    pub payload: &'a [u8],
}

impl<'a> Roundtrip7z<'a> {
    /// Parses `data`.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let (h, payload) = data.split_first_chunk::<5>()?;
        Some(Self {
            order: order_from(h[0]),
            mem: mem_from(h[1], u16::from_le_bytes([h[2], h[3]]), MAX_ROUNDTRIP_MEM),
            end_marker: h[4] & 1 != 0,
            payload,
        })
    }

    /// A seed.
    pub fn seed(order: u32, mem: u32, end_marker: bool, payload: &[u8]) -> Vec<u8> {
        let (e, low) = mem_sel(mem);
        let low = low.to_le_bytes();
        let mut v = vec![order_sel(order), e, low[0], low[1], u8::from(end_marker)];
        v.extend_from_slice(payload);
        v
    }
}

/// `roundtrip_carryless`.
///
/// Layout: `order_sel, mem_sel, flags`, then the payload. The arena is
/// 1..=16 MiB so the stream also decodes as one RAR block. Flags: bit 0 write
/// an end marker.
#[derive(Debug, Clone, Copy)]
pub struct RoundtripCarryless<'a> {
    /// Model order.
    pub order: u32,
    /// Arena size in MiB.
    pub mem_mb: u32,
    /// Finish with an end marker.
    pub end_marker: bool,
    /// What to encode.
    pub payload: &'a [u8],
}

impl<'a> RoundtripCarryless<'a> {
    /// Parses `data`.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let (h, payload) = data.split_first_chunk::<3>()?;
        Some(Self {
            order: order_from(h[0]),
            mem_mb: rar_mem_mb_from(h[1]),
            end_marker: h[2] & 1 != 0,
            payload,
        })
    }

    /// The arena in bytes.
    pub fn mem(&self) -> u32 {
        self.mem_mb << 20
    }

    /// A seed.
    pub fn seed(order: u32, mem_mb: u32, end_marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![order_sel(order), (mem_mb - 1) as u8, u8::from(end_marker)];
        v.extend_from_slice(payload);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_parse_back() {
        let s = Decode7z::seed(16, 1 << 20, Some(1234), b"\0abc");
        let d = Decode7z::parse(&s).unwrap();
        assert_eq!(
            (d.order, d.mem, d.known, d.stream),
            (16, 1 << 20, Some(1234), &b"\0abc"[..])
        );

        let s = Decode7z::seed_invalid(Some(2), None, b"");
        let d = Decode7z::parse(&s).unwrap();
        assert_eq!(d.order, 65);

        let mut s = RarBlock::seed(true, true, 64, 3, Some(9), b"xyz");
        s.extend(RarBlock::seed(false, false, 2, 1, None, b"q"));
        let b = RarBlock::parse_all(&s);
        assert_eq!(b.len(), 2);
        assert!(b[0].fresh && b[0].reset);
        assert_eq!(
            (b[0].order, b[0].mem_mb, b[0].unpacked_remaining),
            (64, 3, 9)
        );
        assert_eq!(b[0].rc_data, b"xyz");
        assert_eq!(b[1].unpacked_remaining, u64::MAX);

        let s = Roundtrip7z::seed(6, 1 << 16, true, b"p");
        let r = Roundtrip7z::parse(&s).unwrap();
        assert_eq!(
            (r.order, r.mem, r.end_marker, r.payload),
            (6, 1 << 16, true, &b"p"[..])
        );

        let s = RoundtripCarryless::seed(8, 2, false, b"p");
        let r = RoundtripCarryless::parse(&s).unwrap();
        assert_eq!((r.order, r.mem(), r.end_marker), (8, 2 << 20, false));
    }
}
