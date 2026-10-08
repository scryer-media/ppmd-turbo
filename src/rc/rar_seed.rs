//! The seed RAR range decoder, carried over from unrar-rs.
//!
//! Dmitry Subbotin's carry-less range coder as RAR 2.9 through 4.x PPMd
//! blocks use it. This is the seed implementation: the dedicated coder
//! module replaces it, keeping [`RangeDecoder`] and the public names
//! re-exported from [`crate::rc`].
//!
//! The decoder reads its input through [`ByteSource`], a buffered-reader
//! shape that lends a contiguous span and is told afterwards how much of it
//! was used. It copies a short window out of the source and reads bytes from
//! that window in its hot loop; it goes back to the source only at the
//! window's edge, and consumes exactly what it read, so a caller sharing the
//! source with another decoder (RAR switches between LZ and PPMd blocks in
//! one stream) finds it positioned right after the last byte the coder took.

use super::RangeDecoder;
use crate::error::{Error, Result};

/// A buffered source of input bytes for the range decoders.
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

const TOP: u32 = 1 << 24;
const BOT: u32 = 1 << 15;
#[cfg(test)]
const BIN_TOTAL: u32 = 1 << 14;

/// Bytes the decoder copies out of its source per refill.
const WINDOW: usize = 64;

/// The registers of a [`RarRangeDecoder`], for resuming it later.
///
/// RAR's solid archives keep one PPMd block, and so one range coder, alive
/// across member boundaries: the coder's registers are saved when a member's
/// output is complete and restored for the next member without reading the
/// four initialization bytes again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeCoderState {
    pub(crate) low: u32,
    pub(crate) code: u32,
    pub(crate) range: u32,
}

/// RAR's carry-less range decoder (Dmitry Subbotin's coder, as RAR 2.9 through
/// 4.x PPMd blocks use it).
///
/// Reads from any [`ByteSource`]; a `&[u8]` is one. Past the end of the input
/// it is fed zero bytes, the convention of every reference decoder, and
/// counts them ([`zero_bytes_past_eof`](Self::zero_bytes_past_eof)) so a
/// caller can tell a stream that ran dry from one still producing symbols.
///
/// Dropping the decoder consumes from the source exactly the bytes the coder
/// read, so the source is left positioned right after them.
pub struct RarRangeDecoder<S: ByteSource> {
    source: S,
    window: [u8; WINDOW],
    window_len: usize,
    window_pos: usize,
    consumed_before_window: usize,
    zero_bytes_past_eof: u32,
    low: u32,
    code: u32,
    range: u32,
    faulted: bool,
}

impl<S: ByteSource> RarRangeDecoder<S> {
    fn with_registers(source: S, state: RangeCoderState) -> Self {
        Self {
            source,
            window: [0; WINDOW],
            window_len: 0,
            window_pos: 0,
            consumed_before_window: 0,
            zero_bytes_past_eof: 0,
            low: state.low,
            code: state.code,
            range: state.range,
            faulted: false,
        }
    }

    /// Starts a decoder at the beginning of a coded block: reads the four
    /// big-endian bytes that initialize `code`.
    ///
    /// Returns [`Error::Truncated`] if the source holds fewer than four
    /// bytes; in that case whatever was read is still consumed.
    pub fn new(source: S) -> Result<Self> {
        let mut decoder = Self::with_registers(
            source,
            RangeCoderState {
                low: 0,
                code: 0,
                range: u32::MAX,
            },
        );
        for _ in 0..4 {
            if decoder.window_pos == decoder.window_len && !decoder.refill_window() {
                return Err(Error::Truncated);
            }
            let byte = decoder.window[decoder.window_pos];
            decoder.window_pos += 1;
            decoder.code = (decoder.code << 8) | u32::from(byte);
        }
        Ok(decoder)
    }

    /// Resumes a decoder from registers saved with [`state`](Self::state),
    /// reading no initialization bytes.
    pub fn from_state(source: S, state: RangeCoderState) -> Self {
        Self::with_registers(source, state)
    }

    /// The decoder's registers, for [`from_state`](Self::from_state).
    pub fn state(&self) -> RangeCoderState {
        RangeCoderState {
            low: self.low,
            code: self.code,
            range: self.range,
        }
    }

    /// Bytes this decoder has read from its source, not counting the zeros
    /// it was fed past the end of the input.
    pub fn position(&self) -> usize {
        self.consumed_before_window + self.window_pos
    }

    /// Zero bytes this decoder has been fed past the end of its input.
    pub fn zero_bytes_past_eof(&self) -> u32 {
        self.zero_bytes_past_eof
    }

