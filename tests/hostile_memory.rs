//! Memory bounds under hostile input, measured with a counting allocator.
//!
//! The decoder's memory is its arena plus a small constant (coder state,
//! input buffer, SEE and BinSumm tables), whatever the input: a tiny arena
//! at a high order on a long input restarts the model instead of growing,
//! and a restart storm reuses the arena instead of leaking one per block.
//!
//! This is its own test binary so the global allocator counts nothing but
//! these tests, and the tests share one lock so they never overlap. The
//! counts are per thread: the harness's own threads (and a finished test's
//! teardown) allocate and free on their own schedule, so a process-wide
//! count would depend on timing. Only the test's own thread moves its tally.

mod hostile_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;

use hostile_support::api::{self, RarDecoder};
use hostile_support::{Rng, fixture, no_panic};

/// Counts each thread's live bytes and high-water mark since the last
/// [`reset_peak`].
struct Counting;

thread_local! {
    /// This thread's live bytes and high-water mark: (live, peak). Both can
    /// dip below their start when the thread frees what another allocated,
    /// so they are signed.
    static MINE: std::cell::Cell<(isize, isize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Moves this thread's tally by `delta` bytes. The const-initialized cell
/// has no destructor and never allocates; `try_with` skips a thread that is
/// being torn down.
fn tally(delta: isize) {
    let _ = MINE.try_with(|m| {
        let (live, peak) = m.get();
        let live = live + delta;
        m.set((live, peak.max(live)));
    });
}

// SAFETY: every method forwards to `System` with the caller's layout and
// pointer unchanged, so `System`'s guarantees carry over; the counter is a
// const-initialized thread-local cell and never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            tally(layout.size() as isize);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            tally(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout` (the caller's
        // contract), so from `System` with the same layout.
        unsafe { System.dealloc(ptr, layout) };
        tally(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and `new_size` is the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            tally(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

/// This thread's live bytes now.
fn live() -> usize {
    MINE.with(|m| m.get()).0.max(0) as usize
}

/// Restarts this thread's high-water mark at its live count and returns it.
fn reset_peak() -> usize {
    MINE.with(|m| {
        let (live, _) = m.get();
        m.set((live, live));
        live.max(0) as usize
    })
}

/// Bytes above `base` at this thread's high-water mark.
fn peak_above(base: usize) -> usize {
    (MINE.with(|m| m.get()).1.max(0) as usize).saturating_sub(base)
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

/// `Params::memory_footprint` is exact: building a codec allocates exactly
/// that many bytes, holds them, and says so through its own
/// `memory_footprint`.
#[test]
fn memory_footprint_is_what_a_codec_allocates() {
    use ppmd_turbo::{
        CarrylessDecoder, CarrylessEncoder, Params, RarPpmd, SevenZDecoder, SevenZEncoder,
    };
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for (order, mem) in [(2, 2048), (6, 1 << 20), (64, (16 << 20) + 7)] {
        let params = Params::new(order, mem).expect("legal parameters");
        let want = params.memory_footprint() as usize;
        let check = |what: &str, held: u64, base: usize| {
            let got = (live() - base, peak_above(base));
            assert_eq!(got, (want, want), "{what} {order}/{mem}: (live, peak)");
            assert_eq!(held as usize, want, "{what} {order}/{mem}: reported");
        };

        let base = reset_peak();
        let d = SevenZDecoder::new(params, None).expect("decoder");
        check("7z decoder", d.memory_footprint(), base);
        drop(d);
        let base = reset_peak();
        let e = SevenZEncoder::new(params).expect("encoder");
        check("7z encoder", e.memory_footprint(), base);
        drop(e);
        let base = reset_peak();
        let d = CarrylessDecoder::new(params, None).expect("decoder");
        check("carry-less decoder", d.memory_footprint(), base);
        drop(d);
        let base = reset_peak();
        let e = CarrylessEncoder::new(params).expect("encoder");
        check("carry-less encoder", e.memory_footprint(), base);
        drop(e);
    }
    for mb in [1, 3] {
        let params = Params::rar(6, mb).expect("legal parameters");
        let want = params.memory_footprint() as usize;
        let mut rar = RarPpmd::new();
        assert_eq!(rar.memory_footprint(), 0);
        let base = reset_peak();
        rar.start_block(Some(params)).expect("model starts");
        let got = (live() - base, peak_above(base));
        assert_eq!(got, (want, want), "RAR {mb} MiB: (live, peak)");
        assert_eq!(
            rar.memory_footprint() as usize,
            want,
            "RAR {mb} MiB: reported"
        );
        // A restart at the same size allocates nothing.
        let base = reset_peak();
        rar.start_block(Some(params)).expect("model restarts");
        assert_eq!(peak_above(base), 0, "RAR {mb} MiB: restart allocated");
    }
}

/// An arena taken back with `into_arena` and handed to `with_arena` is the
/// same memory: the next codec allocates none of it again, whichever codec
/// family it moves between, and decodes as a fresh one would.
#[test]
fn a_reused_arena_is_the_same_memory() {
    use ppmd_turbo::{CarrylessDecoder, Params, SevenZDecoder, SevenZEncoder, SevenZStatus};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let big = Params::new(6, 4 << 20).expect("legal parameters");
    let small = Params::new(6, 3 << 20).expect("legal parameters");
    let payload = b"an invented line of text, repeated. ".repeat(64);
    let stream = api::encode_7z(&payload, 6, small.mem_size(), false).expect("encodes");

    let enc = SevenZEncoder::new(big).expect("encoder");
    #[cfg(feature = "internals")]
    let addr = enc.arena_addr();
    let arena = enc.into_arena();
    let capacity = arena.capacity();

    // 4 MiB covers 3 MiB within twice the need: reused, nothing allocated.
    let base = reset_peak();
    let mut dec =
        SevenZDecoder::with_arena(small, Some(payload.len() as u64), arena).expect("decoder");
    let peak = peak_above(base);
    assert!(peak < (1 << 20), "with_arena allocated {peak} bytes");
    #[cfg(feature = "internals")]
    assert_eq!(dec.arena_addr(), addr, "the arena moved");
    let mut out = vec![0u8; payload.len()];
    let step = dec.decode(&stream, true, &mut out).expect("decodes");
    assert_eq!(step.status, SevenZStatus::ReachedSize);
    assert!(out == payload);

    let arena = dec.into_arena();
    assert_eq!(arena.capacity(), capacity);
    let base = reset_peak();
    let carry = CarrylessDecoder::with_arena(small, None, arena).expect("decoder");
    assert!(peak_above(base) < (1 << 20));
    let arena = carry.into_arena();
    assert_eq!(arena.capacity(), capacity);

    // An arena more than twice the need is not kept: the codec allocates
    // its own and the big one is freed.
    let tiny = Params::new(6, 1 << 16).expect("legal parameters");
    let dec = SevenZDecoder::with_arena(tiny, None, arena).expect("decoder");
    assert_eq!(dec.memory_footprint(), tiny.memory_footprint());
}
