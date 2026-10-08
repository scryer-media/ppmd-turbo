//! Hostile input against both decoder APIs: truncation, bit flips,
//! out-of-range parameters, tiny arenas, restart storms, empty input, size
//! lies, the carry-less coder's range-below-total fault and the 7z end-marker
//! edge cases.
//!
//! Every decode runs inside [`no_panic`], so a panic fails the test with the
//! input in hex. Expected errors follow `docs/algorithms.md` (sections 4.2,
//! 5.1 and 5.2) where it fixes them; where it does not, the accepted set is
//! spelled out at the assertion. All inputs are the generated fixtures in
//! `tests/hostile_fixtures` or derived from them deterministically.

mod hostile_support;

use hostile_support::api::{self, RarDecoder};
use hostile_support::{
    Coder, Fixture, Kind, Rng, assert_err, cut_points, fixture, fixtures, fixtures_of, no_panic,
};
use ppmd_turbo::{PPMD7_MAX_MEM_SIZE, PPMD7_MIN_MEM_SIZE, Result};

fn decode_7z(f: &Fixture, stream: &[u8], known: Option<u64>) -> Result<Vec<u8>> {
    no_panic(&format!("{} 7z decode", f.name), stream, || {
        api::decode_7z(stream, f.order, f.mem, known)
    })
}

/// One reset block on a fresh decoder.
fn decode_rar(f: &Fixture, rc: &[u8], remaining: u64) -> (Result<usize>, Vec<u8>) {
    no_panic(&format!("{} RAR decode", f.name), rc, || {
        let mut out = Vec::new();
        let r = RarDecoder::new().decode_block(true, f.order, f.mem_mb(), rc, remaining, &mut out);
        (r, out)
    })
}

/// The size a RAR caller would pass: the payload length, plus slack when the
/// stream carries an end marker that must stop the block on its own.
fn rar_remaining(f: &Fixture) -> u64 {
    f.payload.len() as u64 + if f.end_marker { 1000 } else { 0 }
}

/// The size a 7z caller would pass: known without an end marker, unknown
/// with one.
fn z7_known(f: &Fixture) -> Option<u64> {
    (!f.end_marker).then_some(f.payload.len() as u64)
}

const BAD_STREAM: &[Kind] = &[Kind::Truncated, Kind::Corrupt];

// ---------------------------------------------------------------------------
// Harness checks: these run now.

#[test]
#[cfg_attr(miri, ignore = "reads fixture files; Miri isolates the file system")]
fn fixtures_are_consistent() {
    let all = fixtures();
    assert!(all.iter().any(|f| f.coder == Coder::SevenZ));
    assert!(all.iter().any(|f| f.coder == Coder::CarryLess));
    for f in &all {
        assert!((2..=64).contains(&f.order), "{}", f.name);
        assert!(
            (PPMD7_MIN_MEM_SIZE..=PPMD7_MAX_MEM_SIZE).contains(&f.mem),
            "{}",
            f.name
        );
        match f.coder {
            // The 7z coder's first byte is always 0 (`Ppmd7z_RangeDec_Init`).
            Coder::SevenZ => assert_eq!(f.stream.first(), Some(&0), "{}", f.name),
            Coder::CarryLess => {
                assert!(f.stream.len() >= 4, "{}", f.name);
                assert_eq!(f.mem % (1 << 20), 0, "{}: RAR arenas are whole MiB", f.name);
            }
        }
    }
}

#[test]
fn cut_points_are_deterministic_and_in_range() {
    let a = cut_points("x", 1000);
    assert_eq!(a, cut_points("x", 1000));
    assert!(a.iter().all(|&c| c < 1000));
    assert!((0..=64).all(|c| a.contains(&c)));
    assert!(a.len() > 65);
    assert_eq!(cut_points("y", 10), (0..10).collect::<Vec<_>>());
    assert!(cut_points("z", 0).is_empty());
}

#[test]
fn no_panic_reports_the_input() {
    let r = std::panic::catch_unwind(|| no_panic("probe", &[0xAB, 0xCD], || panic!("boom")));
    let msg = r.expect_err("must panic");
    let msg = msg.downcast_ref::<String>().expect("String payload");
    assert!(msg.contains("abcd") && msg.contains("boom"), "{msg}");
}

