//! Model parameters: the validated order and memory size every codec is
//! built from, their 7z and RAR encodings, and the memory they cost.

use crate::alloc::Arena as ModelArena;
use crate::error::{Error, Result};
use crate::model::Model;

/// The most input any decoder in the crate takes for one symbol, at the
/// largest order: `4 * (64 + 2)` bytes of the carry-less coder. A caller
/// that always presents at least this many bytes (or the last of its input)
/// never sees `NeedInput` with input left to give.
pub const MAX_INPUT_PER_SYMBOL: usize = 4 * (Params::MAX_ORDER as usize + 2);

/// The model order and arena size of a PPMd variant H stream, checked
/// against the limits variant H accepts.
///
/// One check, done once: every codec takes a `Params`, so nothing inside
/// the crate or its consumers re-validates loose integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Params {
    order: u8,
    mem_size: u32,
}

impl Params {
    /// The smallest model order (`PPMD7_MIN_ORDER` in 7-Zip).
    pub const MIN_ORDER: u32 = 2;
    /// The largest model order (`PPMD7_MAX_ORDER` in 7-Zip).
    pub const MAX_ORDER: u32 = 64;
    /// The smallest arena in bytes (`PPMD7_MIN_MEM_SIZE` in 7-Zip).
    pub const MIN_MEM: u32 = 1 << 11;
    /// The largest arena in bytes (`PPMD7_MAX_MEM_SIZE` in 7-Zip):
    /// `0xFFFF_FFFF - 12 * 3`, so the allocator's three trailing units still
    /// fit in 32-bit offsets.
    pub const MAX_MEM: u32 = 0xFFFF_FFFF - 12 * 3;
    /// The largest arena a RAR PPMd block header can declare, in MiB: the
    /// header stores `size - 1` in one byte.
    pub const RAR_MAX_MEM_MB: u32 = 256;

    /// Checks `order` against [`MIN_ORDER`](Self::MIN_ORDER)`..=`[`MAX_ORDER`](Self::MAX_ORDER)
    /// and `mem_size` against [`MIN_MEM`](Self::MIN_MEM)`..=`[`MAX_MEM`](Self::MAX_MEM).
    ///
    /// Errors: [`ErrorKind::InvalidParameters`](crate::ErrorKind::InvalidParameters)
    /// for either outside its range.
    pub const fn new(order: u32, mem_size: u32) -> Result<Self> {
        if order < Self::MIN_ORDER
            || order > Self::MAX_ORDER
            || mem_size < Self::MIN_MEM
            || mem_size > Self::MAX_MEM
        {
            return Err(Error::invalid_parameters());
        }
        Ok(Self {
            order: order as u8,
            mem_size,
        })
    }

    /// The nearest valid parameters: `order` and `mem_size` clamped into
    /// their ranges. For encoder options, which take any number.
    pub const fn clamped(order: u32, mem_size: u32) -> Self {
        let order = if order < Self::MIN_ORDER {
            Self::MIN_ORDER
        } else if order > Self::MAX_ORDER {
            Self::MAX_ORDER
        } else {
            order
        };
        let mem_size = if mem_size < Self::MIN_MEM {
            Self::MIN_MEM
        } else if mem_size > Self::MAX_MEM {
            Self::MAX_MEM
        } else {
            mem_size
        };
        Self {
            order: order as u8,
            mem_size,
        }
    }

    /// Reads the five property bytes a 7z `PPMD` coder carries: the order,
    /// then the arena size as a little-endian `u32`.
    ///
    /// Errors: [`ErrorKind::InvalidParameters`](crate::ErrorKind::InvalidParameters)
    /// for a property block that is not five bytes long or holds values
    /// outside the limits.
    pub fn from_7z_props(props: &[u8]) -> Result<Self> {
        let &[order, m0, m1, m2, m3] = props else {
            return Err(Error::invalid_parameters());
        };
        Self::new(u32::from(order), u32::from_le_bytes([m0, m1, m2, m3]))
    }

    /// The five property bytes a 7z `PPMD` coder carries.
    pub const fn to_7z_props(self) -> [u8; 5] {
        let m = self.mem_size.to_le_bytes();
        [self.order, m[0], m[1], m[2], m[3]]
    }

    /// The parameters of a RAR PPMd block header: the model order the header
    /// decodes to and the arena size in MiB (the header's byte plus one).
    ///
    /// Errors: [`ErrorKind::InvalidParameters`](crate::ErrorKind::InvalidParameters)
    /// for an order outside `2..=64` or a size outside
    /// `1..=`[`RAR_MAX_MEM_MB`](Self::RAR_MAX_MEM_MB).
    pub const fn rar(order: u32, mem_mb: u32) -> Result<Self> {
        if mem_mb < 1 || mem_mb > Self::RAR_MAX_MEM_MB {
            return Err(Error::invalid_parameters());
        }
        Self::new(order, mem_mb << 20)
    }

    /// The model order.
    pub const fn order(self) -> u32 {
        self.order as u32
    }

    /// The arena size in bytes.
    pub const fn mem_size(self) -> u32 {
        self.mem_size
    }

