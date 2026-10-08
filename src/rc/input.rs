//! Where the range decoders read their bytes from.
//!
//! A range decoder pulls one byte per normalization step, so the per-byte
//! path is the one that matters. [`RangeInput`] is that path: a decoder is
//! generic over it, so after monomorphization `next_byte` is an inlined
//! field load and one predictable comparison against the end of the current
//! buffer. Anything slower (a `Read` call, a refill from a shared
//! [`ByteSource`]) happens only when that buffer runs out, in a `#[cold]`
//! function.
//!
//! Three backings:
//!
//! - [`SliceInput`] borrows the whole stream as `&[u8]` and never refills:
//!   the fastest path, and the one the batch decode loop is designed around.
//! - [`ReadInput`] owns a refill buffer (64 KiB by default, configurable)
//!   over any [`std::io::Read`], so an unbuffered `File` costs one `read`
//!   per refill rather than per byte.
//! - [`SourceInput`] reads from a [`ByteSource`] another reader shares (RAR
//!   switches between LZ and PPMd blocks in one bit stream). It copies a
//!   short window out of the source and, on drop, consumes from the source
//!   exactly the bytes the coder took, so the source is left positioned
//!   right after them.
//!
//! **Past the end of the input** every backing returns zero bytes and counts
//! them ([`RangeInput::zero_bytes_past_eof`]). That is what RARLAB unrar and
//! unrar-rs do (`read_byte_or_zero`), and what 7-Zip does through its byte
//! reader, which returns 0 at EOF and raises its `Extra` flag. A decoder
//! therefore never fails on a short stream mid-symbol; the framing decides
//! whether the padding it was fed is legal (RAR tolerates a little mid-block,
//! 7z tolerates none) by reading the count.

use std::io::{ErrorKind, Read};

/// The default refill size of [`ReadInput`]: 64 KiB.
pub const DEFAULT_REFILL_SIZE: usize = 1 << 16;

/// Bytes [`SourceInput`] copies out of its [`ByteSource`] per refill. A power
/// of two, so the window index can be masked instead of bounds-checked.
const WINDOW: usize = 256;

/// The byte stream a range decoder reads, one byte per normalization step.
///
/// Implementations keep the per-byte path to a load and a comparison and
/// move every refill out of line. Past the end of the input `next_byte`
/// returns 0 and counts it; it never fails and never panics.
pub trait RangeInput {
    /// Takes the next byte, or 0 once the input has ended (counted by
    /// [`zero_bytes_past_eof`](Self::zero_bytes_past_eof)).
    fn next_byte(&mut self) -> u8;

    /// Bytes taken from the input so far, not counting the zeros fed past
    /// its end.
    fn position(&self) -> usize;

    /// Zero bytes fed past the end of the input so far.
    fn zero_bytes_past_eof(&self) -> u32;

    /// Takes the I/O error that ended the input, if one did. Inputs that
    /// cannot fail return `None`.
    fn take_io_error(&mut self) -> Option<std::io::Error> {
        None
    }
}

/// Converts a value into the [`RangeInput`] a decoder reads from, so the
/// decoder constructors take `&[u8]`, `&mut impl ByteSource` or an explicit
/// input alike.
pub trait IntoRangeInput {
    /// The input this value becomes.
    type Input: RangeInput;

    /// Performs the conversion.
    fn into_range_input(self) -> Self::Input;
}

// ---------------------------------------------------------------------------
// Borrowed slice

/// A whole stream borrowed as one slice: no refills, no copies.
///
/// The per-byte path is `data.get(pos)`: one bounds comparison that doubles
/// as the end-of-input test, so the fast path needs no `unsafe` and has no
/// second check.
#[derive(Clone, Debug)]
pub struct SliceInput<'a> {
    data: &'a [u8],
    pos: usize,
    zero_bytes_past_eof: u32,
}

