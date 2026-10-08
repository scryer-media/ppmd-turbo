//! Memory bounds under hostile input, measured with a counting allocator.
//!
//! The decoder's memory is its arena plus a small constant (coder state,
//! input buffer, SEE and BinSumm tables), whatever the input: a tiny arena
//! at a high order on a long input restarts the model instead of growing,
//! and a restart storm reuses the arena instead of leaking one per block.
//!
//! This is its own test binary so the global allocator counts nothing but
//! these tests, and the tests share one lock so they never overlap.

mod hostile_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use hostile_support::api::{self, RarDecoder};
use hostile_support::{Rng, fixture, no_panic};

/// Counts live bytes and the high-water mark since the last [`reset_peak`].
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` with the caller's layout and
// pointer unchanged, so `System`'s guarantees carry over; the counters are
// plain atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout` (the caller's
        // contract), so from `System` with the same layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and `new_size` is the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

/// Live bytes now.
fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Restarts the high-water mark at the live count and returns it.
fn reset_peak() -> usize {
    let now = live();
    PEAK.store(now, Ordering::Relaxed);
    now
}

/// Bytes above `base` at the high-water mark.
fn peak_above(base: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(base)
}

/// What a decoder may hold beyond its arena and the caller's output: the
/// input refill buffer (`docs/backlog.md` D1 allows up to 1 MiB), the
/// harness's 64 KiB read buffer, and the model's fixed tables.
const SLACK: usize = (1 << 20) + (64 << 10) + (64 << 10);

#[test]
fn counting_allocator_counts() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let base = reset_peak();
    let v = vec![0u8; 1 << 20];
    assert!(peak_above(base) >= 1 << 20);
    drop(v);
    assert!(live() <= base + 4096, "{} live above {base}", live());
}

#[test]
#[ignore = "awaiting decoder"]
fn z7_tiny_arena_long_input_stays_within_arena() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = fixture("z7-long-o64-m2k");
    let stream = f.stream.clone();
    let known = Some(f.payload.len() as u64);
    let base = reset_peak();
    let out = no_panic("tiny arena", &stream, || {
        api::decode_7z(&stream, f.order, f.mem, known)
    })
    .expect("decodes");
    let used = peak_above(base);
    let bound = f.mem as usize + 2 * f.payload.len() + SLACK;
    assert!(out == f.payload);
    assert!(used <= bound, "peak {used} bytes above base, bound {bound}");

    // Garbage at the same parameters, no size: the output cap bounds the
    // output; nothing else may grow.
    let mut rng = Rng::new(0x004D_454D);
    let mut garbage = rng.bytes(64 << 10);
    garbage[0] = 0;
    let base = reset_peak();
    let r = no_panic("tiny arena garbage", &garbage, || {
        api::decode_7z(&garbage, f.order, f.mem, None)
    });
    let used = peak_above(base);
    let bound = f.mem as usize + 2 * api::OUTPUT_CAP + SLACK;
    assert!(
        used <= bound,
        "peak {used} bytes above base, bound {bound} ({r:?})"
    );
}

#[test]
#[ignore = "awaiting decoder"]
fn rar_restart_storm_reuses_the_arena() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = fixture("cl-text-o6-m1-eos");
    let remaining = f.payload.len() as u64 + 1000;
    let mut dec = RarDecoder::new();
    let mut out = Vec::with_capacity(f.payload.len() + 1000);
    let block = |dec: &mut RarDecoder, out: &mut Vec<u8>| {
        out.clear();
        no_panic("restart storm", &f.stream, || {
            dec.decode_block(true, f.order, f.mem_mb(), &f.stream, remaining, out)
        })
        .expect("block decodes");
        assert!(*out == f.payload);
    };
    block(&mut dec, &mut out);
    let base = reset_peak();
    for _ in 0..100 {
        block(&mut dec, &mut out);
    }
    let after = live();
    // One arena may be swapped for another of the same size; never more.
    let arena = f.mem as usize;
    assert!(
        peak_above(base) <= arena + SLACK,
        "peak {} above base",
        peak_above(base)
    );
    assert!(
        after <= base + SLACK,
        "{} bytes more live after 100 resets",
        after - base
    );
}
