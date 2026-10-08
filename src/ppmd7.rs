//! The 7z framing: the `PPMD` method (`03 04 01`) as `.7z` carries it.
//!
//! A 7z PPMd stream is variant H over Igor Pavlov's range coder
//! ([`SevenZipRangeDecoder`]): one leading zero byte, four bytes of coder
//! initialization, then the coded symbols. The coder properties are the
//! model order and the arena size; 7z stores them as five bytes (order,
//! then the size as little-endian `u32`). 7-Zip writes no end marker: the
//! folder's unpacked size says where the data ends. A stream that does
//! carry a marker (an escape out of the order-0 context) ends there.
//!
//! [`Ppmd7Decoder`] reads a stream through [`std::io::Read`];
//! [`decode_7z`] decodes a whole stream held in memory. Both follow 7-Zip's
//! `PpmdDecoder.cpp`:
//!
//! - **Reading past the input is an error.** 7-Zip raises its `Extra` flag
//!   when the coder asks for a byte the input does not have, and fails the
//!   stream; the symbol decoded with that byte is not output. A well-formed
//!   stream never needs one: the decoder reads exactly the bytes the encoder
//!   wrote. The error is [`Error::Truncated`].
//! - **The end marker** ends the data when the coder's `code` register is
//!   0 there, and is [`Error::CorruptStream`] otherwise.
//! - **A known unpacked size** stops decoding after that many bytes. Like
//!   7-Zip's decoder without `FinishStream`, nothing is checked there by
//!   default; [`Ppmd7Decoder::set_finish_stream`] adds 7-Zip's
//!   `FinishStream` check (the coder's `code` is 0 at the size).
//!
//! Decoded output is identical to 7-Zip's.
//!
//! [`Ppmd7Encoder`] writes a stream through [`std::io::Write`] and
//! [`encode_7z`] encodes a slice into a `Vec`, as 7-Zip's `PpmdEncoder.cpp`
//! drives `Ppmd7z_EncodeSymbols` and `Ppmd7z_Flush_RangeEnc`
//! (`C/Ppmd7Enc.c`). For the same order and memory size the stream is
//! byte-identical to 7-Zip's; 7-Zip writes no end marker in a `.7z`, so pass
//! `false` to [`Ppmd7Encoder::finish`] to match it.

use std::io::{self, Read, Write};

use crate::error::{Error, Result};
use crate::model::Model;
use crate::rc::{
    IntoRangeInput, RangeInput, RangeOutput, ReadInput, SevenZipRangeDecoder, SevenZipRangeEncoder,
    SliceInput, WriteOutput,
};

pub use crate::{
    PPMD7_MAX_MEM_SIZE, PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE, PPMD7_MIN_ORDER, SYM_END,
};

/// How decoding stopped, once it has.
enum State {
    /// More symbols may follow.
    Decoding,
    /// The known size or the end marker was reached; reads return 0.
    Finished,
    /// An error ended the stream; reads repeat it.
    Failed(Error),
}

/// A copy of `e` for repeating it on a later read.
fn echo(e: &Error) -> Error {
    match e {
        Error::CorruptStream { detail } => Error::CorruptStream { detail },
        Error::InvalidParameters => Error::InvalidParameters,
        Error::Io(inner) => Error::Io(io::Error::new(inner.kind(), inner.to_string())),
        Error::Truncated => Error::Truncated,
    }
}

/// The model, the coder and the framing state, over any coder input.
struct Core<I: RangeInput> {
    model: Model,
    rc: SevenZipRangeDecoder<I>,
    /// Bytes still to produce, when the unpacked size is known.
    remaining: Option<u64>,
    /// 7-Zip's `FinishStream`: at the known size `code` must be 0, and an
    /// end marker before it is an error.
    finish_stream: bool,
    state: State,
    /// An error met after some bytes of the current read were produced; it
    /// is returned by the next read, so those bytes are not lost.
    pending: Option<Error>,
}

