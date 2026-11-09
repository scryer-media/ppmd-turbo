//! F11: arbitrary bytes decoded twice, once in the crate's own zeroed arena
//! and once in garbage-filled memory an `ArenaAllocator` lends, through the
//! same split points. The two must agree on every byte, verdict and error.
//!
//! Input: a mode byte (bit 0: RAR, else 7z), a garbage seed byte, then a
//! `decode_7z` input (`ppmd_turbo_fuzz::layout::Decode7z`) or a `decode_rar`
//! input (`ppmd_turbo_fuzz::layout::RarBlock`). The 7z case also decodes a
//! third time in the lent arena handed back from the second, so a reused
//! arena's leftover bytes are covered too. Every arena is lent (an in-RAM
//! limit of 0), so every restart to another size asks the allocator again.
#![no_main]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use libfuzzer_sys::fuzz_target;
use ppmd_turbo::{Arena, ArenaAllocError, ArenaAllocator, ArenaMemory};
use ppmd_turbo_fuzz::SplitMix64;
use ppmd_turbo_fuzz::api::{self, RarSession};
use ppmd_turbo_fuzz::layout::{Decode7z, RarBlock};
use ppmd_turbo_fuzz::params::OUTPUT_CAP;

/// Heap memory filled with garbage.
struct Garbage(Box<[u8]>);

// SAFETY: the boxed slice is owned by this value alone, initialized, and
// never moved or freed until the value drops; moving the `Box` holding the
// value does not move the slice's heap allocation.
unsafe impl ArenaMemory for Garbage {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// Lends garbage-filled memory, a different garbage for every arena, and
/// sometimes a little more than asked.
struct GarbageAllocator(AtomicU64);

impl ArenaAllocator for GarbageAllocator {
    fn allocate(&self, len: usize) -> Result<Box<dyn ArenaMemory>, ArenaAllocError> {
        let mut rng = SplitMix64::new(self.0.fetch_add(1, Ordering::Relaxed));
        let extra = rng.below(8) as usize;
        let mut bytes = vec![0u8; len + extra].into_boxed_slice();
        for chunk in bytes.chunks_mut(8) {
            let word = rng.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(Box::new(Garbage(bytes)))
    }
}

fn lender(seed: u8) -> Arc<GarbageAllocator> {
    Arc::new(GarbageAllocator(AtomicU64::new(
        0x4C45_4E54_0000_0000 | u64::from(seed),
    )))
}

fuzz_target!(|data: &[u8]| {
    let [mode, seed, rest @ ..] = data else {
        return;
    };
    if mode & 1 == 0 {
        let Some(case) = Decode7z::parse(rest) else {
            return;
        };
        let Some(owned) = api::decode_7z(case.stream, case.order, case.mem, case.known, OUTPUT_CAP)
        else {
            return;
        };
        let arena = Arena::with_allocator(0, lender(*seed));
        let (lent, arena) = api::decode_7z_in(
            case.stream,
            case.order,
            case.mem,
            case.known,
            OUTPUT_CAP,
            arena,
        );
        assert_eq!(lent, owned, "lent memory decoded differently");
        if let Some(arena) = arena {
            let (reused, _) = api::decode_7z_in(
                case.stream,
                case.order,
                case.mem,
                case.known,
                OUTPUT_CAP,
                arena,
            );
            assert_eq!(reused, owned, "a reused lent arena decoded differently");
        }
    } else {
        let alloc = lender(*seed);
        let fresh = || {
            (
                RarSession::new().expect("a RAR session"),
                RarSession::with_allocator(0, alloc.clone()),
            )
        };
        let (mut owned, mut lent) = fresh();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for block in RarBlock::parse_all(rest) {
            if block.fresh {
                (owned, lent) = fresh();
            }
            a.clear();
            b.clear();
            let decode = |s: &mut RarSession, out: &mut Vec<u8>| {
                s.decode_block(
                    block.reset,
                    block.order,
                    block.mem_mb,
                    block.rc_data,
                    block.unpacked_remaining,
                    out,
                )
            };
            let ra = decode(&mut owned, &mut a);
            let rb = decode(&mut lent, &mut b);
            assert_eq!(ra, rb, "lent memory ended a block differently");
            assert!(a == b, "lent memory decoded a block differently");
        }
    }
});
