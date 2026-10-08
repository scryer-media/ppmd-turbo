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
/// decoder constructors take `&[u8]`, any [`ByteSource`] (owned or `&mut`)
/// or an explicit input alike.
///
/// A `&[u8]` becomes a [`SliceInput`] and a [`ByteSource`] a
/// [`SourceInput`] over it.
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
///
/// A slice is not a `ByteSource`: pass `&[u8]` to a decoder directly and it
/// reads through [`SliceInput`], with no window copies.
///
/// # Lending a span or falling back to one byte
///
/// A source that lends its own buffer when it can and otherwise produces one
/// byte at a time (a bit reader off a byte boundary, say) can stage that byte
/// in a one-byte array it owns and return a slice of it: [`SourceInput`]
/// takes the first byte of every non-empty span at once (see its
/// guarantees), so the staged byte is consumed by the next call. Written as
/// `if !span().is_empty() { return span(); }`, the lending path calls the
/// span accessor twice; that is the borrow checker's limit on returning a
/// borrow conditionally, not this trait's, and the second call is usually
/// an inlined field read.
pub trait ByteSource {
    /// Lends the bytes available at the current position without consuming
    /// them. An empty span means the input has ended, for now: a later call
    /// may return bytes again.
    fn fill_buf(&mut self) -> &[u8];

    /// Marks the first `amount` bytes of the span last returned by
    /// [`fill_buf`](Self::fill_buf) as consumed. `amount` never exceeds that
    /// span's length.
    fn consume(&mut self, amount: usize);
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
/// fetched, and on drop (or [`into_parts`](Self::into_parts)) it consumes
/// exactly the bytes taken, so it is left positioned right after the last
/// byte the coder read.
///
/// # Guarantees to the source
///
/// - [`fill_buf`](ByteSource::fill_buf) is called only when the window is
///   used up and a byte is wanted, once per window, and never otherwise.
/// - The first byte of a non-empty span is taken by that same call: a span
///   is never fetched and then left unread.
/// - Each call that returns an empty span feeds exactly one zero past the
///   end, and each zero fed past the end comes from exactly one such call.
///   So a source can count the zeros itself, one per empty span it returns.
/// - [`consume`](ByteSource::consume) is called with the bytes taken from the
///   previous span (0 after an empty one) just before each `fill_buf`, and
///   once more on drop or [`into_parts`](Self::into_parts) with the bytes
///   taken from the current one. It never exceeds that span's length.
pub struct SourceInput<S: ByteSource> {
    /// Always `Some` until [`into_parts`](Self::into_parts) takes it.
    source: Option<S>,
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
            source: Some(source),
            window: [0; WINDOW],
            window_len: 0,
            window_pos: 0,
            consumed_before_window: 0,
            zero_bytes_past_eof: 0,
        }
    }

    /// The source. Bytes in the current window are not yet consumed from it.
    pub fn source(&self) -> &S {
        self.source
            .as_ref()
            .expect("the source is only taken by into_parts")
    }

    /// Consumes the bytes taken from the current window and gives back the
    /// source, positioned right after the last byte the coder read, with the
    /// number of zeros fed past its end.
    pub fn into_parts(mut self) -> (S, u32) {
        let mut source = self
            .source
            .take()
            .expect("the source is only taken by into_parts");
        source.consume(self.window_pos);
        (source, self.zero_bytes_past_eof)
    }

    /// Consumes the current window from the source and copies the next one
    /// out of it; returns its first byte, or records the end of the input
    /// and returns 0.
    #[cold]
    #[inline(never)]
    fn refill(&mut self) -> u8 {
        let source = self
            .source
            .as_mut()
            .expect("the source is only taken by into_parts");
        source.consume(self.window_len);
        self.consumed_before_window += self.window_len;
        let available = source.fill_buf();
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
        if let Some(source) = self.source.as_mut() {
            source.consume(self.window_pos);
        }
    }
}

/// Any source, owned or borrowed: `&mut S` is a [`ByteSource`] too, so
/// `CarrylessRangeDecoder::new(&mut source)` leaves `source` usable after
/// the decoder is dropped, and `CarrylessRangeDecoder::new(source)` moves it
/// in (get it back with [`SourceInput::into_parts`]).
impl<S: ByteSource> IntoRangeInput for S {
    type Input = SourceInput<S>;

    #[inline]
    fn into_range_input(self) -> SourceInput<S> {
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

/// A test source over a slice that lends at most `span` bytes at a time and
/// counts its calls.
#[cfg(test)]
pub(crate) struct Lent<'a> {
    pub(crate) data: &'a [u8],
    span: usize,
    last: usize,
    pub(crate) fills: usize,
    pub(crate) empty_fills: u32,
}

#[cfg(test)]
impl<'a> Lent<'a> {
    pub(crate) fn new(data: &'a [u8], span: usize) -> Self {
        Self {
            data,
            span,
            last: 0,
            fills: 0,
            empty_fills: 0,
        }
    }

    /// The bytes not yet consumed.
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.data
    }
}

