//! `Read` and `Write` over the 7z step codecs, for code built on `std::io`
//! (a 7z coder chain, command-line tools).
//!
//! [`SevenZReader`] takes a [`BufRead`], so the only copy it makes is a
//! short stitch buffer when the reader's buffer ends within
//! [`Params::max_input_per_symbol`] bytes of a symbol; the step decoder never
//! holds bytes itself. [`SevenZWriter`] writes only settled bytes:
//! [`flush`](Write::flush) passes through, and the stream's tail is written
//! exactly once, by [`SevenZWriter::finish`].
//!
//! Errors arrive as [`std::io::Error`]s whose inner error is the crate's
//! [`Error`](crate::Error) (see the `From` conversion for the kinds).

use std::io::{self, BufRead, Read, Write};
use std::vec;
use std::vec::Vec;

use crate::error::Result;
use crate::params::{MAX_INPUT_PER_SYMBOL, Params};
use crate::sevenz::{SevenZDecoder, SevenZEncoder, SevenZStatus};

/// The stitch buffer's size: room for a tail shorter than the margin plus
/// at least a margin of new bytes.
const STITCH: usize = 2 * MAX_INPUT_PER_SYMBOL;

/// The output buffer of [`SevenZWriter`].
const WRITE_BUF: usize = 64 << 10;

/// A [`Read`] that decodes a 7z `PPMD` stream from a [`BufRead`].
///
/// It reads the inner reader to its end or to the stream's end (the known
/// size, or the end marker), whichever comes first, and leaves the reader
/// just past the bytes the coder consumed except for at most
/// [`MAX_INPUT_PER_SYMBOL`] bytes it may have taken into its stitch buffer.
///
/// ```
/// use std::io::Read;
/// use ppmd_turbo::{Params, io::SevenZReader};
///
/// let stream = [0u8; 5];
/// let mut reader = SevenZReader::new(&stream[..], Params::new(6, 1 << 16)?, Some(0))?;
/// let mut out = Vec::new();
/// reader.read_to_end(&mut out)?;
/// assert!(out.is_empty());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct SevenZReader<R: BufRead> {
    inner: R,
    decoder: SevenZDecoder,
    stitch: Vec<u8>,
    done: bool,
}

impl<R: BufRead> SevenZReader<R> {
    /// A reader over `inner` with a fresh decoder; `unpacked` as for
    /// [`SevenZDecoder::new`].
    pub fn new(inner: R, params: Params, unpacked: Option<u64>) -> Result<Self> {
        Ok(Self::from_decoder(
            inner,
            SevenZDecoder::new(params, unpacked)?,
        ))
    }

    /// A reader over `inner` driving `decoder`, which may have been built
    /// with a reused arena or FinishStream on.
    pub fn from_decoder(inner: R, decoder: SevenZDecoder) -> Self {
        Self {
            inner,
            decoder,
            stitch: Vec::new(),
            done: false,
        }
    }

    /// The decoder, for its totals.
    pub fn decoder(&self) -> &SevenZDecoder {
        &self.decoder
    }

    /// The inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The inner reader and the decoder. Bytes in the stitch buffer are
    /// dropped; the decoder's [`total_in`](SevenZDecoder::total_in) says
    /// where the coder stopped.
    pub fn into_parts(self) -> (R, SevenZDecoder) {
        (self.inner, self.decoder)
    }

    fn step(&mut self, buf: &mut [u8]) -> io::Result<SevenZStatus> {
        if self.stitch.is_empty() {
            let avail = self.inner.fill_buf()?;
            let last = avail.is_empty();
            let step = self.decoder.decode(avail, last, buf)?;
            let len = avail.len();
            if step.status == SevenZStatus::NeedInput {
                // Keep the tail the decoder could not use yet.
                self.stitch.extend_from_slice(&avail[step.consumed..]);
                self.inner.consume(len);
            } else {
                self.inner.consume(step.consumed);
            }
            return Ok(self.finish_step(step.produced, step.status));
        }
        // Stitch mode: the old tail followed by a peek at the reader's
        // buffer, which is consumed only as far as the coder takes it.
        let old = self.stitch.len();
        let avail = self.inner.fill_buf()?;
        let last = avail.is_empty();
        let take = avail.len().min(STITCH - old);
        self.stitch.extend_from_slice(&avail[..take]);
        let step = self.decoder.decode(&self.stitch, last, buf)?;
        if step.consumed >= old {
            self.inner.consume(step.consumed - old);
            self.stitch.clear();
        } else if step.status == SevenZStatus::NeedInput {
            // The reader's buffer is shorter than a symbol needs: absorb it.
            self.stitch.drain(..step.consumed);
            self.inner.consume(take);
        } else {
            self.stitch.truncate(old);
            self.stitch.drain(..step.consumed);
        }
        Ok(self.finish_step(step.produced, step.status))
    }