// ---------------------------------------------------------------------------
// Truncation.

#[test]
fn z7_truncation_never_yields_wrong_output() {
    for f in fixtures_of(Coder::SevenZ) {
        for cut in cut_points(&f.name, f.stream.len()) {
            let stream = &f.stream[..cut];
            // A truncated stream either still had every byte the symbols
            // needed (the flush bytes are partly slack) or reports the
            // overrun: 7-Zip's Extra flag is an error (algorithms.md 5.2).
            for known in [z7_known(&f), Some(f.payload.len() as u64)] {
                match decode_7z(&f, stream, known) {
                    Ok(out) => assert!(out == f.payload, "{} cut {cut}: wrong output", f.name),
                    r => assert_err(&format!("{} cut {cut}", f.name), &r, BAD_STREAM),
                }
            }
        }
    }
}

#[test]
fn rar_truncation_is_bounded() {
    for f in fixtures_of(Coder::CarryLess) {
        for cut in cut_points(&f.name, f.stream.len()) {
            let rc = &f.stream[..cut];
            let remaining = rar_remaining(&f);
            let (r, out) = decode_rar(&f, rc, remaining);
            // Zero bytes past the end are legal up to a guard (algorithms.md
            // D1, unrar-rs's EOF guard), so a short block may still decode.
            match r {
                Ok(consumed) => {
                    assert!(consumed <= cut, "{} cut {cut}: consumed {consumed}", f.name);
                    assert!(out.len() as u64 <= remaining);
                }
                Err(e) => assert!(
                    BAD_STREAM.contains(&hostile_support::kind(&e)),
                    "{} cut {cut}: {e:?}",
                    f.name
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bit flips in the first 64 bytes.

#[test]
fn z7_bit_flips_are_errors_or_full_length() {
    for f in fixtures_of(Coder::SevenZ) {
        for i in 0..f.stream.len().min(64) {
            for mask in [0x01, 0x80, 0xFF] {
                let mut s = f.stream.clone();
                s[i] ^= mask;
                let what = format!("{} byte {i} ^ {mask:#04x}", f.name);
                let known = Some(f.payload.len() as u64);
                match decode_7z(&f, &s, known) {
                    Ok(out) => assert_eq!(out.len(), f.payload.len(), "{what}"),
                    r => assert_err(&what, &r, BAD_STREAM),
                }
                if i == 0 {
                    // The 7z coder's first byte must be 0 (algorithms.md 5.2,
                    // backlog test 4).
                    let r = decode_7z(&f, &s, known);
                    assert_err(&what, &r, &[Kind::Corrupt]);
                }
                let r = decode_7z(&f, &s, None);
                if let Ok(out) = &r {
                    assert!(out.len() <= api::OUTPUT_CAP, "{what}");
                }
            }
        }
    }
}

#[test]
fn rar_bit_flips_are_bounded() {
    for f in fixtures_of(Coder::CarryLess) {
        for i in 0..f.stream.len().min(64) {
            for mask in [0x01, 0x80, 0xFF] {
                let mut s = f.stream.clone();
                s[i] ^= mask;
                let remaining = rar_remaining(&f);
                let (r, out) = decode_rar(&f, &s, remaining);
                match r {
                    Ok(consumed) => {
                        assert!(consumed <= s.len());
                        assert!(out.len() as u64 <= remaining);
                    }
                    Err(e) => assert!(
                        BAD_STREAM.contains(&hostile_support::kind(&e)),
                        "{} byte {i}: {e:?}",
                        f.name
                    ),
                }
            }
        }
    }
}

#[test]
fn z7_code_all_ones_at_init_is_corrupt() {
    // `Code == 0xFFFFFFFF` after the five init bytes is an error
    // (`Ppmd7z_RangeDec_Init`; backlog test 4).
    let f = fixture("z7-text-o6-m64k");
    let mut s = f.stream.clone();
    s[1..5].fill(0xFF);
    assert_err(
        "code all ones",
        &decode_7z(&f, &s, Some(10)),
        &[Kind::Corrupt],
    );
}

// ---------------------------------------------------------------------------
// Parameters out of range.

#[test]
fn z7_order_and_memory_out_of_range() {
    let f = fixture("z7-text-o6-m64k");
    let known = Some(f.payload.len() as u64);
    for order in [0, 1, 65, 255, u32::MAX] {
        let r = no_panic("order", &f.stream, || {
            api::decode_7z(&f.stream, order, f.mem, known)
        });
        assert_err(&format!("order {order}"), &r, &[Kind::InvalidParameters]);
    }
    for mem in [
        0,
        1,
        PPMD7_MIN_MEM_SIZE - 1,
        PPMD7_MAX_MEM_SIZE + 1,
        u32::MAX,
    ] {
        let r = no_panic("mem", &f.stream, || {
            api::decode_7z(&f.stream, f.order, mem, known)
        });
        assert_err(&format!("mem {mem}"), &r, &[Kind::InvalidParameters]);
    }
}

#[test]
fn rar_order_and_memory_out_of_range() {
    let f = fixture("cl-text-o6-m1-eos");
    // Order 1 is rejected by RAR itself (algorithms.md 5.1); the mapped
    // order never exceeds 64. Arenas: zero, and sizes whose byte count does
    // not fit the 32-bit arena (RAR's MaxMB byte caps a real one at 256 MiB).
    let cases: [(u32, u32); 7] = [
        (0, 1),
        (1, 1),
        (65, 1),
        (u32::MAX, 1),
        (6, 0),
        (6, 4096),
        (6, u32::MAX),
    ];
    for (order, mem_mb) in cases {
        let r = no_panic("rar params", &f.stream, || {
            let mut out = Vec::new();
            RarDecoder::new().decode_block(true, order, mem_mb, &f.stream, 100, &mut out)
        });
        assert_err(
            &format!("order {order} mem {mem_mb} MiB"),
            &r,
            &[Kind::InvalidParameters],
        );
    }
}

// ---------------------------------------------------------------------------
// Tiny arenas and restarts.

#[test]
fn z7_tiny_arena_high_order_restarts_like_7zip() {
    // 16 KiB of text at order 64 in a 2 KiB arena: the model restarts over
    // and over, exactly where 7-Zip's does, so the stream ppmd-rust wrote
    // decodes (the restart is RestartModel, algorithms.md 1.3).
    let f = fixture("z7-long-o64-m2k");
    let out = decode_7z(&f, &f.stream, Some(f.payload.len() as u64)).expect("decodes");
    assert!(out == f.payload);
}

#[test]
fn z7_tiny_arena_garbage_terminates() {
    let f = fixture("z7-long-o64-m2k");
    let mut rng = Rng::new(0x6A72);
    for round in 0..8 {
        let mut s = rng.bytes(16 << 10);
        s[0] = 0;
        match decode_7z(&f, &s, None) {
            Ok(out) => assert!(out.len() <= api::OUTPUT_CAP, "round {round}"),
            r => assert_err(&format!("round {round}"), &r, BAD_STREAM),
        }
        let r = decode_7z(&f, &s, Some(1 << 20));
        if let Ok(out) = &r {
            assert_eq!(out.len(), 1 << 20, "round {round}");
        }
    }
}

#[test]
fn rar_restart_storm() {
    // The reset flag on every block: each block rebuilds the model, and the
    // output never depends on what came before.
    let f = fixture("cl-text-o6-m1-eos");
    let other = fixture("cl-records-o16-m1");
    let mut dec = RarDecoder::new();
    for i in 0..200 {
        let (fx, remaining) = if i % 3 == 2 {
            (&other, other.payload.len() as u64)
        } else {
            (&f, rar_remaining(&f))
        };
        let mut out = Vec::new();
        let r = no_panic("restart storm", &fx.stream, || {
            dec.decode_block(true, fx.order, fx.mem_mb(), &fx.stream, remaining, &mut out)
        });
        let consumed = r.unwrap_or_else(|e| panic!("block {i}: {e:?}"));
        assert!(consumed <= fx.stream.len());
        assert!(out == fx.payload, "block {i}: output differs");
    }
}

// ---------------------------------------------------------------------------
// Empty input and missing models.

#[test]
fn z7_zero_length_input() {
    let f = fixture("z7-text-o6-m64k");
    // Nothing asked for: either nothing to do, or the coder's five init
    // bytes are missing.
    match decode_7z(&f, &[], Some(0)) {
        Ok(out) => assert!(out.is_empty()),
        r => assert_err("empty, size 0", &r, &[Kind::Truncated]),
    }
    assert_err(
        "empty, size 1",
        &decode_7z(&f, &[], Some(1)),
        &[Kind::Truncated],
    );
    assert_err(
        "empty, unsized",
        &decode_7z(&f, &[], None),
        &[Kind::Truncated],
    );
    for n in 1..5 {
        let r = decode_7z(&f, &f.stream[..n], Some(1));
        assert_err(&format!("{n} init bytes"), &r, &[Kind::Truncated]);
    }
}

#[test]
fn z7_empty_payload_streams() {
    let f = fixture("z7-empty-o6-m64k");
    assert_eq!(decode_7z(&f, &f.stream, Some(0)).expect("sized"), b"");
    let f = fixture("z7-empty-o6-m64k-eos");
    assert_eq!(decode_7z(&f, &f.stream, None).expect("marker"), b"");
}

#[test]
fn rar_zero_length_block() {
    let f = fixture("cl-text-o6-m1-eos");
    let (r, out) = decode_rar(&f, &[], 100);
    assert!(out.is_empty());
    match r {
        Ok(consumed) => assert_eq!(consumed, 0),
        Err(e) => assert_eq!(hostile_support::kind(&e), Kind::Truncated, "{e:?}"),
    }
    for n in 1..4 {
        let (r, out) = decode_rar(&f, &f.stream[..n], 100);
        assert!(out.len() <= 100);
        if let Err(e) = r {
            assert!(
                BAD_STREAM.contains(&hostile_support::kind(&e)),
                "{n} bytes: {e:?}"
            );
        }
    }
}

#[test]
fn rar_no_reset_without_model_is_corrupt() {
    // algorithms.md 5.1: a no-reset block with no model is an error.
    let f = fixture("cl-text-o6-m1-eos");
    let r = no_panic("no model", &f.stream, || {
        let mut out = Vec::new();
        RarDecoder::new().decode_block(false, f.order, f.mem_mb(), &f.stream, 100, &mut out)
    });
    assert_err("no-reset first block", &r, &[Kind::Corrupt]);
}

#[test]
fn rar_solid_continuation_after_an_error_does_not_panic() {
    let f = fixture("cl-text-o6-m1-eos");
    let mut dec = RarDecoder::new();
    let mut flipped = f.stream.clone();
    flipped[10] ^= 0x55;
    for (reset, rc) in [
        (true, &flipped),
        (false, &f.stream),
        (false, &flipped),
        (true, &f.stream),
    ] {
        no_panic("solid run", rc, || {
            let mut out = Vec::new();
            let _ = dec.decode_block(reset, f.order, f.mem_mb(), rc, rar_remaining(&f), &mut out);
        });
    }
}

// ---------------------------------------------------------------------------
// Output size lies.

#[test]
fn rar_size_larger_than_payload_stops_at_end_marker() {
    let f = fixture("cl-text-o6-m1-eos");
    for remaining in [f.payload.len() as u64 + 1, 1 << 30, u64::MAX] {
        let (r, out) = decode_rar(&f, &f.stream, remaining);
        r.unwrap_or_else(|e| panic!("remaining {remaining}: {e:?}"));
        assert!(out == f.payload, "remaining {remaining}");
    }
}

#[test]
fn rar_size_larger_than_payload_without_marker_terminates() {
    // No end marker: past the real data the coder reads zero padding until
    // the guard, then reports truncation. It must stop either way.
    let f = fixture("cl-records-o16-m1");
    for remaining in [f.payload.len() as u64 + 64, u64::MAX] {
        let (r, out) = decode_rar(&f, &f.stream, remaining);
        let n = f.payload.len().min(out.len());
        assert!(
            out[..n] == f.payload[..n],
            "remaining {remaining}: payload prefix differs"
        );
        if let Err(e) = r {
            assert!(BAD_STREAM.contains(&hostile_support::kind(&e)), "{e:?}");
        }
        assert!(
            (out.len() as u64) < f.payload.len() as u64 + (1 << 24),
            "runaway: {}",
            out.len()
        );
    }
}

#[test]
fn z7_size_larger_than_payload() {
    let f = fixture("z7-text-o6-m64k");
    let known = Some(f.payload.len() as u64 + 16);
    match decode_7z(&f, &f.stream, known) {
        Ok(out) => assert!(out.starts_with(&f.payload) && out.len() == f.payload.len() + 16),
        r => assert_err("size lie", &r, BAD_STREAM),
    }
}

// ---------------------------------------------------------------------------
// The carry-less coder's range below the total.

#[test]
fn carryless_range_below_total_is_a_fault_not_a_division_by_zero() {
    // After normalisation the carry-less range is at least 2^15 (BOT), and
    // an escape total can reach about 39.8k (255 states near MAX_FREQ plus a
    // SEE escape of up to 8191), so `Range / total` can be 0. 7-Zip checks
    // first (`Ppmd7aDec.c:75`, `:230`); unrar divides and traps (algorithms.md
    // 4.2). Registers are crafted directly: no stream reaching this state
    // turned up in a bounded search, and the property is the coder's.
    let cases: [(u32, u32, u32, u32); 4] = [
        (0, 0x7FFF, 0x8000, 0x9000),
        (0x00FF_8000, 0x00FF_9000, 0x8000, 39_811),
        (0, 0, 0, 1),
        (0x1234_0000, 0x1234_0001, 1, u32::MAX),
    ];
    for (low, code, range, total) in cases {
        let faulted = no_panic("carry-less threshold", &total.to_le_bytes(), || {
            api::carryless_threshold_faults(low, code, range, total)
        });
        assert!(faulted, "range {range:#x} total {total} not reported");
    }
    let ok = no_panic("carry-less threshold", &[], || {
        api::carryless_threshold_faults(0, 0x1000, u32::MAX, 257)
    });
    assert!(!ok, "a sane state faulted");
}

// ---------------------------------------------------------------------------
// 7z end-marker edge cases (algorithms.md 5.2).

#[test]
fn z7_no_marker_with_known_size_decodes() {
    let f = fixture("z7-text-o6-m64k");
    let out = decode_7z(&f, &f.stream, Some(f.payload.len() as u64)).expect("decodes");
    assert!(out == f.payload);
}

#[test]
fn z7_marker_with_unknown_size_decodes() {
    for name in ["z7-text-o6-m64k-eos", "z7-ramp-o16-m1m-eos"] {
        let f = fixture(name);
        let out = decode_7z(&f, &f.stream, None).expect("decodes to the marker");
        assert!(out == f.payload, "{name}");
        // The marker with the size known too: the size is reached first.
        let out = decode_7z(&f, &f.stream, Some(f.payload.len() as u64)).expect("sized");
        assert!(out == f.payload, "{name} sized");
    }
}

#[test]
fn z7_marker_before_known_size_is_an_error() {
    let f = fixture("z7-text-o6-m64k-eos");
    let r = decode_7z(&f, &f.stream, Some(f.payload.len() as u64 + 10));
    assert_err("marker before size", &r, BAD_STREAM);
}

#[test]
fn z7_data_after_marker_is_not_decoded() {
    // The decoder stops at the marker; trailing bytes are not output. The
    // packed-size check (`consumed == packSize`, algorithms.md 5.2) belongs
    // to the container, so the stream API may also report them as corrupt.
    let f = fixture("z7-text-o6-m64k-eos");
    let mut s = f.stream.clone();
    s.extend_from_slice(b"trailing bytes after the end marker");
    match decode_7z(&f, &s, None) {
        Ok(out) => assert!(out == f.payload),
        r => assert_err("trailing data", &r, &[Kind::Corrupt]),
    }
}

#[test]
fn z7_no_marker_with_unknown_size_is_an_error() {
    // Without a marker or a size the decoder runs past the data: the Extra
    // flag (read past the input) is an error, never silent garbage.
    let f = fixture("z7-text-o6-m64k");
    let r = decode_7z(&f, &f.stream, None);
    assert_err("no marker, no size", &r, BAD_STREAM);
}
