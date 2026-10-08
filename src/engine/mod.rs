//! The engine every framing shares: the batched symbol loop over the
//! context model (`run.rs`).
//!
//! The model (`crate::model`), its sub-allocator and SEE stay in their own
//! modules; the framings (`sevenz`, `rar`, `carryless`) own the stop rules
//! and call into this loop for the symbols in between.

pub(crate) mod run;
