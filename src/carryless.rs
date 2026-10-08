//! Raw carry-less PPMd streams.
//!
//! Variant H over Dmitry Subbotin's carry-less range coder, as Dmitry
//! Shkarin's public-domain PPMd var.H encoder writes it (the coder 7-Zip
//! calls `Ppmd7a` and RAR 2.9 through 4.x uses inside its PPMd blocks): four
//! bytes of coder initialization, then the coded symbols. The encode path
//! follows 7-Zip's `Ppmd7Enc.c` and Shkarin's encoder over the crate's
//! shared model; RARLAB's unrar has no encoder, and no RARLAB source text is
//! used here.
//!
//! This is the range-coded symbol stream only: no RAR block header, no
//! escape layer, no archive. Writing RAR blocks or archives is out of scope
//! by design. The pair exists so the carry-less paths, including
//! [`RarPpmd`](crate::RarPpmd), can be round-trip tested; it is for
//! correctness only, never benchmarked and not tuned.
//!
//! [`CarrylessDecoder`] and [`CarrylessEncoder`] have exactly the surface of
//! the 7z pair and its stop rules ([`crate::sevenz`]); the end marker needs
//! the coder's `code` to equal its `low`, and an initial code of
//! `0xFFFFFFFF` is corrupt (7-Zip's `Ppmd7a_RangeDec_Init`).

use crate::arena::Arena;
use crate::error::{Progress, Result};
use crate::params::Params;
use crate::stream::{Carryless, Finish, SevenZStatus, StreamDecoder, StreamEncoder};

/// A raw carry-less stream decoder; see [`SevenZDecoder`](crate::SevenZDecoder)
/// for the contract. A non-final call needs `4 * (order + 2)` bytes of input
/// to decode a symbol.
pub struct CarrylessDecoder {
    inner: StreamDecoder<Carryless>,
}

impl CarrylessDecoder {
    /// A decoder for one stream; `unpacked` as for
    /// [`SevenZDecoder::new`](crate::SevenZDecoder::new).
    pub fn new(params: Params, unpacked: Option<u64>) -> Result<Self> {
        Self::with_arena(params, unpacked, Arena::empty())
    }

    /// [`new`](Self::new) in `arena` when its capacity fits.
    pub fn with_arena(params: Params, unpacked: Option<u64>, arena: Arena) -> Result<Self> {
        Ok(Self {
            inner: StreamDecoder::with_arena(params, unpacked, arena)?,
        })
    }

    /// Starts the next stream, keeping the arena when it fits.
    pub fn reset(&mut self, params: Params, unpacked: Option<u64>) -> Result<()> {
        self.inner.reset(params, unpacked)
    }

    /// FinishStream, as [`SevenZDecoder::set_finish_stream`](crate::SevenZDecoder::set_finish_stream).
    pub fn set_finish_stream(&mut self, on: bool) {
        self.inner.set_finish_stream(on);
    }

    /// Decodes from `input` into `out`, as
    /// [`SevenZDecoder::decode`](crate::SevenZDecoder::decode).
    pub fn decode(
        &mut self,
        input: &[u8],
        input_is_last: bool,
        out: &mut [u8],
    ) -> Result<Progress<SevenZStatus>> {
        self.inner.decode(input, input_is_last, out)
    }

    /// Input bytes consumed since construction or the last reset.
    pub fn total_in(&self) -> u64 {
        self.inner.total_in()
    }

    /// Output bytes produced since construction or the last reset.
    pub fn total_out(&self) -> u64 {
        self.inner.total_out()
    }

    /// Heap bytes the decoder holds.
    pub fn memory_footprint(&self) -> u64 {
        self.inner.memory_footprint()
    }

    /// The arena, for the next codec.
    pub fn into_arena(self) -> Arena {
        self.inner.into_arena()
    }
}

/// A raw carry-less stream encoder; see [`SevenZEncoder`](crate::SevenZEncoder)
/// for the contract. The carry-less coder holds nothing back, so
/// [`finish`](Self::finish) writes the end marker if asked and then the four
/// bytes of the coder's `low`.
pub struct CarrylessEncoder {
    inner: StreamEncoder<Carryless>,
}

impl CarrylessEncoder {
    /// An encoder for one stream.
    pub fn new(params: Params) -> Result<Self> {
        Self::with_arena(params, Arena::empty())
    }

    /// [`new`](Self::new) in `arena` when its capacity fits.
    pub fn with_arena(params: Params, arena: Arena) -> Result<Self> {
        Ok(Self {
            inner: StreamEncoder::with_arena(params, arena)?,
        })
    }

    /// Starts the next stream, keeping the arena when it fits.
    pub fn reset(&mut self, params: Params) -> Result<()> {
        self.inner.reset(params)
    }

    /// Encodes from `input` into `out`, as
    /// [`SevenZEncoder::encode`](crate::SevenZEncoder::encode).
    pub fn encode(&mut self, input: &[u8], out: &mut [u8]) -> Result<Progress<()>> {
        self.inner.encode(input, out)
    }

    /// Ends the stream, as [`SevenZEncoder::finish`](crate::SevenZEncoder::finish).
    pub fn finish(&mut self, out: &mut [u8], end_marker: bool) -> Result<Finish> {
        self.inner.finish(out, end_marker)
    }

    /// Output bytes owed before any further symbol.
    pub fn pending_output(&self) -> u64 {
        self.inner.pending_output()
    }

    /// Input bytes encoded since construction or the last reset.
    pub fn total_in(&self) -> u64 {
        self.inner.total_in()
    }

    /// Output bytes produced since construction or the last reset.
    pub fn total_out(&self) -> u64 {
        self.inner.total_out()
    }

    /// Heap bytes the encoder holds.
    pub fn memory_footprint(&self) -> u64 {
        self.inner.memory_footprint()
    }

    /// The arena, for the next codec.
    pub fn into_arena(self) -> Arena {
        self.inner.into_arena()
    }
}
