//! Memory bounds under hostile input, measured with a counting allocator.
//!
//! The decoder's memory is its arena plus a small constant (coder state,
//! input buffer, SEE and BinSumm tables), whatever the input: a tiny arena
//! at a high order on a long input restarts the model instead of growing,
//! and a restart storm reuses the arena instead of leaking one per block.
//!
//! The counters are per thread: each test measures only the allocations made
//! on its own thread, which is where the decoder runs, so the harness and
//! other tests running at the same time cannot move them, however loaded the
//! machine is.

mod hostile_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use hostile_support::api::{self, RarDecoder};
use hostile_support::{Rng, fixture, no_panic};

/// Counts live bytes and the high-water mark since the last [`reset_peak`],
/// for the allocating thread.
struct Counting;

thread_local! {
    // Const-initialised with no destructor, so reading them never allocates
    // and they are usable for the thread's whole life, teardown included.
    // Signed: a block freed on another thread than the one that allocated it
    // moves the freeing thread's count below its own allocations.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

/// Adds `delta` bytes to this thread's live count and raises its peak.
fn count(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

fn bytes(n: usize) -> isize {
    isize::try_from(n).expect("allocation size fits isize")
}

// SAFETY: every method forwards to `System` with the caller's layout and
// pointer unchanged, so `System`'s guarantees carry over; the counters are
// const-initialised thread-locals and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            count(bytes(layout.size()));
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout (see the impl).
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(bytes(layout.size()));
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout` (the caller's
        // contract), so from `System` with the same layout.
        unsafe { System.dealloc(ptr, layout) };
        count(-bytes(layout.size()));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, and `new_size` is the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            count(bytes(new_size) - bytes(layout.size()));
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Live bytes on this thread now.
fn live() -> isize {
    LIVE.with(Cell::get)
}

/// Restarts this thread's high-water mark at its live count and returns it.
fn reset_peak() -> isize {
    let now = live();
    PEAK.with(|peak| peak.set(now));
    now
}

/// Bytes above `base` at this thread's high-water mark.
fn peak_above(base: isize) -> usize {
    usize::try_from(PEAK.with(Cell::get) - base).unwrap_or(0)
}

/// Bytes live on this thread above `base`, or 0 if fewer.
fn live_above(base: isize) -> usize {
    usize::try_from(live() - base).unwrap_or(0)
}

/// What a decoder may hold beyond its arena and the caller's output: the
/// input refill buffer (`docs/backlog.md` D1 allows up to 1 MiB), the
/// harness's 64 KiB read buffer, and the model's fixed tables.
const SLACK: usize = (1 << 20) + (64 << 10) + (64 << 10);

#[test]
fn counting_allocator_counts() {
    // Another thread holds 8 MiB across the measurement and allocates 16 MiB
    // more in the middle of it, as a concurrent test would; this thread's
    // counts must see none of it. The
    // threads hand off through channels, so the overlap is certain, not
    // timing-dependent.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let other = std::thread::spawn(move || {
        let big = vec![1u8; 8 << 20];
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let more = vec![2u8; 16 << 20];
        held_tx.send(()).unwrap();
        drop((big, more));
    });
    held_rx.recv().unwrap();

    let base = reset_peak();
    let v = vec![0u8; 1 << 20];
    let peak = peak_above(base);
    assert!(
        (1 << 20..(1 << 20) + 4096).contains(&peak),
        "peak {peak} above base"
    );
    release_tx.send(()).unwrap();
    held_rx.recv().unwrap();
    drop(v);
    assert!(
        live_above(base) <= 4096,
        "{} live above base",
        live_above(base)
    );
    assert!(peak_above(base) < (1 << 20) + 4096);
    other.join().unwrap();
}

#[test]
fn z7_tiny_arena_long_input_stays_within_arena() {
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
    let after = live_above(base);
    // One arena may be swapped for another of the same size; never more.
    let arena = f.mem as usize;
    assert!(
        peak_above(base) <= arena + SLACK,
        "peak {} above base",
        peak_above(base)
    );
    assert!(after <= SLACK, "{after} bytes more live after 100 resets");
}

/// `Params::memory_footprint` is exact: building a codec allocates exactly
/// that many bytes, holds them, and says so through its own
/// `memory_footprint`.
#[test]
fn memory_footprint_is_what_a_codec_allocates() {
    use ppmd_turbo::{
        CarrylessDecoder, CarrylessEncoder, Params, RarPpmd, SevenZDecoder, SevenZEncoder,
    };
    for (order, mem) in [(2, 2048), (6, 1 << 20), (64, (16 << 20) + 7)] {
        let params = Params::new(order, mem).expect("legal parameters");
        let want = params.memory_footprint() as usize;
        let check = |what: &str, held: u64, base: isize| {
            let got = (live_above(base), peak_above(base));
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
        let got = (live_above(base), peak_above(base));
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