impl<I: RangeInput + IntoRangeInput<Input = I>> Core<I> {
    /// Checks the parameters, initializes the coder, then builds the model,
    /// so a stream that fails its first five bytes allocates no arena.
    fn new(input: I, order: u32, mem_size: u32, remaining: Option<u64>) -> Result<Self> {
        Model::check_parameters(order, mem_size)?;
        let rc = SevenZipRangeDecoder::new(input)?;
        let model = Model::new(order, mem_size)?;
        Ok(Self {
            model,
            rc,
            remaining,
            finish_stream: false,
            state: State::Decoding,
            pending: None,
        })
    }

    /// Records `e` as the stream's end. With `produced` bytes already in
    /// the caller's buffer the error waits for the next call.
    #[cold]
    fn fail(&mut self, produced: usize, e: Error) -> Result<usize> {
        self.state = State::Failed(echo(&e));
        if produced > 0 {
            self.pending = Some(e);
            Ok(produced)
        } else {
            Err(e)
        }
    }

    /// The input ran out under the coder: the reader's error if it failed,
    /// otherwise a truncated stream.
    #[cold]
    fn extra_error(&mut self) -> Error {
        match self.rc.input_mut().take_io_error() {
            Some(e) => Error::Io(e),
            None => Error::Truncated,
        }
    }

    /// The known size was reached.
    fn finish_at_size(&mut self, produced: usize) -> Result<usize> {
        if self.finish_stream && !self.rc.is_finished_ok() {
            return self.fail(
                produced,
                Error::CorruptStream {
                    detail: "7z PPMd stream does not end at its unpacked size",
                },
            );
        }
        self.state = State::Finished;
        Ok(produced)
    }

    /// Decodes up to `out.len()` bytes (fewer at the known size or the end
    /// marker). Returns 0 only when the data has ended or `out` is empty.
    #[inline]
    fn fill(&mut self, out: &mut [u8]) -> Result<usize> {
        match &self.state {
            State::Decoding => {}
            State::Finished => return Ok(0),
            State::Failed(e) => {
                return Err(self.pending.take().unwrap_or_else(|| echo(e)));
            }
        }
        let want = match self.remaining {
            Some(0) => return self.finish_at_size(0),
            Some(left) => out.len().min(usize::try_from(left).unwrap_or(usize::MAX)),
            None => out.len(),
        };
        let out = &mut out[..want];

        let mut produced = 0;
        let ended = loop {
            let Some(slot) = out.get_mut(produced) else {
                break false;
            };
            match self.model.decode_symbol(&mut self.rc) {
                Ok(Some(byte)) => {
                    // `Extra` is checked before the symbol is kept, as
                    // `PpmdDecoder.cpp` does.
                    if self.rc.zero_bytes_past_eof() != 0 {
                        let e = self.extra_error();
                        return self.fail(produced, e);
                    }
                    *slot = byte;
                    produced += 1;
                }
                Ok(None) => break true,
                Err(e) => {
                    let e = if self.rc.zero_bytes_past_eof() != 0 {
                        self.extra_error()
                    } else {
                        e
                    };
                    return self.fail(produced, e);
                }
            }
        };

        if ended {
            if self.rc.zero_bytes_past_eof() != 0 {
                let e = self.extra_error();
                return self.fail(produced, e);
            }
            if !self.rc.is_finished_ok() {
                return self.fail(
                    produced,
                    Error::CorruptStream {
                        detail: "7z PPMd end marker with a nonzero range coder code",
                    },
                );
            }
            if self.finish_stream && self.remaining.is_some_and(|r| r != produced as u64) {
                return self.fail(
                    produced,
                    Error::CorruptStream {
                        detail: "7z PPMd end marker before the unpacked size",
                    },
                );
            }
            self.state = State::Finished;
            return Ok(produced);
        }

        if let Some(left) = self.remaining.as_mut() {
            *left -= produced as u64;
            if *left == 0 {
                return self.finish_at_size(produced);
            }
        }
        Ok(produced)
    }
}

