//! Range coders.
//!
//! Two coders carry variant H in practice, and the context model is generic
//! over them through [`RangeDecoder`] and [`RangeEncoder`]:
//!
//! - Dmitry Subbotin's carry-less range coder, which RAR 2.9 through 4.x
//!   uses for its PPMd blocks and Dmitry Shkarin's `.pmd` (7-Zip's `Ppmd7a`)
//!   uses for its streams ([`CarrylessRangeDecoder`],
//!   [`CarrylessRangeEncoder`]; `carryless.rs`), and
//! - Igor Pavlov's LZMA-style range coder with carry propagation, which the
//!   7z `PPMD` method uses ([`SevenZipRangeDecoder`],
//!   [`SevenZipRangeEncoder`]; `sevenz.rs`).
//!
//! The decoders read through a [`RangeInput`] (`input.rs`): a borrowed
//! slice, an owned refill buffer over `std::io::Read`, or a window over a
//! [`ByteSource`] shared with another reader. The encoders write through a
//! [`RangeOutput`] (`output.rs`). Every coder is generic over its input or
//! output, so the per-byte path compiles to a buffer load or store with a
//! refill or flush only at the buffer's edge, and a batch loop in the model
//! decodes any number of symbols without a call through a trait object.
//!
//! **Corrupt input.** Neither decoder ever panics or divides by zero. A
//! range scaled to zero (a frequency total past the range, which only a
//! corrupt stream or a corrupt model state produces) or a symbol size of
//! zero sets a sticky fault ([`RangeDecoder::faulted`]) and leaves the
//! arithmetic defined; the model turns the fault into
//! [`Error::CorruptStream`](crate::Error::CorruptStream). The carry-less
//! decoder's RAR-style API ([`CarrylessRangeDecoder::get_current_count`])
//! returns the error directly.

mod carryless;
mod input;
mod output;
mod sevenz;

#[cfg(test)]
mod tests;

pub use carryless::{CarrylessRangeDecoder, CarrylessRangeEncoder, RangeCoderState};
pub use input::{
    ByteSource, DEFAULT_REFILL_SIZE, IntoRangeInput, RangeInput, ReadInput, SliceInput, SourceInput,
};
pub use output::{DEFAULT_FLUSH_SIZE, RangeOutput, SliceOutput, WriteOutput};
pub use sevenz::{SevenZipRangeDecoder, SevenZipRangeEncoder};

/// RAR's name for the carry-less decoder: RAR 2.9 through 4.x PPMd blocks
/// are coded with it.
pub type RarRangeDecoder<I> = CarrylessRangeDecoder<I>;

/// `kTopValue` / `TOP`: both coders shift a byte in or out while the range
/// is below 2^24 (`Ppmd7Dec.c`, `Ppmd7aDec.c`).
pub(crate) const TOP: u32 = 1 << 24;

/// `kBot` / `BOT`: the carry-less coder's lower bound on the range
/// (`Ppmd7aDec.c`).
pub(crate) const BOT: u32 = 1 << 15;

/// Binary contexts code against a total of `PPMD_BIN_SCALE = 1 << 14`
/// (`Ppmd.h`), computed by shift rather than division.
pub(crate) const BIN_TOTAL_BITS: u32 = 14;

/// `PPMD_BIN_SCALE`.
pub(crate) const BIN_TOTAL: u32 = 1 << BIN_TOTAL_BITS;

/// The error a coder fault becomes.
#[cold]
pub(crate) fn corrupt(_detail: &'static str) -> crate::Error {
    crate::Error::CorruptStream
}

/// The operations the context model decodes symbols through.
///
/// These are the three operations 7-Zip's `Ppmd7` model calls on its range
/// decoder (`GetThreshold`, `Decode`, `DecodeBit`). A coder implements each
/// with its own arithmetic; the model's output is bit-exact with the
/// reference only if the coder's arithmetic is too:
///
/// - **RAR** (Subbotin's carry-less coder): `get_threshold(t)` sets
///   `range /= t` and returns `(code - low) / range`; `decode(s, n)` sets
///   `low += s * range`, `range *= n`, then normalizes; `decode_bit(s0)`
///   sets `range >>= 14`, compares `(code - low) / range` with `s0`, and
///   decodes `(0, s0)` or `(s0, 2^14 - s0)` the same way. Normalization
///   shifts in a byte while `(low ^ (low + range)) < 2^24`, or while
///   `range < 2^15` after setting `range = -low & (2^15 - 1)`. All
///   arithmetic is wrapping `u32`.
/// - **7z** (Pavlov's coder): `get_threshold(t)` sets `range /= t` and
///   returns `code / range`; `decode(s, n)` sets `code -= s * range`,
///   `range *= n`, then normalizes; `decode_bit(s0)` computes
///   `bound = (range >> 14) * s0` and keeps `[0, bound)` or
///   `[bound, range)`.
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

    /// Decodes a binary decision over a total of `1 << 14`, where the first
    /// outcome has frequency `size0`. Returns 0 for the first outcome and 1
    /// for the second, and normalizes before returning.
    fn decode_bit(&mut self, size0: u32) -> u32;

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

/// The operations the context model encodes symbols through: the mirror of
/// [`RangeDecoder`].
///
/// 7-Zip's encoder divides the range by the total in the model
/// (`R->Range /= SummFreq`) and then calls `RC_Encode(start, size)`; here the
/// division is part of [`encode`](Self::encode). Every call normalizes before
/// it returns, so the model never normalizes explicitly.
pub trait RangeEncoder {
    /// Encodes the symbol occupying `[start, start + size)` out of `total`,
    /// then normalizes.
    fn encode(&mut self, start: u32, size: u32, total: u32);

    /// Encodes a binary decision over a total of `1 << 14` whose first
    /// outcome has frequency `size0`: `bit` 0 for the first outcome, any
    /// other value for the second. Normalizes before returning.
    fn encode_bit(&mut self, size0: u32, bit: u32);

    /// Whether the coder met a range scaled to zero (a total past the
    /// range, or a symbol size of zero). The stream it is writing is then
    /// undecodable; finishing the encoder reports the fault as an error.
    fn faulted(&self) -> bool;
}
