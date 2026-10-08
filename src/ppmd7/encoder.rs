//! The 7z `PPMD` stream encoder.
//!
//! Variant H over Igor Pavlov's 7z range coder, as 7-Zip's `PpmdEncoder.cpp`
//! drives `Ppmd7z_EncodeSymbols` and `Ppmd7z_Flush_RangeEnc`
//! (`C/Ppmd7Enc.c`, public domain). For the same order and memory size the
//! stream is byte-identical to 7-Zip's: 7-Zip writes no end marker in a
//! `.7z`, so pass `false` to [`Ppmd7Encoder::finish`] to match it.

use std::io::{self, Write};

use crate::error::{Error, Result};
use crate::model::Model;
use crate::rc::{RangeOutput, SevenZipRangeEncoder, WriteOutput};

/// Encodes a 7z `PPMD` stream into any [`std::io::Write`].
///
/// Bytes written to it are coded one symbol each; coded bytes reach the
/// writer in 64 KiB batches, and [`finish`](Self::finish) writes the rest
/// (the end marker if asked, then the coder's five flush bytes). A write
/// error from the writer is reported by the next [`flush`](Write::flush) or
/// by [`finish`](Self::finish). Nothing reaches the writer as a container:
/// the properties (order, then the memory size little-endian) are the
/// caller's to store.
pub struct Ppmd7Encoder<W: Write> {
    model: Model,
    rc: SevenZipRangeEncoder<WriteOutput<W>>,
}

impl<W: Write> Ppmd7Encoder<W> {
    /// A new encoder writing to `writer`, with model order `order` and an
    /// arena of `mem_size` bytes.
    ///
    /// Errors: [`Error::InvalidParameters`] for an order outside
    /// [`PPMD7_MIN_ORDER`](crate::PPMD7_MIN_ORDER)`..=`[`PPMD7_MAX_ORDER`](crate::PPMD7_MAX_ORDER)
    /// or a size outside
    /// [`PPMD7_MIN_MEM_SIZE`](crate::PPMD7_MIN_MEM_SIZE)`..=`[`PPMD7_MAX_MEM_SIZE`](crate::PPMD7_MAX_MEM_SIZE).
    pub fn new(writer: W, order: u32, mem_size: u32) -> Result<Self> {
        Ok(Self {
            model: Model::new(order, mem_size)?,
            rc: SevenZipRangeEncoder::new(WriteOutput::new(writer)),
        })
    }

    /// The writer. Up to 64 KiB of coded bytes may still be buffered.
    pub fn get_ref(&self) -> &W {
        self.rc.output().get_ref()
    }

    /// Encodes every byte of `data`.
    ///
    /// Errors: [`Error::CorruptStream`] if the model or the coder went
    /// inconsistent, which a correct encoder never does; the stream is then
    /// unusable.
    pub fn encode(&mut self, data: &[u8]) -> Result<()> {
        self.model.encode_bytes(&mut self.rc, data)
    }

    /// Writes the end marker when `with_end_marker` is set, flushes the
    /// coder (`Ppmd7z_Flush_RangeEnc`) and returns the writer.
    ///
    /// Errors: the writer's error, or [`Error::CorruptStream`] as for
    /// [`encode`](Self::encode).
    pub fn finish(mut self, with_end_marker: bool) -> Result<W> {
        if with_end_marker {
            self.model.encode_symbol(&mut self.rc, None)?;
        }
        Ok(self.rc.finish()?.into_inner())
    }
}

impl<W: Write> Write for Ppmd7Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encode(buf).map_err(into_io)?;
        Ok(buf.len())
    }

    /// Hands the bytes the coder has settled to the writer and flushes it.
    /// The coder's pending bytes stay until [`finish`](Ppmd7Encoder::finish).
    fn flush(&mut self) -> io::Result<()> {
        self.rc.output_mut().finish().map_err(into_io)
    }
}

/// Encodes `data` into a new `Vec`: the stream [`Ppmd7Encoder`] would
/// write, without the buffered writer in between.
///
/// Errors: as [`Ppmd7Encoder::new`] and [`Ppmd7Encoder::finish`].
pub fn encode_to_vec(data: &[u8], order: u32, mem_size: u32, end_marker: bool) -> Result<Vec<u8>> {
    let mut model = Model::new(order, mem_size)?;
    let mut rc = SevenZipRangeEncoder::new(Vec::with_capacity(data.len() / 2 + 16));
    model.encode_bytes(&mut rc, data)?;
    if end_marker {
        model.encode_symbol(&mut rc, None)?;
    }
    rc.finish()
}

/// A crate error as an `io::Error`, keeping an I/O error as it was.
pub(crate) fn into_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        other => io::Error::other(other),
    }
}
