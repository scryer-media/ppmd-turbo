//! Range coders.
//!
//! Two coders carry variant H in practice, and the context model is generic
//! over them through [`RangeDecoder`]:
//!
//! - RAR's carry-less range coder, Dmitry Subbotin's design, which RAR
//!   2.9 through 4.x uses for its PPMd blocks ([`RarRangeDecoder`]), and
//! - 7-Zip's LZMA-style range coder with carry propagation (Igor Pavlov's
//!   `Ppmd7z_RangeDec`), which the 7z `PPMD` method uses (not yet here).
//!
//! This module owns the [`RangeDecoder`] trait, the seam between the model
//! and the coders. The RAR decoder here ([`RarRangeDecoder`], with its
//! [`ByteSource`] input and resumable [`RangeCoderState`]) is the seed
//! implementation carried over from unrar-rs, kept in `rar_seed` so the
//! dedicated coder implementations can replace it without touching the
//! model or the RAR framing.

mod rar_seed;

pub use rar_seed::{ByteSource, RangeCoderState, RarRangeDecoder};

/// The operations the context model decodes symbols through.
///
/// These are the three operations 7-Zip's `Ppmd7` model calls on its range
/// decoder (`GetThreshold`, `Decode`, `DecodeBit`). A coder implements each
/// with its own arithmetic; the model's output is bit-exact with the
/// reference only if the coder's arithmetic is too:
///
/// - **RAR** (Subbotin's carry-less coder): `get_threshold(t)` sets
///   `range /= t` and returns `(code - low) / range`; `decode(s, n)` sets
///   `low += s * range`, `range *= n`, then normalizes; `decode_bit(s0, t)`
///   sets `range /= t`, compares `(code - low) / range` with `s0`, and
///   decodes `(0, s0)` or `(s0, t - s0)` the same way. Normalization
///   shifts in a byte while `(low ^ (low + range)) < 2^24`, or while
///   `range < 2^15` after setting `range = -low & (2^15 - 1)`. All
///   arithmetic is wrapping `u32`.
/// - **7z** (Pavlov's coder): `get_threshold(t)` sets `range /= t` and
///   returns `code / range`; `decode(s, n)` sets `code -= s * range`,
///   `range *= n`, then normalizes; `decode_bit(s0, t)` computes
///   `bound = (range / t) * s0` and keeps `[0, bound)` or `[bound, range)`.
///
/// For variant H the binary total is always `1 << 14` (`BIN_SCALE`); the
/// model passes it as a constant, so an inlined coder divides by a shift.
///
/// Every call to [`decode`](Self::decode) and [`decode_bit`](Self::decode_bit)
/// normalizes before it returns, so the model never normalizes explicitly.
pub trait RangeDecoder {
    /// Scales the current range by `total` and returns the cumulative count
    /// the next symbol falls under. A count of `total` or more means the
    /// stream is corrupt.
    fn get_threshold(&mut self, total: u32) -> u32;

    /// Consumes the symbol occupying `[start, start + size)` of the range
    /// last scaled by [`get_threshold`](Self::get_threshold), then
    /// normalizes.
    fn decode(&mut self, start: u32, size: u32);

    /// Decodes a binary decision over `total` (a power of two), where the
    /// first outcome has frequency `size0`. Returns 0 for the first outcome
    /// and 1 for the second, and normalizes before returning.
    fn decode_bit(&mut self, size0: u32, total: u32) -> u32;

    /// Whether the coder ever met a range scaled to zero, which it would
    /// have to divide by or could never normalize.
    ///
    /// A well-formed stream never gets there: the model's totals are bounded
    /// and normalization keeps the range large. A corrupt stream can drive
    /// a total past the range; the coder records that instead of dividing by
    /// zero, keeps its arithmetic defined, and the model turns the fault into
    /// [`Error::CorruptStream`](crate::Error::CorruptStream).
    ///
    /// The fault is sticky: once set, every later symbol decoded through
    /// this coder is reported corrupt too.
    fn faulted(&self) -> bool;
}
