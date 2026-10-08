//! Bounded model parameters from raw input bytes.
//!
//! The decoder's memory is the sub-allocator arena, so clamping the memory
//! size is what keeps one iteration's footprint bounded. Orders cover the
//! whole legal range; out-of-range values come from a short explicit list so
//! the parameter checks are exercised without ever asking for a huge arena.

/// The largest arena the decode targets ask for: 64 MiB.
pub const MAX_DECODE_MEM: u32 = 64 << 20;

/// The largest arena the round-trip and structure targets ask for: 16 MiB.
/// They hold up to four arenas at once (two encoders, two decoders).
pub const MAX_ROUNDTRIP_MEM: u32 = 16 << 20;

/// The largest RAR arena, in MiB. A RAR block sequence may reset many times,
/// so the arena stays small.
pub const MAX_RAR_MEM_MB: u32 = 16;

/// The most output a decode target collects when no size is known. Garbage
/// can decode to many symbols per input byte; the cap keeps the run bounded.
pub const OUTPUT_CAP: usize = 1 << 20;

/// Orders outside variant H's 2..=64.
pub const INVALID_ORDERS: [u32; 4] = [0, 1, 65, 255];

/// 7z memory sizes outside 2 KiB..=`PPMD7_MAX_MEM_SIZE`.
pub const INVALID_MEMS: [u32; 4] = [0, 1, 2047, ppmd_turbo::Params::MAX_MEM + 1];

/// RAR arena sizes in MiB that must be refused: zero, and sizes whose byte
/// count does not fit the 32-bit arena (4096 MiB is 2^32 bytes).
pub const INVALID_RAR_MEM_MB: [u32; 3] = [0, 4096, u32::MAX];

/// A legal order, 2..=64.
pub fn order_from(sel: u8) -> u32 {
    2 + u32::from(sel) % 63
}

/// The selector [`order_from`] maps to `order` (2..=64).
pub fn order_sel(order: u32) -> u8 {
    assert!((2..=64).contains(&order), "order {order}");
    (order - 2) as u8
}

/// A legal memory size of at least 2 KiB and at most `max`: a power of two
/// from 2^11 to 2^26 picked by `exp_sel`, plus `low` below that power.
pub fn mem_from(exp_sel: u8, low: u16, max: u32) -> u32 {
    let base = 1u32 << (11 + u32::from(exp_sel) % 16);
    (base + u32::from(low) % base).min(max)
}

/// The `(exp_sel, low)` pair [`mem_from`] maps to `mem`, for the seed
/// generator. `mem` must be a power of two from 2^11 to 2^26, plus less than
/// 2^16 and less than that power.
pub fn mem_sel(mem: u32) -> (u8, u16) {
    let e = 31 - mem.leading_zeros();
    assert!((11..=26).contains(&e), "mem {mem}");
    let low = mem - (1 << e);
    let low = u16::try_from(low).expect("mem low part fits u16");
    ((e - 11) as u8, low)
}

/// A legal RAR arena size in MiB, 1..=[`MAX_RAR_MEM_MB`].
pub fn rar_mem_mb_from(sel: u8) -> u32 {
    1 + u32::from(sel) % MAX_RAR_MEM_MB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_invert() {
        for order in 2..=64 {
            assert_eq!(order_from(order_sel(order)), order);
        }
        for mem in [2048, 4096 + 7, 1 << 16, (1 << 20) + 1000, 64 << 20] {
            let (e, low) = mem_sel(mem);
            assert_eq!(mem_from(e, low, MAX_DECODE_MEM), mem);
        }
    }

    #[test]
    fn every_selector_is_in_range() {
        for sel in 0..=255u8 {
            assert!((2..=64).contains(&order_from(sel)));
            assert!((1..=MAX_RAR_MEM_MB).contains(&rar_mem_mb_from(sel)));
            for low in [0, 1, 0x7FFF, u16::MAX] {
                let mem = mem_from(sel, low, MAX_DECODE_MEM);
                assert!((ppmd_turbo::Params::MIN_MEM..=MAX_DECODE_MEM).contains(&mem));
            }
        }
    }
}
