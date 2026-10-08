//! Range coders.
//!
//! Two coders carry variant H in practice, and the model is generic over them:
//!
//! - RAR's carry-less range coder (Subbotin's design, as used by Shkarin's
//!   reference and by unrar for RAR 2.9/3.x PPMd blocks), and
//! - 7-Zip's LZMA-style range coder with carry propagation (Igor Pavlov's
//!   `Ppmd7z_RangeDec` / `Ppmd7z_RangeEnc`).
//!
//! This module will hold both decoders and both encoders. The trait below is
//! a placeholder for the decoder seam; the extraction work finalizes it.

/// The interface the context model decodes symbols through.
///
/// Frequencies are cumulative counts out of `total`; the model computes them
/// and the coder narrows its range.
pub trait RangeDecoder {
    /// Scales the current range by `total` and returns the cumulative count
    /// the next symbol falls under.
    fn get_threshold(&mut self, total: u32) -> u32;

    /// Consumes the symbol occupying `[start, start + size)` of the range
    /// last scaled by [`get_threshold`](Self::get_threshold).
    fn decode(&mut self, start: u32, size: u32);

    /// Decodes a binary decision whose first outcome has frequency `size0`
    /// out of `total`, returning `true` for the second outcome.
    fn decode_bit(&mut self, size0: u32, total: u32) -> bool;
}
