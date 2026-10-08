//! F5: the fast decode path against the checked one, on F1 and F4 inputs
//! behind a mode byte. The two must agree on every byte, verdict and coder
//! position. Logic and layout: `ppmd_turbo_fuzz::paths`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ppmd_turbo_fuzz::paths::check(data));
