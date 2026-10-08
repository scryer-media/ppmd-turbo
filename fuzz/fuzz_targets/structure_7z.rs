//! Structure-aware near-valid input: the fuzzer picks the order, arena,
//! payload generator and up to eight edits (bit flips, truncation, insertion,
//! deletion, trailing data); the harness encodes the generated payload with
//! ppmd-rust and applies the edits. Most iterations start from a stream that
//! decodes and step just off it.
//!
//! - With no edits, ppmd-turbo decodes the stream to the payload.
//! - With edits, ppmd-turbo and ppmd-rust agree as in
//!   `decode_differential_7z`, and ppmd-turbo never panics.
//!
//! Arenas are capped at 16 MiB and payloads at 16 KiB.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::mutate::Case;
use ppmd_turbo_fuzz::outcome::{Verdict, agree};
use ppmd_turbo_fuzz::{api, reference};

fuzz_target!(|case: Case| {
    let (order, mem) = (case.order(), case.mem());
    let payload = case.payload();
    let mut stream = reference::encode_7z(&payload, order, mem, case.end_marker);
    case.apply(&mut stream);
    let known = case.known_size.then_some(payload.len());
    let cap = payload.len() + 4096;

    let Some(ours) = api::decode_7z(&stream, order, mem, known, cap) else {
        return;
    };
    if case.edits.is_empty() && (known.is_some() || case.end_marker) {
        let want = if known.is_some() {
            Verdict::Complete
        } else {
            Verdict::Ended
        };
        assert_eq!(ours.verdict, want);
        assert!(ours.output == payload, "unedited stream decodes wrong");
    }
    let theirs = reference::decode_7z(&stream, order, mem, known, cap);
    if let Err(why) = agree(&theirs, &ours) {
        panic!(
            "order {order} mem {mem} known {known:?} edits {:?}: {why}",
            case.edits
        );
    }
});
