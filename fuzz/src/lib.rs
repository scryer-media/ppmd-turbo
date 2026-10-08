//! The shared half of ppmd-turbo's fuzz harness.
//!
//! Every target in `fuzz_targets/` is a thin `fuzz_target!` over this crate:
//!
//! - [`params`] maps raw input bytes to bounded model parameters, so a
//!   campaign's per-iteration memory stays small enough for a shared box;
//! - [`layout`] is the byte layout each target reads, and the inverse the
//!   seed generator writes;
//! - [`synth`] is the std-only core shared with the root crate's hostile
//!   suites: SplitMix64, the invented payloads (text from a made-up
//!   vocabulary, runs, ramps, structured and random binary), ppmd-rust's
//!   encoders and the hostile-test fixtures;
//! - [`payload`] re-exports the payloads with the fuzz-only `Arbitrary`;
//! - [`reference`](mod@reference) is ppmd-rust 1.5.0, the in-process reference;
//! - [`api`] is the shim over ppmd-turbo's decoder and encoder API; every
//!   entry routes to ppmd-turbo (see `docs/testing.md`);
//! - [`outcome`] turns a decode into an output plus a verdict, and decides
//!   when two verdicts agree;
//! - [`mutate`] is the structure-aware input of `structure_7z`;
//! - [`paths`] is `checked_vs_unchecked`: the fast decode path against the
//!   checked one;
//! - [`ops`] is `model_ops`: operation sequences against the long-lived
//!   model APIs;
//! - [`seeds`] generates the seed corpora, the fuzz regressions and the
//!   hostile-test fixtures under `tests/hostile_fixtures`. None of them is
//!   committed: `cargo run --locked -p ppmd-corpus -- fixtures` writes them.

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
pub mod synth;

pub use synth::SplitMix64;