    fn finish_step(&mut self, _produced: usize, status: SevenZStatus) -> SevenZStatus {
        if matches!(status, SevenZStatus::ReachedSize | SevenZStatus::EndMarker) {
            self.done = true;
        }
        status
    }
}

impl<R: BufRead> Read for SevenZReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.done {
                return Ok(0);
            }
            let before = self.decoder.total_out();
            let status = self.step(buf)?;
            let produced = (self.decoder.total_out() - before) as usize;
            match status {
                SevenZStatus::NeedInput if produced == 0 => continue,
                _ => return Ok(produced),
            }
        }
    }
}

/// A [`Write`] that encodes a 7z `PPMD` stream into another writer.
///
/// Call [`finish`](Self::finish) to write the stream's tail; dropping the
/// writer without it leaves the stream unterminated.
///
/// ```
/// use std::io::{Read, Write};
/// use ppmd_turbo::{Params, io::{SevenZReader, SevenZWriter}};
///
/// let params = Params::new(6, 1 << 16)?;
/// let mut writer = SevenZWriter::new(Vec::new(), params)?;
/// writer.write_all(b"one two three two one")?;
/// writer.finish()?;
/// let stream = writer.into_inner();
///
/// let mut reader = SevenZReader::new(&stream[..], params, Some(21))?;
/// let mut out = String::new();
/// reader.read_to_string(&mut out)?;
/// assert_eq!(out, "one two three two one");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct SevenZWriter<W: Write> {
    inner: W,
    encoder: SevenZEncoder,
    buf: Vec<u8>,
    end_marker: bool,
    finished: bool,
}

impl<W: Write> SevenZWriter<W> {
    /// A writer into `inner` with a fresh encoder.
    pub fn new(inner: W, params: Params) -> Result<Self> {
        Ok(Self::from_encoder(inner, SevenZEncoder::new(params)?))
    }

    /// A writer into `inner` driving `encoder`.
    pub fn from_encoder(inner: W, encoder: SevenZEncoder) -> Self {
        Self {
            inner,
            encoder,
            buf: vec![0u8; WRITE_BUF],
            end_marker: false,
            finished: false,
        }
    }

    /// Whether [`finish`](Self::finish) writes an end marker before the
    /// coder's flush (off: 7-Zip writes none in a `.7z`).
    pub fn set_end_marker(&mut self, on: bool) {
        self.end_marker = on;
    }

    /// The encoder, for its totals.
    pub fn encoder(&self) -> &SevenZEncoder {
        &self.encoder
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Writes the stream's tail (the end marker if set, then the coder's
    /// flush) and flushes the inner writer. Only the first call writes;
    /// later calls just flush.
    pub fn finish(&mut self) -> io::Result<()> {
        while !self.finished {
            let fin = self.encoder.finish(&mut self.buf, self.end_marker)?;
            self.inner.write_all(&self.buf[..fin.produced])?;
            self.finished = fin.done;
        }
        self.inner.flush()
    }

    /// The inner writer and the encoder.
    pub fn into_parts(self) -> (W, SevenZEncoder) {
        (self.inner, self.encoder)
    }

    /// The inner writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for SevenZWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        loop {
            let step = self.encoder.encode(data, &mut self.buf)?;
            self.inner.write_all(&self.buf[..step.produced])?;
            if step.consumed > 0 {
                return Ok(step.consumed);
            }
        }
    }

    /// Every byte the encoder has settled is already in the inner writer,
    /// so this only flushes it. It never writes the stream's tail.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
