//! The symbol loop.
//!
//! Every decode framing runs its symbols through the two loops here:
//!
//! - [`fast`] decodes while the cursor is at or before `fast_end`, the
//!   point after which fewer than the coder's per-symbol input bound remain
//!   (`Params::max_input_per_symbol` for 7z, `4 * (order + 2)` for the
//!   carry-less coder). A symbol started there can never read past the
//!   input, so the loop has no end-of-input or padding test at all: the
//!   cursor's bounds comparison never fails, the output is filled through
//!   `iter_mut`, and the only per-symbol branches are the model's own.
//! - [`edge`] decodes the last input's final symbols one at a time with the
//!   framing's padding rule, after `fast` has stopped at its margin.
//!
//! The margin is enforced, not assumed: debug builds assert that `fast`
//! never fed a zero past the input, and the `chunking_invariance` fuzz
//! target checks it per call.
//!
//! **Hook for the model's batched entry point.** Each iteration is one
//! `Model::decode_symbol`. A model-side batch (registers in locals across
//! symbols, one fault check per batch, vector layouts) replaces the body of
//! [`fast`] and nothing else: the framings only see `(produced, Stop)`.

use crate::error::{Error, Result};
use crate::model::Model;
use crate::rc::{CarrylessRangeDecoder, RangeDecoder, RangeInput, SevenZipRangeDecoder};

/// Why a loop stopped without an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The output slice is full.
    OutputFull,
    /// The cursor passed `fast_end`: the rest needs more input, or the edge
    /// loop.
    Margin,
    /// The model's end marker. The framing checks the coder.
    EndMarker,
    /// The RAR escape byte, consumed and not written.
    Escape,
}

/// A decoder whose cursor position and padding count the loops can read.
pub(crate) trait Cursor: RangeDecoder {
    fn position(&self) -> usize;
    fn padding(&self) -> u32;
}

impl<I: RangeInput> Cursor for SevenZipRangeDecoder<I> {
    #[inline(always)]
    fn position(&self) -> usize {
        SevenZipRangeDecoder::position(self)
    }
    #[inline(always)]
    fn padding(&self) -> u32 {
        self.zero_bytes_past_eof()
    }
}

impl<I: RangeInput> Cursor for CarrylessRangeDecoder<I> {
    #[inline(always)]
    fn position(&self) -> usize {
        CarrylessRangeDecoder::position(self)
    }
    #[inline(always)]
    fn padding(&self) -> u32 {
        self.zero_bytes_past_eof()
    }
}

/// Decodes into `out` while the cursor is at or before `fast_end`. With
/// `ESC`, a symbol equal to `esc` stops the loop (consumed, not written).
/// Returns the bytes written and why it stopped; on an error the bytes
/// before it are still valid.
#[inline]
pub(crate) fn fast<R: Cursor, const ESC: bool>(
    model: &mut Model,
    rc: &mut R,
    out: &mut [u8],
    fast_end: usize,
    esc: u8,
) -> (usize, Result<Stop>) {
    let mut produced = 0;
    for slot in out.iter_mut() {
        if rc.position() > fast_end {
            return (produced, Ok(Stop::Margin));
        }
        match model.decode_symbol(rc) {
            Ok(Some(byte)) => {
                debug_assert_eq!(rc.padding(), 0, "a symbol outran the input margin");
                if ESC && byte == esc {
                    return (produced, Ok(Stop::Escape));
                }
                *slot = byte;
                produced += 1;
            }
            Ok(None) => return (produced, Ok(Stop::EndMarker)),
            Err(e) => return (produced, Err(e)),
        }
    }
    (produced, Ok(Stop::OutputFull))
}

/// The padding rule of the edge loop.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Padding {
    /// Zeros fed in earlier calls since the stream (or block) started.
    pub(crate) before: u32,
    /// Zeros a byte may have needed and still be kept: 0 for 7z and the raw
    /// carry-less stream, the caller's allowance for RAR.
    pub(crate) allowance: u32,
    /// Whether a model error met on padding is reported as truncation (7z:
    /// the input ran out under the coder) rather than as the model's error.
    pub(crate) truncates_errors: bool,
}

/// Decodes the last input's remaining symbols one at a time, checking the
/// padding after each. A byte that needed more padding than allowed is
/// truncation and is not written. Stops at the end marker without checking
/// it (the framing does).
pub(crate) fn edge<R: Cursor, const ESC: bool>(
    model: &mut Model,
    rc: &mut R,
    out: &mut [u8],
    esc: u8,
    rule: Padding,
) -> (usize, Result<Stop>) {
    let mut produced = 0;
    for slot in out.iter_mut() {
        let result = model.decode_symbol(rc);
        let padded = rule.before.saturating_add(rc.padding());
        match result {
            Ok(Some(byte)) => {
                if padded > rule.allowance {
                    return (produced, Err(Error::truncated()));
                }
                if ESC && byte == esc {
                    return (produced, Ok(Stop::Escape));
                }
                *slot = byte;
                produced += 1;
            }
            Ok(None) => return (produced, Ok(Stop::EndMarker)),
            Err(e) => {
                let e = if rule.truncates_errors && padded > rule.allowance {
                    Error::truncated()
                } else {
                    e
                };
                return (produced, Err(e));
            }
        }
    }
    (produced, Ok(Stop::OutputFull))
}