impl<'a> SliceInput<'a> {
    /// Reads `data` from its first byte.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            zero_bytes_past_eof: 0,
        }
    }

    /// The bytes not yet taken.
    pub fn remaining(&self) -> &'a [u8] {
        self.data.get(self.pos..).unwrap_or_default()
    }
}

impl RangeInput for SliceInput<'_> {
    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if let Some(&byte) = self.data.get(self.pos) {
            self.pos += 1;
            byte
        } else {
            self.past_eof()
        }
    }

    #[inline]
    fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    fn zero_bytes_past_eof(&self) -> u32 {
        self.zero_bytes_past_eof
    }
}

impl SliceInput<'_> {
    #[cold]
    #[inline(never)]
    fn past_eof(&mut self) -> u8 {
        self.zero_bytes_past_eof = self.zero_bytes_past_eof.saturating_add(1);
        0
    }
}

impl<'a> IntoRangeInput for &'a [u8] {
    type Input = SliceInput<'a>;

    #[inline]
    fn into_range_input(self) -> SliceInput<'a> {
        SliceInput::new(self)
    }
}

impl<'a> IntoRangeInput for SliceInput<'a> {
    type Input = Self;

    #[inline]
    fn into_range_input(self) -> Self {
        self
    }
}

// ---------------------------------------------------------------------------
// Owned buffer over `Read`

/// An owned refill buffer over a [`std::io::Read`].
///
/// The buffer is a `Vec` whose length is the filled part, so the per-byte
/// path is the same single `get(pos)` comparison as [`SliceInput`]'s; the
/// `read` call happens once per refill. An I/O error ends the input (the
/// decoder is fed zeros from then on, as at EOF) and is kept for the caller:
/// see [`io_error`](Self::io_error) and [`take_io_error`](Self::take_io_error).
/// [`std::io::ErrorKind::Interrupted`] is retried.
#[derive(Debug)]
pub struct ReadInput<R: Read> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    refill_size: usize,
    consumed_before_buf: usize,
    zero_bytes_past_eof: u32,
    ended: bool,
    error: Option<std::io::Error>,
}

impl<R: Read> ReadInput<R> {
    /// Reads through a buffer of [`DEFAULT_REFILL_SIZE`] bytes.
    pub fn new(reader: R) -> Self {
        Self::with_refill_size(reader, DEFAULT_REFILL_SIZE)
    }

    /// Reads through a buffer of `refill_size` bytes (at least 1).
    pub fn with_refill_size(reader: R, refill_size: usize) -> Self {
        let refill_size = refill_size.max(1);
        Self {
            reader,
            buf: Vec::with_capacity(refill_size),
            pos: 0,
            refill_size,
            consumed_before_buf: 0,
            zero_bytes_past_eof: 0,
            ended: false,
            error: None,
        }
    }

    /// The I/O error that ended the input, if one did.
    pub fn io_error(&self) -> Option<&std::io::Error> {
        self.error.as_ref()
    }

    /// Takes the I/O error that ended the input, if one did.
    pub fn take_io_error(&mut self) -> Option<std::io::Error> {
        self.error.take()
    }

    /// The bytes already read from the reader but not yet taken.
    pub fn buffered(&self) -> &[u8] {
        self.buf.get(self.pos..).unwrap_or_default()
    }

    /// The reader. Bytes in the buffer are already read from it.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Unwraps the reader. Buffered bytes not yet taken are lost; read
    /// [`buffered`](Self::buffered) first if they matter.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Refills the buffer and returns its first byte, or records the end of
    /// the input and returns 0.
    #[cold]
    #[inline(never)]
    fn refill(&mut self) -> u8 {
        if !self.ended {
            self.consumed_before_buf += self.buf.len();
            self.buf.clear();
            self.buf.resize(self.refill_size, 0);
            self.pos = 0;
            let filled = loop {
                match self.reader.read(&mut self.buf) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.error = Some(e);
                        break 0;
                    }
                }
            };
            // A reader that claims more than it was given is broken; trust
            // only the buffer.
            self.buf.truncate(filled.min(self.refill_size));
            if let Some(&byte) = self.buf.first() {
                self.pos = 1;
                return byte;
            }
            self.ended = true;
        }
        self.zero_bytes_past_eof = self.zero_bytes_past_eof.saturating_add(1);
        0
    }
}

