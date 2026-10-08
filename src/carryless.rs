//! Raw carry-less PPMd streams.
//!
//! Variant H over Dmitry Subbotin's carry-less range coder, as Dmitry
//! Shkarin's public-domain PPMd var.H encoder writes it (the coder 7-Zip
//! calls `Ppmd7a` and RAR 2.9 through 4.x uses inside its PPMd blocks).
//! The encode path follows 7-Zip's `Ppmd7Enc.c` and Shkarin's encoder over
//! the crate's shared model; RARLAB's unrar has no encoder, and no RARLAB
//! source text is used here.
//!
//! This is the range-coded symbol stream only: no RAR block header, no
//! escape layer, no archive. Writing RAR blocks or archives is out of scope
//! by design. It exists so the carry-less decoding paths
//! ([`CarrylessRangeDecoder`](crate::rc::CarrylessRangeDecoder),
//! [`RarDecoder`](crate::rar::RarDecoder)) can be round-trip tested; it is
//! for correctness only, never benchmarked and not a tuned encoder.

use std::io::{self, Write};

use crate::error::Result;
use crate::model::Model;
use crate::rc::{CarrylessRangeEncoder, RangeOutput, WriteOutput};

/// Encodes a raw carry-less PPMd stream into any [`std::io::Write`]: the
/// counterpart of [`Ppmd7Encoder`](crate::ppmd7::Ppmd7Encoder) with the
/// carry-less coder.
///
/// [`finish`](Self::finish) writes the end marker if asked and then the four
/// bytes of the coder's `low`. The stream starts with the coder's four
/// initialization bytes, as a RAR PPMd block's coder data does.
pub struct CarrylessEncoder<W: Write> {
    model: Model,
    rc: CarrylessRangeEncoder<WriteOutput<W>>,
}

impl<W: Write> CarrylessEncoder<W> {
    /// A new encoder writing to `writer`, with model order `order` and an
    /// arena of `mem_size` bytes (a RAR block declares whole MiB).
    ///
    /// Errors: [`Error::InvalidParameters`](crate::Error::InvalidParameters)
    /// for parameters variant H does not accept.
    pub fn new(writer: W, order: u32, mem_size: u32) -> Result<Self> {
        Ok(Self {
            model: Model::new(order, mem_size)?,
            rc: CarrylessRangeEncoder::new(WriteOutput::new(writer)),
        })
    }

    /// The writer. Up to 64 KiB of coded bytes may still be buffered.
    pub fn get_ref(&self) -> &W {
        self.rc.output().get_ref()
    }

    /// Encodes every byte of `data`.
    ///
    /// Errors: [`Error::CorruptStream`](crate::Error::CorruptStream) if the
    /// model or the coder went inconsistent, which a correct encoder never
    /// does.
    pub fn encode(&mut self, data: &[u8]) -> Result<()> {
        self.model.encode_bytes(&mut self.rc, data)
    }

    /// Writes the end marker when `with_end_marker` is set, flushes the
    /// coder and returns the writer.
    ///
    /// Errors: the writer's error, or as for [`encode`](Self::encode).
    pub fn finish(mut self, with_end_marker: bool) -> Result<W> {
        if with_end_marker {
            self.model.encode_symbol(&mut self.rc, None)?;
        }
        Ok(self.rc.finish()?.into_inner())
    }
}

impl<W: Write> Write for CarrylessEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encode(buf).map_err(io::Error::from)?;
        Ok(buf.len())
    }

    /// Hands the bytes already written by the coder to the writer and
    /// flushes it. The coder's `low` stays until
    /// [`finish`](CarrylessEncoder::finish).
    fn flush(&mut self) -> io::Result<()> {
        self.rc.output_mut().finish().map_err(io::Error::from)
    }
}

/// Encodes `data` into a new `Vec` as a raw carry-less stream.
///
/// Errors: as [`CarrylessEncoder::new`] and [`CarrylessEncoder::finish`].
pub fn encode_carryless(
    data: &[u8],
    order: u32,
    mem_size: u32,
    end_marker: bool,
) -> Result<Vec<u8>> {
    let mut model = Model::new(order, mem_size)?;
    let mut rc = CarrylessRangeEncoder::new(Vec::with_capacity(data.len() / 2 + 16));
    model.encode_bytes(&mut rc, data)?;
    if end_marker {
        model.encode_symbol(&mut rc, None)?;
    }
    rc.finish()
}
