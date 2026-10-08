//! F6: arbitrary block start, forget, cleanup, model start, restart and
//! decode sequences against `RarPpmd` and the internals `Model` on hostile
//! streams; a restart must always give back a model that decodes a fresh
//! stream exactly. Logic and layout: `ppmd_turbo_fuzz::ops`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ppmd_turbo_fuzz::ops::run(data));
