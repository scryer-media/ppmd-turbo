//! Where the range encoders write their bytes.
//!
//! The encoders emit one byte per normalization step (and runs of carry
//! bytes from the 7z coder's `ShiftLow`), so, as on the input side, the
//! per-byte path is an inlined store into a buffer and anything slower
//! happens at the buffer's edge. [`RangeOutput`] is that path; the encoders
//! are generic over it.
//!
//! Three backings:
//!
//! - `Vec<u8>` (owned or `&mut`), which grows as needed;
//! - [`SliceOutput`], a caller's fixed buffer: running out of room is
//!   recorded and reported by [`RangeOutput::finish`], never a panic;
//! - [`WriteOutput`], an owned buffer (64 KiB by default) flushed to any
//!   [`std::io::Write`] when full and at the end. A write error is kept and
//!   reported by `finish`; bytes after it are dropped.

use std::io::{ErrorKind, Write};

use crate::error::{Error, Result};

/// The default buffer size of [`WriteOutput`]: 64 KiB.
pub const DEFAULT_FLUSH_SIZE: usize = 1 << 16;

/// The byte sink a range encoder writes to.
pub trait RangeOutput {
    /// Appends one byte. Never fails here: a sink that cannot take it
    /// records the failure and reports it from [`finish`](Self::finish).
    fn write_byte(&mut self, byte: u8);

    /// Flushes whatever is buffered to the final sink and reports any
    /// failure recorded since the sink was created.
    fn finish(&mut self) -> Result<()>;
}

impl RangeOutput for Vec<u8> {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        self.push(byte);
    }

    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

impl<O: RangeOutput + ?Sized> RangeOutput for &mut O {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        (**self).write_byte(byte);
    }

    fn finish(&mut self) -> Result<()> {
        (**self).finish()
    }
}

/// A caller's fixed-size buffer.
///
/// Writing past its end is recorded, not a panic: [`finish`](RangeOutput::finish)
/// then returns an [`Error::Io`] of kind [`ErrorKind::WriteZero`].
#[derive(Debug)]
pub struct SliceOutput<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflowed: bool,
}

impl<'a> SliceOutput<'a> {
    /// Writes into `buf` from its first byte.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflowed: false,
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether a byte was dropped for lack of room.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// The bytes written so far.
    pub fn written(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl RangeOutput for SliceOutput<'_> {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        } else {
            self.overflowed = true;
        }
    }

    fn finish(&mut self) -> Result<()> {
        if self.overflowed {
            return Err(Error::Io(ErrorKind::WriteZero.into()));
        }
        Ok(())
    }
}

/// An owned buffer flushed to a [`std::io::Write`] when full and at
/// [`finish`](RangeOutput::finish).
#[derive(Debug)]
pub struct WriteOutput<W: Write> {
    writer: W,
    buf: Vec<u8>,
    flush_size: usize,
    error: Option<std::io::Error>,
}

impl<W: Write> WriteOutput<W> {
    /// Buffers [`DEFAULT_FLUSH_SIZE`] bytes between writes.
    pub fn new(writer: W) -> Self {
        Self::with_flush_size(writer, DEFAULT_FLUSH_SIZE)
    }

    /// Buffers `flush_size` bytes (at least 1) between writes.
    pub fn with_flush_size(writer: W, flush_size: usize) -> Self {
        let flush_size = flush_size.max(1);
        Self {
            writer,
            buf: Vec::with_capacity(flush_size),
            flush_size,
            error: None,
        }
    }

    /// The writer. Buffered bytes have not reached it yet.
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Unwraps the writer. Call [`finish`](RangeOutput::finish) first, or
    /// buffered bytes are lost.
    pub fn into_inner(self) -> W {
        self.writer
    }

    #[cold]
    #[inline(never)]
    fn flush_buf(&mut self) {
        if self.error.is_none()
            && let Err(e) = self.writer.write_all(&self.buf)
        {
            self.error = Some(e);
        }
        self.buf.clear();
    }
}

impl<W: Write> RangeOutput for WriteOutput<W> {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        self.buf.push(byte);
        if self.buf.len() >= self.flush_size {
            self.flush_buf();
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.flush_buf();
        if let Some(e) = self.error.take() {
            return Err(Error::Io(e));
        }
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_output_reports_overflow_at_finish() {
        let mut buf = [0u8; 2];
        let mut out = SliceOutput::new(&mut buf);
        for b in [1, 2, 3] {
            out.write_byte(b);
        }
        assert!(out.overflowed());
        assert_eq!(out.written(), [1, 2]);
        assert!(matches!(out.finish(), Err(Error::Io(e)) if e.kind() == ErrorKind::WriteZero));
    }

    #[test]
    fn write_output_flushes_at_the_edge_and_at_finish() {
        let mut out = WriteOutput::with_flush_size(Vec::new(), 3);
        for b in 0..7u8 {
            out.write_byte(b);
        }
        assert_eq!(out.get_ref().len(), 6);
        out.finish().unwrap();
        assert_eq!(out.into_inner(), [0, 1, 2, 3, 4, 5, 6]);
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("pipe gone"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_output_keeps_the_first_error() {
        let mut out = WriteOutput::with_flush_size(Broken, 1);
        out.write_byte(1);
        out.write_byte(2);
        assert!(matches!(out.finish(), Err(Error::Io(e)) if e.to_string() == "pipe gone"));
    }
}