#[cfg(test)]
impl ByteSource for Lent<'_> {
    fn fill_buf(&mut self) -> &[u8] {
        self.fills += 1;
        self.last = self.data.len().min(self.span);
        if self.last == 0 {
            self.empty_fills += 1;
        }
        &self.data[..self.last]
    }

    fn consume(&mut self, amount: usize) {
        assert!(
            amount <= self.last,
            "consumed {amount} of a {}-byte span",
            self.last
        );
        self.data = &self.data[amount..];
        self.last -= amount;
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
        let mut source = Lent::new(&data, usize::MAX);
        {
            let mut input = (&mut source).into_range_input();
            let got = drain(&mut input, WINDOW + 10);
            assert_eq!(got, data[..WINDOW + 10]);
            assert_eq!(input.position(), WINDOW + 10);
        }
        assert_eq!(source.rest(), &data[WINDOW + 10..]);
    }

    #[test]
    fn source_input_feeds_zeros_past_the_end() {
        let data = [9u8, 8, 7];
        let mut source = Lent::new(&data, usize::MAX);
        {
            let mut input = SourceInput::new(&mut source);
            assert_eq!(drain(&mut input, 5), [9, 8, 7, 0, 0]);
            assert_eq!(input.position(), 3);
            assert_eq!(input.zero_bytes_past_eof(), 2);
        }
        assert!(source.rest().is_empty());
    }

    /// The documented call pattern: one `fill_buf` per window and one empty
    /// span per zero fed past the end, also when the source ends, resumes
    /// and ends again, and for every span size across the window size.
    #[test]
    fn source_input_fetches_one_empty_span_per_zero_past_the_end() {
        let data: Vec<u8> = (0..700u32).map(|i| (i * 13 + 1) as u8).collect();
        for span in [1, 2, 7, WINDOW - 1, WINDOW, WINDOW + 1, usize::MAX] {
            let mut source = Lent::new(&data[..300], span);
            let mut input = SourceInput::new(&mut source);
            assert_eq!(drain(&mut input, 304), [&data[..300], &[0; 4][..]].concat());
            let source = input.source.as_mut().unwrap();
            assert_eq!(source.empty_fills, 4, "span {span}");
            // More data arrives: the next fetch lends it, and the count of
            // empty spans keeps matching the count of zeros.
            source.data = &data[300..700];
            assert_eq!(drain(&mut input, 400), &data[300..700]);
            assert_eq!(drain(&mut input, 3), [0; 3]);
            assert_eq!(input.zero_bytes_past_eof(), 7);
            let (source, zeros) = input.into_parts();
            assert_eq!((source.empty_fills, zeros), (7, 7), "span {span}");
            assert!(source.rest().is_empty());
            // One fetch per window and one per zero, and no others.
            let w = span.min(WINDOW);
            let windows = 300usize.div_ceil(w) + 400usize.div_ceil(w);
            assert_eq!(source.fills, windows + 7, "span {span}");
        }
    }

    /// An owned source moves into a decoder and comes back with
    /// `into_parts`, positioned after the coder's bytes.
    #[test]
    fn an_owned_source_comes_back_from_the_decoder() {
        let mut data = vec![0u8, 0x12, 0x34, 0x56, 0x78];
        data.extend_from_slice(b"tail");
        let dec = crate::rc::RarRangeDecoder::new(Lent::new(&data, 3)).unwrap();
        let (source, zeros) = dec.into_input().into_parts();
        assert_eq!(source.rest(), b"\x78tail");
        assert_eq!(zeros, 0);

        let dec = crate::rc::RarRangeDecoder::new(Lent::new(&data[..2], 3));
        assert!(matches!(dec, Err(crate::Error::Truncated)));
    }

    /// A source that lends its buffer when it can and otherwise stages one
    /// byte in itself, as the type's docs describe; every staged byte is
    /// taken by the fetch that returned it.
    #[test]
    fn a_staged_one_byte_span_is_always_taken() {
        struct Staged<'a> {
            data: &'a [u8],
            byte: [u8; 1],
            staged: bool,
            fetches: usize,
        }
        impl ByteSource for Staged<'_> {
            fn fill_buf(&mut self) -> &[u8] {
                assert!(!self.staged, "a staged byte was fetched twice");
                self.fetches += 1;
                // Lend on even fetches, stage one byte on odd ones.
                if self.fetches.is_multiple_of(2) {
                    return self.data;
                }
                match self.data.split_first() {
                    Some((&b, _)) => {
                        self.byte[0] = b;
                        self.staged = true;
                        &self.byte
                    }
                    None => &[],
                }
            }
            fn consume(&mut self, amount: usize) {
                if self.staged {
                    assert!(amount <= 1);
                    self.staged = amount == 0;
                }
                self.data = &self.data[amount..];
            }
        }
        let data: Vec<u8> = (0..600u32).map(|i| i as u8 ^ 0x5A).collect();
        let mut source = Staged {
            data: &data,
            byte: [0],
            staged: false,
            fetches: 0,
        };
        let mut input = SourceInput::new(&mut source);
        assert_eq!(drain(&mut input, 600), data);
        drop(input);
        assert!(!source.staged && source.data.is_empty());
    }
}
