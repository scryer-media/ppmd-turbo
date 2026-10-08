//! F1 differential: the same input as `decode_7z`, decoded by ppmd-rust
//! 1.5.0's `Ppmd7Decoder` and by ppmd-turbo. The outputs and the error/ok
//! verdict must agree byte for byte, with the allowances documented on
//! `ppmd_turbo_fuzz::outcome::agree` (error classes may differ; ppmd-rust
//! treats the end of its input as the end of the data).
//!
//! Arenas are capped at 64 MiB; both decoders hold one, so an iteration
//! stays under 128 MiB of arena.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo_fuzz::layout::Decode7z;
use ppmd_turbo_fuzz::outcome::agree;
use ppmd_turbo_fuzz::params::OUTPUT_CAP;
use ppmd_turbo_fuzz::{api, reference};

fuzz_target!(|data: &[u8]| {
    let Some(case) = Decode7z::parse(data) else {
        return;
    };
    let Some(ours) = api::decode_7z(case.stream, case.order, case.mem, case.known, OUTPUT_CAP)
    else {
        return;
    };
    let theirs = reference::decode_7z(case.stream, case.order, case.mem, case.known, OUTPUT_CAP);
    if let Err(why) = agree(&theirs, &ours) {
        panic!(
            "order {} mem {} known {:?}: {why}",
            case.order, case.mem, case.known
        );
    }
});
