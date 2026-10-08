//! F4: arbitrary bytes as a run of up to 64 RAR PPMd blocks through
//! long-lived decoders: reset and no-reset blocks (solid continuation of the
//! model), fresh decoders (a new non-solid member), invalid orders and arena
//! sizes, and output sizes that lie (`u64::MAX` symbols remaining).
//!
//! Every block must return an error or finish. It must never panic, never
//! read out of bounds, never claim to consume more than its data, and never
//! decode more than `unpacked_remaining` symbols; a block whose data is
//! exhausted must stop within the zero-padding guard rather than decoding
//! forever. Arenas are capped at 16 MiB. Input layout:
//! `ppmd_turbo_fuzz::layout::RarBlock`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::api::RarSession;
use ppmd_turbo_fuzz::layout::RarBlock;
use ppmd_turbo_fuzz::outcome::ErrKind;

/// The most symbols one block may produce when its size is a lie: every
/// input byte, padding included, carries at most this many symbols. Generous;
/// a block that exceeds it is decoding without consuming input.
const SYMBOLS_PER_BYTE: u64 = 1 << 12;

fuzz_target!(|data: &[u8]| {
    let blocks = RarBlock::parse_all(data);
    let Some(mut session) = RarSession::new() else {
        return;
    };
    let mut have_model = false;
    let mut out = Vec::new();
    for block in blocks {
        if block.fresh {
            let Some(s) = RarSession::new() else { return };
            session = s;
            have_model = false;
        }
        out.clear();
        let result = session.decode_block(
            block.reset,
            block.order,
            block.mem_mb,
            block.rc_data,
            block.unpacked_remaining,
            &mut out,
        );
        // RAR's MaxMB is a byte, so a legal arena is 1..=256 MiB.
        let legal = (2..=64).contains(&block.order) && (1..=256).contains(&block.mem_mb);
        let has_data = !block.rc_data.is_empty();
        match result {
            Ok(consumed) => {
                let len = block.rc_data.len();
                assert!(consumed <= len, "consumed {consumed} of {len}");
                assert!(out.len() as u64 <= block.unpacked_remaining);
                let bound = (len as u64 + 1024).saturating_mul(SYMBOLS_PER_BYTE);
                assert!(
                    out.len() as u64 <= bound,
                    "{} symbols from {len} bytes",
                    out.len()
                );
                if block.reset && has_data {
                    assert!(
                        legal,
                        "accepted order {} arena {} MiB",
                        block.order, block.mem_mb
                    );
                }
                if has_data {
                    assert!(
                        block.reset || have_model,
                        "decoded a no-reset block with no model"
                    );
                }
            }
            Err(kind) => {
                if block.reset && !legal && has_data {
                    assert_eq!(kind, ErrKind::InvalidParameters);
                }
            }
        }
        // A legal reset may build the model even if the block then fails, so
        // only "no legal reset since the decoder was made" is asserted on.
        have_model |= block.reset && legal && has_data;
    }
});