/// A 7z `PPMD` stream decoder over a [`Read`]er, read through [`Read`].
///
/// The input is read through a 64 KiB buffer, so an unbuffered file costs
/// one `read` call per 64 KiB, never one per byte.
///
/// Errors come back as [`io::Error`]s wrapping an [`Error`] (see
/// `From<Error> for io::Error`); a reader's own error is passed through.
/// Once a read fails, every later read fails the same way.
pub struct Ppmd7Decoder<R: Read> {
    core: Core<ReadInput<R>>,
}

impl<R: Read> Ppmd7Decoder<R> {
    /// A decoder for a stream of unknown unpacked size: reading ends at the
    /// end marker (or fails if the input runs out first).
    ///
    /// Reads the coder's five initialization bytes. Errors:
    /// [`Error::InvalidParameters`] for an `order` outside
    /// [`PPMD7_MIN_ORDER`]`..=`[`PPMD7_MAX_ORDER`] or a `mem_size` outside
    /// [`PPMD7_MIN_MEM_SIZE`]`..=`[`PPMD7_MAX_MEM_SIZE`];
    /// [`Error::Truncated`] when the input has fewer than five bytes;
    /// [`Error::CorruptStream`] when the first byte is not 0 or the initial
    /// code is `0xFFFFFFFF`; [`Error::Io`] when the reader fails.
    pub fn new(reader: R, order: u32, mem_size: u32) -> Result<Self> {
        Ok(Self {
            core: Core::new(ReadInput::new(reader), order, mem_size, None)?,
        })
    }

    /// A decoder for a stream of known unpacked size (the 7z folder's):
    /// reading ends after `unpacked_size` bytes, or earlier at an end
    /// marker. Errors as [`new`](Self::new).
    pub fn with_unpacked_size(
        reader: R,
        order: u32,
        mem_size: u32,
        unpacked_size: u64,
    ) -> Result<Self> {
        Ok(Self {
            core: Core::new(ReadInput::new(reader), order, mem_size, Some(unpacked_size))?,
        })
    }

    /// 7-Zip's `FinishStream` mode, off by default. When on, with a known
    /// unpacked size, the coder's `code` register must be 0 once the size
    /// is reached, and an end marker before the size is an error; both are
    /// [`Error::CorruptStream`]. Checking that the whole packed stream was
    /// consumed is the container's job: compare [`position`](Self::position)
    /// with the packed size.
    pub fn set_finish_stream(&mut self, finish_stream: bool) {
        self.core.finish_stream = finish_stream;
    }

    /// Bytes of input the coder has consumed.
    pub fn position(&self) -> usize {
        self.core.rc.position()
    }

    /// Bytes still to decode, when the unpacked size is known.
    pub fn remaining(&self) -> Option<u64> {
        self.core.remaining
    }

    /// Whether the data has ended: the known size or the end marker was
    /// reached.
    pub fn is_finished(&self) -> bool {
        matches!(self.core.state, State::Finished)
    }

    /// The reader. Bytes the decoder has buffered are already read from it.
    pub fn get_ref(&self) -> &R {
        self.core.rc.input().get_ref()
    }

    /// Unwraps the reader. Bytes the decoder has buffered but not consumed
    /// are lost.
    pub fn into_inner(self) -> R {
        self.core.rc.into_input().into_inner()
    }
}

impl<R: Read> Read for Ppmd7Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.core.fill(buf).map_err(io::Error::from)
    }
}

/// Decodes a whole 7z `PPMD` stream held in memory.
///
/// With `unpacked_len`, decodes exactly that many bytes (the 7z folder's
/// unpacked size); an end marker or the end of the input before then is an
/// error. Without it, decodes to the end marker. Parameter and stream
/// errors are as [`Ppmd7Decoder::new`]'s.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
) -> Result<Vec<u8>> {
    let mut core = Core::new(SliceInput::new(stream), order, mem_size, unpacked_len)?;
    let mut out = Vec::new();
    // Grow geometrically, so a size the stream does not back costs memory
    // only as fast as the stream produces output.
    let mut chunk = 1 << 16;
    loop {
        let start = out.len();
        let want = match unpacked_len {
            Some(len) => match usize::try_from(len - start as u64) {
                Ok(0) => break,
                Ok(left) => left.min(chunk),
                Err(_) => chunk,
            },
            None => chunk,
        };
        out.resize(start + want, 0);
        let n = core.fill(&mut out[start..])?;
        out.truncate(start + n);
        if n == 0 {
            break;
        }
        chunk = chunk.saturating_mul(2).min(1 << 26);
    }
    if let Some(e) = core.pending.take() {
        return Err(e);
    }
    if unpacked_len.is_some_and(|len| len != out.len() as u64) {
        return Err(Error::CorruptStream {
            detail: "7z PPMd end marker before the unpacked size",
        });
    }
    Ok(out)
}

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
        self.encode(buf).map_err(io::Error::from)?;
        Ok(buf.len())
    }

    /// Hands the bytes the coder has settled to the writer and flushes it.
    /// The coder's pending bytes stay until [`finish`](Ppmd7Encoder::finish).
    fn flush(&mut self) -> io::Result<()> {
        self.rc.output_mut().finish().map_err(io::Error::from)
    }
}