    /// The exact heap bytes a codec built with these parameters allocates:
    /// the arena (the size plus up to three bytes of alignment, as 7-Zip's
    /// `Ppmd7_Alloc` lays it out) and the model's tables. A codec allocates nothing else, so a
    /// caller can reserve this before building one.
    pub const fn memory_footprint(self) -> u64 {
        ModelArena::arena_bytes(self.mem_size) as u64 + core::mem::size_of::<Model>() as u64
    }

    /// The most input one symbol takes from the 7z coder: two bytes per
    /// coder operation, and at most `order + 1` operations per symbol (one
    /// per context the escape chain visits), plus one of slack. A non-final
    /// call to [`SevenZDecoder::decode`](crate::SevenZDecoder::decode) needs
    /// at least this many bytes to decode a symbol. The carry-less coder
    /// takes up to four bytes per operation; see
    /// [`RarPpmd::max_input_per_symbol`](crate::RarPpmd::max_input_per_symbol).
    pub const fn max_input_per_symbol(self) -> usize {
        sevenz_margin(self.order)
    }

    /// 7-Zip's `ReduceSize` (`PpmdEncoder.cpp`, `CEncProps::Normalize`):
    /// for an input of `input_len` bytes, shrinks the arena to the smallest
    /// power of two from 64 KiB that is at least 16 times the input, when
    /// that is smaller than the current size. The order is unchanged.
    /// Encoding a small input with the reduced parameters writes the same
    /// stream 7-Zip writes for it.
    pub const fn reduced_for(self, input_len: u64) -> Self {
        // 7-Zip keeps ReduceSize as a u32 and ignores sizes that do not fit.
        let reduce = if input_len < u32::MAX as u64 {
            input_len as u32
        } else {
            u32::MAX
        };
        const MULT: u32 = 16;
        let mut mem = self.mem_size;
        if mem / MULT > reduce {
            let mut i = 16;
            while i < 32 {
                let m = 1u32 << i;
                if reduce <= m / MULT {
                    if mem > m {
                        mem = m;
                    }
                    break;
                }
                i += 1;
            }
        }
        Self {
            order: self.order,
            mem_size: mem,
        }
    }
}

/// `2 * (order + 2)`: the 7z coder's per-symbol input bound.
pub const fn sevenz_margin(order: u8) -> usize {
    2 * (order as usize + 2)
}

/// `4 * (order + 2)`: the carry-less coder's per-symbol input bound.
pub const fn carryless_margin(order: u8) -> usize {
    4 * (order as usize + 2)
}

impl Params {
    pub(crate) const fn order_u8(self) -> u8 {
        self.order
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn limits_match_7zip() {
        assert_eq!(Params::MIN_ORDER, 2);
        assert_eq!(Params::MAX_ORDER, 64);
        assert_eq!(Params::MIN_MEM, 2048);
        assert_eq!(Params::MAX_MEM, 0xFFFF_FFDB);
        assert_eq!(MAX_INPUT_PER_SYMBOL, 264);
    }

    #[test]
    fn out_of_range_values_are_invalid() {
        for (order, mem) in [(1, 1 << 16), (65, 1 << 16), (6, 2047), (6, u32::MAX)] {
            assert_eq!(
                Params::new(order, mem).unwrap_err().kind,
                ErrorKind::InvalidParameters
            );
        }
        for (order, mb) in [(6, 0), (6, 257), (1, 1), (65, 1)] {
            assert!(Params::rar(order, mb).is_err());
        }
        assert_eq!(Params::rar(6, 256).unwrap().mem_size(), 256 << 20);
        assert_eq!(Params::clamped(0, 0), Params::new(2, 2048).unwrap());
        assert_eq!(
            Params::clamped(99, u32::MAX),
            Params::new(64, Params::MAX_MEM).unwrap()
        );
    }

    #[test]
    fn props_round_trip() {
        let p = Params::new(6, 16 << 20).unwrap();
        assert_eq!(p.to_7z_props(), [6, 0, 0, 0, 1]);
        assert_eq!(Params::from_7z_props(&p.to_7z_props()).unwrap(), p);
        assert!(Params::from_7z_props(&[6, 0, 0, 0]).is_err());
        assert!(Params::from_7z_props(&[6, 0, 0, 0, 1, 0]).is_err());
    }

    /// Hand-checked against `CEncProps::Normalize`: the arena shrinks to the
    /// first power of two `m >= 2^16` with `reduce <= m / 16`.
    #[test]
    fn reduce_size_follows_7zip() {
        let p = Params::new(6, 16 << 20).unwrap();
        assert_eq!(p.reduced_for(0).mem_size(), 1 << 16);
        assert_eq!(p.reduced_for(4096).mem_size(), 1 << 16);
        assert_eq!(p.reduced_for(4097).mem_size(), 1 << 17);
        assert_eq!(p.reduced_for(1 << 20).mem_size(), 16 << 20);
        assert_eq!(p.reduced_for(u64::MAX).mem_size(), 16 << 20);
        let small = Params::new(6, 4096).unwrap();
        assert_eq!(small.reduced_for(0), small);
    }
}
