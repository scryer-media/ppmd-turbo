//! F1: arbitrary bytes as a raw 7z `PPMD` stream, with an order of 2..=64
//! (or an invalid one), an arena of 2 KiB..=64 MiB (or an invalid size), and
//! either a known output size (the 7z shape) or decoding to the end marker.
//!
//! The decoder must return an error or finish. It must never panic, never
//! read out of bounds, and never hand out more than it was asked for; with
//! no known size the harness stops at `OUTPUT_CAP`. Input layout:
//! `ppmd_turbo_fuzz::layout::Decode7z`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::api;
use ppmd_turbo_fuzz::layout::Decode7z;
use ppmd_turbo_fuzz::outcome::{ErrKind, Verdict};
use ppmd_turbo_fuzz::params::OUTPUT_CAP;

fuzz_target!(|data: &[u8]| {
    let Some(case) = Decode7z::parse(data) else {
        return;
    };
    let legal = (ppmd_turbo::PPMD7_MIN_ORDER..=ppmd_turbo::PPMD7_MAX_ORDER).contains(&case.order)
        && (ppmd_turbo::PPMD7_MIN_MEM_SIZE..=ppmd_turbo::PPMD7_MAX_MEM_SIZE).contains(&case.mem);
    let Some(out) = api::decode_7z(case.stream, case.order, case.mem, case.known, OUTPUT_CAP)
    else {
        return;
    };
    let limit = case.known.unwrap_or(OUTPUT_CAP);
    assert!(
        out.output.len() <= limit,
        "{} bytes out, limit {limit}",
        out.output.len()
    );
    if !legal {
        assert_eq!(out.verdict, Verdict::Failed(ErrKind::InvalidParameters));
    }
    if out.verdict == Verdict::Complete {
        assert_eq!(Some(out.output.len()), case.known);
    }
});