    /// The source this decoder reads from. Bytes in the decoder's window are
    /// not yet consumed from it.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Consumes the current window from the source and copies the next one
    /// out of it. Returns `false` when the source has ended.
    #[cold]
    #[inline(never)]
    fn refill_window(&mut self) -> bool {
        self.source.consume(self.window_len);
        self.consumed_before_window += self.window_len;
        let available = self.source.fill_buf();
        let len = available.len().min(WINDOW);
        self.window[..len].copy_from_slice(&available[..len]);
        self.window_len = len;
        self.window_pos = 0;
        len != 0
    }

    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if self.window_pos == self.window_len && !self.refill_window() {
            self.zero_bytes_past_eof = self.zero_bytes_past_eof.saturating_add(1);
            return 0;
        }
        let byte = self.window[self.window_pos];
        self.window_pos += 1;
        byte
    }

    /// Subbotin's carry-less normalization.
    ///
    /// A zero range never leaves this loop: `low ^ (low + 0)` is always
    /// below `TOP`, so the reference shifts in bytes forever. Only a corrupt
    /// symbol size (zero, or a product that wraps to zero) gets here with a
    /// zero range; that is recorded as a fault so the loop ends and the model
    /// reports a corrupt stream. The reset branch cannot produce a zero range:
    /// it runs only when `low + range` crosses a `TOP` boundary with `range`
    /// below `BOT`, so `low` is not a multiple of `BOT`.
    #[inline(always)]
    fn normalize(&mut self) {
        if self.range == 0 {
            self.fault();
        }
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) >= TOP {
                if self.range >= BOT {
                    break;
                }
                self.range = self.low.wrapping_neg() & (BOT - 1);
            }
            self.code = (self.code << 8) | u32::from(self.next_byte());
            self.range <<= 8;
            self.low <<= 8;
        }
    }

    /// Records a range scaled to zero and returns a count no symbol owns.
    /// `range` is left at 1 so the arithmetic that follows stays defined.
    #[cold]
    fn fault(&mut self) -> u32 {
        self.faulted = true;
        self.range = 1;
        u32::MAX
    }
}

impl<S: ByteSource> Drop for RarRangeDecoder<S> {
    fn drop(&mut self) {
        self.source.consume(self.window_pos);
    }
}

impl<S: ByteSource> RangeDecoder for RarRangeDecoder<S> {
    #[inline(always)]
    fn get_threshold(&mut self, total: u32) -> u32 {
        // `total` is never zero: the model rejects a zero total before it
        // gets here, and `checked_div` keeps a mistake from panicking.
        self.range = self.range.checked_div(total).unwrap_or(0);
        if self.range == 0 {
            return self.fault();
        }
        self.code.wrapping_sub(self.low) / self.range
    }

    #[inline(always)]
    fn decode(&mut self, start: u32, size: u32) {
        self.low = self.low.wrapping_add(start.wrapping_mul(self.range));
        self.range = size.wrapping_mul(self.range);
        self.normalize();
    }

    #[inline(always)]
    fn decode_bit(&mut self, size0: u32, total: u32) -> u32 {
        self.range = self.range.checked_div(total).unwrap_or(0);
        let count = match self.code.wrapping_sub(self.low).checked_div(self.range) {
            Some(count) => count,
            None => self.fault(),
        };
        if count < size0 {
            self.range = size0.wrapping_mul(self.range);
            self.normalize();
            0
        } else {
            self.low = self.low.wrapping_add(size0.wrapping_mul(self.range));
            self.range = total.wrapping_sub(size0).wrapping_mul(self.range);
            self.normalize();
            1
        }
    }

