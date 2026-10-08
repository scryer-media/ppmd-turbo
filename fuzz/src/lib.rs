//! The shared half of ppmd-turbo's fuzz harness.
//!
//! Every target in `fuzz_targets/` is a thin `fuzz_target!` over this crate:
//!
//! - [`params`] maps raw input bytes to bounded model parameters, so a
//!   campaign's per-iteration memory stays small enough for a shared box;
//! - [`layout`] is the byte layout each target reads, and the inverse the
//!   seed generator writes;
//! - [`payload`] generates invented payloads (text from a made-up vocabulary,
//!   runs, ramps, structured and random binary);
//! - [`reference`] is ppmd-rust 1.5.0, the in-process reference;
//! - [`api`] is the shim over ppmd-turbo's decoder and encoder API, which
//!   returns `None` until that API lands (see `docs/testing.md`);
//! - [`outcome`] turns a decode into an output plus a verdict, and decides
//!   when two verdicts agree;
//! - [`mutate`] is the structure-aware input of `structure_7z`;
//! - [`paths`] is `checked_vs_unchecked`: the fast decode path against the
//!   checked one;
//! - [`ops`] is `model_ops`: operation sequences against the long-lived
//!   model APIs;
//! - [`seeds`] generates the committed seed corpora and the hostile-test
//!   fixtures under `tests/hostile_fixtures`.

#![forbid(unsafe_code)]

pub mod api;
pub mod layout;
pub mod mutate;
pub mod ops;
pub mod outcome;
pub mod params;
pub mod paths;
pub mod payload;
pub mod reference;
pub mod seeds;

/// SplitMix64: the harness's only source of pseudo-randomness, so seeds,
/// payloads and fixtures are identical on every machine.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound > 0`).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// `len` pseudo-random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}
