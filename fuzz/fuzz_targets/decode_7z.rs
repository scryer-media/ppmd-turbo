//! Arbitrary bytes as 7z `PPMD` properties followed by a coded stream. The
//! decoder must return an error or finish; it must never panic and never read
//! out of bounds.
//!
//! Placeholder until the 7z stream decoder lands: it only checks the
//! properties against the crate's limits.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo::{PPMD7_MAX_MEM_SIZE, PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE, PPMD7_MIN_ORDER};

fuzz_target!(|data: &[u8]| {
    let Some((props, _stream)) = data.split_first_chunk::<5>() else {
        return;
    };
    let order = u32::from(props[0]);
    let mem = u32::from_le_bytes([props[1], props[2], props[3], props[4]]);
    let _valid = (PPMD7_MIN_ORDER..=PPMD7_MAX_ORDER).contains(&order)
        && (PPMD7_MIN_MEM_SIZE..=PPMD7_MAX_MEM_SIZE).contains(&mem);
});
