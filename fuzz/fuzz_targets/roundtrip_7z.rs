//! F3: an arbitrary payload through both 7z encoders and both decoders.
//!
//! - ppmd-turbo's encoder output is byte-identical to ppmd-rust's (and so to
//!   7-Zip's) for the same order, arena and end-marker choice;
//! - ppmd-turbo decodes ppmd-rust's stream back to the payload;
//! - ppmd-rust decodes ppmd-turbo's stream back to the payload;
//! - ppmd-turbo decodes its own stream back to the payload.
//!
//! The shim routes to ppmd-turbo's encoder; the branch for a shim that
//! returns no encoder (ppmd-rust's self round trip) is never taken. Arenas
//! are capped at 16 MiB.
//! Input layout: `ppmd_turbo_fuzz::layout::Roundtrip7z`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::layout::Roundtrip7z;
use ppmd_turbo_fuzz::outcome::Verdict;
use ppmd_turbo_fuzz::{api, reference};

fn decoded(output: &[u8], verdict: Verdict, payload: &[u8], end_marker: bool, who: &str) {
    let want = if end_marker {
        Verdict::Ended
    } else {
        Verdict::Complete
    };
    assert_eq!(verdict, want, "{who}: verdict");
    assert!(output == payload, "{who}: output differs from the payload");
}

fuzz_target!(|data: &[u8]| {
    let Some(case) = Roundtrip7z::parse(data) else {
        return;
    };
    let (order, mem, eos, payload) = (case.order, case.mem, case.end_marker, case.payload);
    // With an end marker, decode without a size: the marker must stop it.
    let known = (!eos).then_some(payload.len());
    let cap = payload.len() + 1;

    let theirs = reference::encode_7z(payload, order, mem, eos);
    let Some(ours) = api::encode_7z(payload, order, mem, eos) else {
        let r = reference::decode_7z(&theirs, order, mem, known, cap);
        decoded(
            &r.outcome.output,
            r.outcome.verdict,
            payload,
            eos,
            "ppmd-rust self round trip",
        );
        if let Some(d) = api::decode_7z(&theirs, order, mem, known, cap) {
            decoded(
                &d.output,
                d.verdict,
                payload,
                eos,
                "ppmd-turbo decoding ppmd-rust",
            );
        }
        return;
    };
    let ours = ours.expect("ppmd-turbo's 7z encoder refused legal parameters");
    assert!(
        ours == theirs,
        "encoder output differs: {} vs {} bytes",
        ours.len(),
        theirs.len()
    );

    if let Some(d) = api::decode_7z(&theirs, order, mem, known, cap) {
        decoded(
            &d.output,
            d.verdict,
            payload,
            eos,
            "ppmd-turbo decoding ppmd-rust",
        );
    }
    if let Some(d) = api::decode_7z(&ours, order, mem, known, cap) {
        decoded(
            &d.output,
            d.verdict,
            payload,
            eos,
            "ppmd-turbo decoding itself",
        );
    }
    let r = reference::decode_7z(&ours, order, mem, known, cap);
    decoded(
        &r.outcome.output,
        r.outcome.verdict,
        payload,
        eos,
        "ppmd-rust decoding ppmd-turbo",
    );
});
