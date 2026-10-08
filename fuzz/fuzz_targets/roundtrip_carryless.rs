//! An arbitrary payload through ppmd-turbo's carry-less encoder and back
//! through its RAR block decoder: correctness only (this encoder is never
//! benchmarked). Cross-checks against ppmd-rust's `7a` coder, which shares
//! the carry-less coder and the variant H model:
//!
//! - ppmd-turbo's carry-less stream decodes, as one reset RAR block, to the
//!   payload;
//! - ppmd-rust's `7a` decoder decodes it to the payload too;
//! - ppmd-rust's `7a` stream decodes through the RAR block API to the payload.
//!
//! Arenas are 1..=16 MiB, whole MiB so the stream is a legal RAR block.
//! Input layout: `ppmd_turbo_fuzz::layout::RoundtripCarryless`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::api::{self, RarSession};
use ppmd_turbo_fuzz::layout::RoundtripCarryless;
use ppmd_turbo_fuzz::reference;

/// Decodes `stream` as one reset RAR block. Without an end marker the block
/// is told the exact size, as RAR's file header would.
fn rar_decode(stream: &[u8], case: &RoundtripCarryless<'_>) -> Option<Vec<u8>> {
    let mut session = RarSession::new()?;
    let remaining = if case.end_marker {
        case.payload.len() as u64 + 1
    } else {
        case.payload.len() as u64
    };
    let mut out = Vec::new();
    let consumed = session
        .decode_block(true, case.order, case.mem_mb, stream, remaining, &mut out)
        .unwrap_or_else(|e| panic!("RAR block decode of a valid stream failed: {e:?}"));
    assert!(consumed <= stream.len());
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let Some(case) = RoundtripCarryless::parse(data) else {
        return;
    };
    let payload = case.payload;
    let theirs = reference::encode_carryless(payload, case.order, case.mem(), case.end_marker);
    if let Some(out) = rar_decode(&theirs, &case) {
        assert!(out == payload, "ppmd-turbo decoding ppmd-rust 7a differs");
    }
    let Some(ours) = api::encode_carryless(payload, case.order, case.mem(), case.end_marker) else {
        return;
    };
    let ours = ours.expect("ppmd-turbo's carry-less encoder refused legal parameters");
    if let Some(out) = rar_decode(&ours, &case) {
        assert!(out == payload, "ppmd-turbo carry-less round trip differs");
    }
    let known = (!case.end_marker).then_some(payload.len());
    let r = reference::decode_carryless_trusted(
        &ours,
        case.order,
        case.mem(),
        known,
        payload.len() + 1,
    );
    assert!(
        r.outcome.output == payload,
        "ppmd-rust 7a decoding ppmd-turbo differs"
    );
});