    #[inline(always)]
    fn faulted(&self) -> bool {
        self.faulted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_input_is_truncated() {
        assert!(matches!(
            RarRangeDecoder::new(&[0u8, 1, 2][..]),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn init_reads_four_big_endian_bytes() {
        let input = [0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF];
        let rd = RarRangeDecoder::new(&input[..]).unwrap();
        assert_eq!(rd.code, 0);
        assert_eq!(rd.range, u32::MAX);
        assert_eq!(rd.position(), 4);
    }

    #[test]
    fn threshold_scales_the_range() {
        let input = [0x40, 0x00, 0x00, 0x00];
        let mut rd = RarRangeDecoder::new(&input[..]).unwrap();
        // code = 0x40000000, range = 0xFFFFFFFF: 0x40000000 / (0xFFFFFFFF / 256).
        assert_eq!(rd.get_threshold(256), 64);
    }

    /// A frequency total past the range is a fault, not a division by zero:
    /// the count comes back as one no symbol owns, the coder says so, and
    /// the arithmetic that follows stays defined.
    #[test]
    fn a_frequency_total_past_the_range_faults_instead_of_dividing_by_zero() {
        let input = [0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut rd = RarRangeDecoder::new(&input[..]).unwrap();
        rd.range = 100;
        assert!(!rd.faulted());
        assert_eq!(rd.get_threshold(1_000), u32::MAX);
        assert!(rd.faulted());
        assert_eq!(rd.range, 1);
        // Later calls keep working on the degenerate range.
        let _ = rd.get_threshold(7);
        let _ = rd.decode_bit(128, BIN_TOTAL);

        let mut rd = RarRangeDecoder::new(&input[..]).unwrap();
        rd.range = 1 << 13;
        assert_eq!(rd.decode_bit(1 << 13, BIN_TOTAL), 1);
        assert!(rd.faulted());

        let mut rd = RarRangeDecoder::new(&input[..]).unwrap();
        assert_eq!(rd.get_threshold(0), u32::MAX);
        assert!(rd.faulted());
    }

    /// A zero symbol size would leave a zero range, which the reference's
    /// normalization never escapes; here it faults and returns.
    #[test]
    fn a_zero_symbol_size_faults_instead_of_spinning() {
        let data = [0u8; 8];
        let mut rc = RarRangeDecoder::new(&data[..]).unwrap();
        let _ = rc.get_threshold(1 << 10);
        rc.decode(0, 0);
        assert!(rc.faulted());
        assert_ne!(rc.state().range, 0);
    }

    /// A resumed state with a range below the binary total divides by zero
    /// in `decode_bit` unless the coder catches it.
    #[test]
    fn decode_bit_on_a_range_below_the_binary_total_faults() {
        let data = [0u8; 8];
        let state = RangeCoderState {
            low: 0,
            code: 0,
            range: (1 << 14) - 1,
        };
        let mut rc = RarRangeDecoder::from_state(&data[..], state);
        let _ = rc.decode_bit(1 << 13, BIN_TOTAL);
        assert!(rc.faulted());
    }

    #[test]
    fn decode_bit_takes_the_first_outcome_below_size0() {
        let input = [0u8; 8];
        let mut rd = RarRangeDecoder::new(&input[..]).unwrap();
        assert_eq!(rd.decode_bit(128, BIN_TOTAL), 0);
    }

    /// `decode_bit` is exactly `get_threshold(1 << 14)` followed by one of
    /// the two `decode` calls: the shift and the division by `1 << 14` agree.
    #[test]
    fn decode_bit_matches_threshold_then_decode() {
        let input = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22];
        for size0 in [1u32, 100, 4096, 8192, 16000, 16383] {
            let mut bit = RarRangeDecoder::new(&input[..]).unwrap();
            let mut long = RarRangeDecoder::new(&input[..]).unwrap();
            let got = bit.decode_bit(size0, BIN_TOTAL);
            let count = long.get_threshold(BIN_TOTAL);
            let want = if count < size0 {
                long.decode(0, size0);
                0
            } else {
                long.decode(size0, BIN_TOTAL - size0);
                1
            };
            assert_eq!(got, want);
            assert_eq!(bit.state(), long.state());
            assert_eq!(bit.position(), long.position());
        }
    }

    /// Past the end of the input the coder is fed zeros and keeps producing
    /// plausible symbols; the count makes that visible to the caller.
    #[test]
    fn counts_the_zeros_it_is_fed_past_eof() {
        let input = [0u8; 4];
        let mut rc = RarRangeDecoder::new(&input[..]).unwrap();
        assert_eq!(rc.zero_bytes_past_eof(), 0);

        // A range too narrow to work with: with `low` at zero normalization
        // runs until `range` reaches `TOP`, so it takes exactly three bytes,
        // and the input holds none of them.
        rc.low = 0;
        rc.range = 1;
        rc.normalize();

        assert_eq!(rc.zero_bytes_past_eof(), 3);
        assert_eq!(rc.position(), 4);
    }

    /// A source that lends one byte at a time, like a bit reader adapter.
    struct Trickle<'a> {
        data: &'a [u8],
        consumed: usize,
    }

    impl ByteSource for Trickle<'_> {
        fn fill_buf(&mut self) -> &[u8] {
            &self.data[..self.data.len().min(1)]
        }

        fn consume(&mut self, amount: usize) {
            assert!(amount <= self.data.len().min(1));
            self.data = &self.data[amount..];
            self.consumed += amount;
        }
    }

    #[test]
    fn a_trickling_source_decodes_like_a_slice_and_is_left_at_the_coder_position() {
        let input: Vec<u8> = (0..200u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut slice = RarRangeDecoder::new(&input[..]).unwrap();
        let mut trickle = Trickle {
            data: &input,
            consumed: 0,
        };
        {
            let mut streamed = RarRangeDecoder::new(&mut trickle).unwrap();
            for step in 0..60u32 {
                let total = 300 + step;
                let a = slice.get_threshold(total);
                let b = streamed.get_threshold(total);
                assert_eq!(a, b);
                let start = a.min(total - 1);
                slice.decode(start, 1);
                streamed.decode(start, 1);
                assert_eq!(slice.state(), streamed.state());
                assert_eq!(slice.position(), streamed.position());
            }
            assert!(streamed.position() > 4);
        }
        assert_eq!(trickle.consumed, slice.position());
    }

    #[test]
    fn dropping_a_slice_decoder_advances_the_slice_by_what_it_read() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut source = &input[..];
        let position = {
            let rc = RarRangeDecoder::new(&mut source).unwrap();
            rc.position()
        };
        assert_eq!(position, 4);
        assert_eq!(source, &[5, 6, 7, 8]);
    }

    #[test]
    fn resuming_from_state_reads_no_init_bytes() {
        let input = [0x40, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78];
        let mut first = RarRangeDecoder::new(&input[..4]).unwrap();
        let _ = first.get_threshold(256);
        first.decode(1, 3);
        let state = first.state();
        let resumed = RarRangeDecoder::from_state(&input[4..], state);
        assert_eq!(resumed.state(), state);
        assert_eq!(resumed.position(), 0);
    }
}