/// Encodes `data` into a new `Vec`: the stream [`Ppmd7Encoder`] would
/// write, without the buffered writer in between. The counterpart of
/// [`decode_7z`].
///
/// Errors: as [`Ppmd7Encoder::new`] and [`Ppmd7Encoder::finish`].
pub fn encode_7z(data: &[u8], order: u32, mem_size: u32, end_marker: bool) -> Result<Vec<u8>> {
    let mut model = Model::new(order, mem_size)?;
    let mut rc = SevenZipRangeEncoder::new(Vec::with_capacity(data.len() / 2 + 16));
    model.encode_bytes(&mut rc, data)?;
    if end_marker {
        model.encode_symbol(&mut rc, None)?;
    }
    rc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty stream 7-Zip's encoder writes: the leading zero and the
    /// four flush bytes.
    const EMPTY: [u8; 5] = [0; 5];

    #[test]
    fn an_empty_stream_decodes_to_nothing_at_size_zero() {
        assert_eq!(decode_7z(&EMPTY, 6, 1 << 16, Some(0)).unwrap(), b"");
        let mut dec = Ppmd7Decoder::with_unpacked_size(&EMPTY[..], 6, 1 << 16, 0).unwrap();
        dec.set_finish_stream(true);
        let mut out = Vec::new();
        assert_eq!(dec.read_to_end(&mut out).unwrap(), 0);
        assert!(dec.is_finished());
        assert_eq!(dec.position(), 5);
    }

    #[test]
    fn parameters_are_checked_before_the_input_is_read() {
        for (order, mem) in [(1, 1 << 16), (65, 1 << 16), (6, 2047), (6, u32::MAX)] {
            assert!(matches!(
                Ppmd7Decoder::new(&[][..], order, mem),
                Err(Error::InvalidParameters)
            ));
        }
        assert!(matches!(
            Ppmd7Decoder::new(&[0u8; 4][..], 6, 1 << 16),
            Err(Error::Truncated)
        ));
        assert!(matches!(
            Ppmd7Decoder::new(&[1u8, 0, 0, 0, 0][..], 6, 1 << 16),
            Err(Error::CorruptStream { .. })
        ));
    }

    #[test]
    fn running_out_of_input_is_truncated_and_sticky() {
        // Five zero bytes decode symbol 0 for a while, then need more input.
        let mut dec = Ppmd7Decoder::new(&EMPTY[..], 6, 1 << 16).unwrap();
        let mut buf = [0u8; 1 << 16];
        let mut total = 0;
        let err = loop {
            match dec.read(&mut buf) {
                Ok(0) => panic!("ended without a marker"),
                Ok(n) => total += n,
                Err(e) => break e,
            }
        };
        assert!(total < buf.len());
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let again = dec.read(&mut buf).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::UnexpectedEof);
        assert!(matches!(
            decode_7z(&EMPTY, 6, 1 << 16, None),
            Err(Error::Truncated)
        ));
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope"))
        }
    }

    #[test]
    fn a_reader_error_is_passed_through() {
        match Ppmd7Decoder::new(Failing, 6, 1 << 16) {
            Err(Error::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::PermissionDenied),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("decoded from a failing reader"),
        }
    }
}