impl<R: Read> RangeInput for ReadInput<R> {
    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if let Some(&byte) = self.buf.get(self.pos) {
            self.pos += 1;
            byte
        } else {
            self.refill()
        }
    }

    #[inline]
    fn position(&self) -> usize {
        self.consumed_before_buf + self.pos
    }

    #[inline]
    fn zero_bytes_past_eof(&self) -> u32 {
        self.zero_bytes_past_eof
    }

    fn take_io_error(&mut self) -> Option<std::io::Error> {
        self.error.take()
    }
}

impl<R: Read> IntoRangeInput for ReadInput<R> {
    type Input = Self;

    #[inline]
    fn into_range_input(self) -> Self {
        self
    }
}

// ---------------------------------------------------------------------------
// Shared buffered source

/// A buffered source of input bytes shared with another reader.
///
/// The same contract as [`std::io::BufRead`], without errors: a source that
/// fails returns an empty span and reports its failure through its own API.
pub trait ByteSource {
    /// Lends the bytes available at the current position without consuming
    /// them. An empty span means the input has ended.
    fn fill_buf(&mut self) -> &[u8];

    /// Marks the first `amount` bytes of the span last returned by
    /// [`fill_buf`](Self::fill_buf) as consumed. `amount` never exceeds that
    /// span's length.
    fn consume(&mut self, amount: usize);
}

impl ByteSource for &[u8] {
    #[inline]
    fn fill_buf(&mut self) -> &[u8] {
        self
    }

    #[inline]
    fn consume(&mut self, amount: usize) {
        *self = self.get(amount..).unwrap_or_default();
    }
}

impl<T: ByteSource + ?Sized> ByteSource for &mut T {
    #[inline]
    fn fill_buf(&mut self) -> &[u8] {
        (**self).fill_buf()
    }

    #[inline]
    fn consume(&mut self, amount: usize) {
        (**self).consume(amount);
    }
}

/// Reads from a [`ByteSource`] through a short window copied out of it.
///
/// The decoder reads the window, not the source: the per-byte path is a
/// comparison and a masked (so unchecked-by-construction) array load, and the
/// source is called only when the window is used up. The window's bytes are
/// lent, not consumed: the source consumes a window when the next one is
/// fetched, and on drop it consumes exactly the bytes taken, so it is left
/// positioned right after the last byte the coder read.
pub struct SourceInput<S: ByteSource> {
    source: S,
    window: [u8; WINDOW],
    window_len: usize,
    window_pos: usize,
    consumed_before_window: usize,
    zero_bytes_past_eof: u32,
}

impl<S: ByteSource> SourceInput<S> {
    /// Reads `source` from its current position.
    pub fn new(source: S) -> Self {
        Self {
            source,
            window: [0; WINDOW],
            window_len: 0,
            window_pos: 0,
            consumed_before_window: 0,
            zero_bytes_past_eof: 0,
        }
    }

    /// The source. Bytes in the current window are not yet consumed from it.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Consumes the current window from the source and copies the next one
    /// out of it; returns its first byte, or records the end of the input
    /// and returns 0.
    #[cold]
    #[inline(never)]
    fn refill(&mut self) -> u8 {
        self.source.consume(self.window_len);
        self.consumed_before_window += self.window_len;
        let available = self.source.fill_buf();
        let len = available.len().min(WINDOW);
        self.window[..len].copy_from_slice(&available[..len]);
        self.window_len = len;
        self.window_pos = 0;
        if len == 0 {
            self.zero_bytes_past_eof = self.zero_bytes_past_eof.saturating_add(1);
            return 0;
        }
        self.window_pos = 1;
        self.window[0]
    }
}

impl<S: ByteSource> RangeInput for SourceInput<S> {
    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if self.window_pos < self.window_len {
            // `window_pos < window_len <= WINDOW`, so the mask is the
            // identity; it only lets the compiler drop the bounds check.
            let byte = self.window[self.window_pos & (WINDOW - 1)];
            self.window_pos += 1;
            byte
        } else {
            self.refill()
        }
    }

    #[inline]
    fn position(&self) -> usize {
        self.consumed_before_window + self.window_pos
    }

    #[inline]
    fn zero_bytes_past_eof(&self) -> u32 {
        self.zero_bytes_past_eof
    }
}

impl<S: ByteSource> Drop for SourceInput<S> {
    fn drop(&mut self) {
        self.source.consume(self.window_pos);
    }
}

impl<'a, S: ByteSource + ?Sized> IntoRangeInput for &'a mut S {
    type Input = SourceInput<&'a mut S>;

    #[inline]
    fn into_range_input(self) -> SourceInput<&'a mut S> {
        SourceInput::new(self)
    }
}

impl<S: ByteSource> IntoRangeInput for SourceInput<S> {
    type Input = Self;

    #[inline]
    fn into_range_input(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain<I: RangeInput>(input: &mut I, n: usize) -> Vec<u8> {
        (0..n).map(|_| input.next_byte()).collect()
    }

    #[test]
    fn slice_input_feeds_zeros_past_the_end_and_counts_them() {
        let mut input = SliceInput::new(&[1, 2, 3]);
        assert_eq!(drain(&mut input, 6), [1, 2, 3, 0, 0, 0]);
        assert_eq!(input.position(), 3);
        assert_eq!(input.zero_bytes_past_eof(), 3);
        assert!(input.remaining().is_empty());
    }

    /// A reader that hands out at most `step` bytes per call and is
    /// interrupted once before every successful read.
    struct Choppy<'a> {
        data: &'a [u8],
        step: usize,
        interrupt: bool,
    }

    impl Read for Choppy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(ErrorKind::Interrupted.into());
            }
            let n = self.data.len().min(self.step).min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn read_input_matches_slice_input_for_any_refill_size() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut want = SliceInput::new(&data);
        let want_bytes = drain(&mut want, 1010);
        for (refill, step) in [(1, 1), (3, 2), (64, 1000), (1 << 16, 17), (7, 5)] {
            let mut input = ReadInput::with_refill_size(
                Choppy {
                    data: &data,
                    step,
                    interrupt: false,
                },
                refill,
            );
            assert_eq!(drain(&mut input, 1010), want_bytes);
            assert_eq!(input.position(), 1000);
            assert_eq!(input.zero_bytes_past_eof(), 10);
            assert!(input.io_error().is_none());
        }
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }

    #[test]
    fn read_input_keeps_the_error_and_feeds_zeros() {
        let mut input = ReadInput::new(Failing);
        assert_eq!(drain(&mut input, 3), [0, 0, 0]);
        assert_eq!(input.zero_bytes_past_eof(), 3);
        assert_eq!(input.take_io_error().unwrap().to_string(), "disk on fire");
    }

    #[test]
    fn source_input_leaves_the_source_after_the_last_byte_taken() {
        let data: Vec<u8> = (0..=255u8).cycle().take(WINDOW * 3 + 5).collect();
        let mut source = &data[..];
        {
            let mut input = (&mut source).into_range_input();
            let got = drain(&mut input, WINDOW + 10);
            assert_eq!(got, data[..WINDOW + 10]);
            assert_eq!(input.position(), WINDOW + 10);
        }
        assert_eq!(source, &data[WINDOW + 10..]);
    }

    #[test]
    fn source_input_feeds_zeros_past_the_end() {
        let data = [9u8, 8, 7];
        let mut source = &data[..];
        {
            let mut input = SourceInput::new(&mut source);
            assert_eq!(drain(&mut input, 5), [9, 8, 7, 0, 0]);
            assert_eq!(input.position(), 3);
            assert_eq!(input.zero_bytes_past_eof(), 2);
        }
        assert!(source.is_empty());
    }
}
