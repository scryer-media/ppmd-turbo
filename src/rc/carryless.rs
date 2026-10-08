//! The carry-less range coder: Dmitry Subbotin's design (1999, public
//! domain), as Dmitry Shkarin's PPMd variant H reference uses it for `.pmd`
//! streams (7-Zip's `Ppmd7a`, `C/Ppmd7aDec.c`) and as RAR 2.9 through 4.x use
//! it for their PPMd blocks.
//!
//! The coder never propagates a carry: when the range gets small while the
//! top byte of `low` is still unsettled, it throws away the code space up to
//! the next `BOT` boundary (`range = -low & (BOT - 1)`), so the top byte
//! settles and can be shifted out. That costs a little compression and
//! makes the encoder a plain byte shifter.
//!
//! The decoder keeps absolute `low` and `code` as RAR's decoder does;
//! 7-Zip's `Ppmd7a` decoder keeps `code` relative to `low` instead. The two
//! are the same arithmetic. All registers are `u32` and all arithmetic wraps.
//!
//! **Corrupt input.** Unlike the 7z coder, the carry-less range can fall
//! below a frequency total, and then `range / total` is zero. 7-Zip's
//! `Ppmd7a` decoder checks `summFreq > Range` and `freqSum > Range` before
//! dividing; RARLAB unrar does not (a crafted stream divides by zero there).
//! Here a range scaled to zero, or a symbol size of zero, is a sticky fault
//! ([`RangeDecoder::faulted`]) and the RAR-style
//! [`get_current_count`](CarrylessRangeDecoder::get_current_count) returns
//! [`Error::CorruptStream`]. A zero range must never reach normalization:
//! `low ^ (low + 0)` is always below `TOP`, so the reference loop would
//! shift in bytes forever.

use super::{BIN_TOTAL, BIN_TOTAL_BITS, BOT, TOP, corrupt};
use super::{IntoRangeInput, RangeDecoder, RangeEncoder, RangeInput, RangeOutput};
use crate::error::{Error, Result};

/// The registers of a [`CarrylessRangeDecoder`], for resuming it later.
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

impl RangeCoderState {
    /// Registers as given. A state saved with
    /// [`CarrylessRangeDecoder::state`] is the normal source; this lets a
    /// caller that kept the three values elsewhere (or a test that needs a
    /// particular state) rebuild one. Any values are safe: a range the
    /// coder cannot scale is reported as a fault, never a division by zero.
    pub fn new(low: u32, code: u32, range: u32) -> Self {
        Self { low, code, range }
    }
}

/// The carry-less range decoder.
///
/// Reads through any [`RangeInput`]. Past the end of the input it is fed
/// zeros, as RARLAB unrar and every reference decoder are, and counts them
/// ([`zero_bytes_past_eof`](Self::zero_bytes_past_eof)) so the framing can
/// tell a stream that ran dry from one still producing symbols.
#[derive(Debug)]
pub struct CarrylessRangeDecoder<I: RangeInput> {
    low: u32,
    code: u32,
    range: u32,
    faulted: bool,
    input: I,
}

impl<I: RangeInput> CarrylessRangeDecoder<I> {
    /// Starts a decoder at the beginning of a coded block: `low = 0`,
    /// `range = 0xFFFFFFFF`, and four big-endian bytes into `code` (RAR's
    /// `InitDecoder`). Any code is accepted, as unrar accepts it.
    ///
    /// Errors: [`Error::Truncated`] if the input holds fewer than four bytes.
    pub fn new<T: IntoRangeInput<Input = I>>(input: T) -> Result<Self> {
        let mut input = input.into_range_input();
        let mut code = 0u32;
        for _ in 0..4 {
            code = (code << 8) | u32::from(input.next_byte());
        }
        if input.zero_bytes_past_eof() != 0 {
            return Err(input.take_io_error().map_or(Error::Truncated, Error::Io));
        }
        Ok(Self::from_parts(
            input,
            RangeCoderState {
                low: 0,
                code,
                range: u32::MAX,
            },
        ))
    }

    /// [`new`](Self::new) with 7-Zip's `Ppmd7a_RangeDec_Init` check: a code
    /// of `0xFFFFFFFF` is [`Error::CorruptStream`].
    pub fn new_7a<T: IntoRangeInput<Input = I>>(input: T) -> Result<Self> {
        let decoder = Self::new(input)?;
        if decoder.code == u32::MAX {
            return Err(corrupt(
                "carry-less range coder: initial code is 0xFFFFFFFF",
            ));
        }
        Ok(decoder)
    }

    /// Resumes a decoder from registers saved with [`state`](Self::state),
    /// reading no initialization bytes.
    pub fn from_state<T: IntoRangeInput<Input = I>>(input: T, state: RangeCoderState) -> Self {
        Self::from_parts(input.into_range_input(), state)
    }

    fn from_parts(input: I, state: RangeCoderState) -> Self {
        Self {
            low: state.low,
            code: state.code,
            range: state.range,
            faulted: false,
            input,
        }
    }

    /// The decoder's registers, for [`from_state`](Self::from_state).
    #[inline]
    pub fn state(&self) -> RangeCoderState {
        RangeCoderState {
            low: self.low,
            code: self.code,
            range: self.range,
        }
    }

    /// `Ppmd7a_RangeDec_IsFinishedOK`: `code - low == 0` (7-Zip keeps the
    /// difference as its `Code`).
    #[inline]
    pub fn is_finished_ok(&self) -> bool {
        self.code == self.low
    }

    /// Bytes read from the input, not counting zeros fed past its end.
    #[inline]
    pub fn position(&self) -> usize {
        self.input.position()
    }

    /// Zero bytes fed past the end of the input.
    #[inline]
    pub fn zero_bytes_past_eof(&self) -> u32 {
        self.input.zero_bytes_past_eof()
    }

    /// The input.
    pub fn input(&self) -> &I {
        &self.input
    }

    /// The input, mutably.
    pub fn input_mut(&mut self) -> &mut I {
        &mut self.input
    }

    /// Unwraps the input.
    pub fn into_input(self) -> I {
        self.input
    }

    /// RAR's `GetCurrentCount`: `range /= scale; return (code - low) / range`.
    ///
    /// Errors: [`Error::CorruptStream`] if `range / scale` is zero, or if the
    /// coder faulted earlier.
    #[inline(always)]
    pub fn get_current_count(&mut self, scale: u32) -> Result<u32> {
        let count = self.get_threshold(scale);
        if self.faulted {
            return Err(corrupt(
                "carry-less range coder: frequency total past the range",
            ));
        }
        Ok(count)
    }

    /// RAR's `Decode` with `SubRange = {cum, cum + freq, scale}` followed by
    /// `ARI_DEC_NORMALIZE`. `scale` was applied by
    /// [`get_current_count`](Self::get_current_count) and is unused here, as
    /// in the reference.
    #[inline(always)]
    pub fn decode_freq(&mut self, cum: u32, freq: u32, _scale: u32) {
        self.decode(cum, freq);
    }

    /// RAR's binary decision over `scale`: `get_current_count(scale)`
    /// compared with `freq0`, then the matching `decode`. Returns `true`
    /// when the first outcome (`[0, freq0)`) was decoded.
    ///
    /// Errors: as [`get_current_count`](Self::get_current_count).
    #[inline]
    pub fn decode_binary(&mut self, freq0: u32, scale: u32) -> Result<bool> {
        let count = self.get_current_count(scale)?;
        if count < freq0 {
            self.decode(0, freq0);
            Ok(true)
        } else {
            self.decode(freq0, scale.wrapping_sub(freq0));
            Ok(false)
        }
    }

    /// Subbotin's normalization (`ARI_DEC_NORMALIZE`; `RC_NORM` in
    /// `Ppmd7aDec.c`):
    ///
    /// ```text
    /// while ((low ^ (low + range)) < TOP
    ///        || (range < BOT && ((range = -low & (BOT - 1)), 1)))
    ///   { code = code << 8 | byte; range <<= 8; low <<= 8; }
    /// ```
    ///
    /// The truncation branch cannot produce a zero range: it runs only when
    /// `low + range` crosses a `TOP` boundary with `range < BOT`, so `low` is
    /// not a multiple of `BOT`. Inside the loop `range < TOP`, so the shift
    /// never drops a set bit, and the loop ends after at most four bytes.
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
            self.code = (self.code << 8) | u32::from(self.input.next_byte());
            self.range <<= 8;
            self.low <<= 8;
        }
    }

    /// Records a range scaled to zero and returns a count no symbol owns.
    /// `range` is left at 1 so the arithmetic that follows stays defined.
    #[cold]
    #[inline(never)]
    fn fault(&mut self) -> u32 {
        self.faulted = true;
        self.range = 1;
        u32::MAX
    }
}

impl<I: RangeInput> RangeDecoder for CarrylessRangeDecoder<I> {
    /// `GetCurrentCount`: `range /= total; (code - low) / range`.
    #[inline(always)]
    fn get_threshold(&mut self, total: u32) -> u32 {
        self.range = self.range.checked_div(total).unwrap_or(0);
        if self.range == 0 {
            return self.fault();
        }
        self.code.wrapping_sub(self.low) / self.range
    }

    /// `low += start * range; range *= size`, then normalize.
    #[inline(always)]
    fn decode(&mut self, start: u32, size: u32) {
        self.low = self.low.wrapping_add(start.wrapping_mul(self.range));
        self.range = size.wrapping_mul(self.range);
        self.normalize();
    }

    /// RAR's `GetCurrentShiftCount(TOT_BITS)` (`range >>= 14`) and the
    /// matching decode. 7-Zip writes the second outcome's range as
    /// `(Range & ~(BIN_SCALE - 1)) - size0` on the unshifted range
    /// (`Ppmd7aDec.c:176`); that is the same number.
    #[inline(always)]
    fn decode_bit(&mut self, size0: u32) -> u32 {
        self.range >>= BIN_TOTAL_BITS;
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
            self.range = BIN_TOTAL.wrapping_sub(size0).wrapping_mul(self.range);
            self.normalize();
            1
        }
    }

    #[inline(always)]
    fn faulted(&self) -> bool {
        self.faulted
    }
}

/// The carry-less range encoder: the mirror of [`CarrylessRangeDecoder`], as
/// Shkarin's variant H reference encodes `.pmd` streams.
///
/// Each normalization step writes `low >> 24`; [`finish`](Self::finish)
/// writes the four bytes of `low` and flushes the output. This encoder
/// writes raw PPMd streams only; it never produces RAR archives.
#[derive(Debug)]
pub struct CarrylessRangeEncoder<O: RangeOutput> {
    low: u32,
    range: u32,
    faulted: bool,
    out: O,
}

impl<O: RangeOutput> CarrylessRangeEncoder<O> {
    /// `low = 0`, `range = 0xFFFFFFFF`.
    pub fn new(out: O) -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            faulted: false,
            out,
        }
    }

    /// The output.
    pub fn output(&self) -> &O {
        &self.out
    }

    /// Writes the four bytes of `low`, most significant first, then flushes
    /// the output and returns it.
    ///
    /// Errors: the output's error, or [`Error::CorruptStream`] if the coder
    /// faulted (see [`RangeEncoder::faulted`]).
    pub fn finish(mut self) -> Result<O> {
        for _ in 0..4 {
            self.out.write_byte((self.low >> 24) as u8);
            self.low <<= 8;
        }
        self.out.finish()?;
        if self.faulted {
            return Err(corrupt(
                "carry-less range encoder: frequency total past the range",
            ));
        }
        Ok(self.out)
    }

    /// Subbotin's normalization, writing `low >> 24` per step. The callers
    /// never leave a zero range, so the loop ends after at most four bytes.
    #[inline(always)]
    fn normalize(&mut self) {
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) >= TOP {
                if self.range >= BOT {
                    break;
                }
                self.range = self.low.wrapping_neg() & (BOT - 1);
            }
            self.out.write_byte((self.low >> 24) as u8);
            self.range <<= 8;
            self.low <<= 8;
        }
    }

    #[cold]
    #[inline(never)]
    fn fault(&mut self) {
        self.faulted = true;
    }
}

impl<O: RangeOutput> RangeEncoder for CarrylessRangeEncoder<O> {
    /// `range /= total; low += start * range; range *= size`, then normalize.
    #[inline(always)]
    fn encode(&mut self, start: u32, size: u32, total: u32) {
        let range = self.range.checked_div(total).unwrap_or(0);
        let range_out = range.wrapping_mul(size);
        if range_out == 0 {
            return self.fault();
        }
        self.low = self.low.wrapping_add(start.wrapping_mul(range));
        self.range = range_out;
        self.normalize();
    }

    /// `range >>= 14`, then `[0, size0)` for bit 0 or `[size0, 2^14)` for
    /// bit 1, then normalize.
    #[inline(always)]
    fn encode_bit(&mut self, size0: u32, bit: u32) {
        let range = self.range >> BIN_TOTAL_BITS;
        let (start, size) = if bit == 0 {
            (0, size0)
        } else {
            (size0, BIN_TOTAL.wrapping_sub(size0))
        };
        let range_out = size.wrapping_mul(range);
        if range_out == 0 {
            return self.fault();
        }
        self.low = self.low.wrapping_add(start.wrapping_mul(range));
        self.range = range_out;
        self.normalize();
    }

    #[inline(always)]
    fn faulted(&self) -> bool {
        self.faulted
    }
}

#[cfg(test)]
mod tests {
    use super::super::SliceInput;
    use super::*;

    fn dec(bytes: &[u8]) -> CarrylessRangeDecoder<SliceInput<'_>> {
        CarrylessRangeDecoder::new(bytes).unwrap()
    }

    #[test]
    fn init_reads_four_big_endian_bytes_and_accepts_any_code() {
        let d = dec(&[0x40, 0x00, 0x00, 0x01, 0xFF]);
        assert_eq!(
            d.state(),
            RangeCoderState {
                low: 0,
                code: 0x4000_0001,
                range: u32::MAX
            }
        );
        assert_eq!(d.position(), 4);
        assert!(CarrylessRangeDecoder::new(&[0xFF; 4][..]).is_ok());
        assert!(matches!(
            CarrylessRangeDecoder::new_7a(&[0xFF; 4][..]),
            Err(Error::CorruptStream { .. })
        ));
        for len in 0..4 {
            assert!(matches!(
                CarrylessRangeDecoder::new(&[0u8; 4][..len]),
                Err(Error::Truncated)
            ));
        }
    }

    /// `0x40000000 / (0xFFFFFFFF / 256) = 0x40000000 / 0x00FFFFFF = 64`.
    /// `Decode(64, 1)`: `low = 64 * 0x00FFFFFF = 0x3FFFFFC0`,
    /// `range = 0x00FFFFFF`; `low ^ (low + range) = 0x3FFFFFC0 ^ 0x40FFFFBF`
    /// `= 0x7F00007F >= TOP` and `range >= BOT`: no shift.
    #[test]
    fn threshold_and_decode_without_normalization() {
        let mut d = dec(&[0x40, 0x00, 0x00, 0x00, 0x55]);
        assert_eq!(d.get_threshold(256), 64);
        d.decode(64, 1);
        assert_eq!(
            d.state(),
            RangeCoderState {
                low: 0x3FFF_FFC0,
                code: 0x4000_0000,
                range: 0x00FF_FFFF
            }
        );
        assert_eq!(d.position(), 4);
    }

    /// The top byte unsettled, then settled: `low = 0x12FF_FF00`,
    /// `range = 0x200` crosses into `0x1300_0100`, so the truncation branch
    /// sets `range = -low & 0x7FFF = 0x100` and shifts once:
    /// `low = 0xFFFF_0000`, `range = 0x1_0000`. Then
    /// `low + range` wraps to 0, `0xFFFF_0000 ^ 0 >= TOP`, `range >= BOT`: stop.
    #[test]
    fn normalization_truncates_the_range_and_low_wraps() {
        let mut d = dec(&[0, 0, 0, 0, 0xAB, 0xCD]);
        d.low = 0x12FF_FF00;
        d.range = 0x200;
        d.normalize();
        assert_eq!(d.low, 0xFFFF_0000);
        assert_eq!(d.range, 0x1_0000);
        assert_eq!(d.code, 0xAB);
        assert_eq!(d.position(), 5);
    }

    /// With `low = 0` a range of 1 needs exactly three shifts to reach `TOP`.
    #[test]
    fn counts_the_zeros_it_is_fed_past_eof() {
        let mut d = dec(&[0u8; 4]);
        d.low = 0;
        d.range = 1;
        d.normalize();
        assert_eq!(d.range, TOP);
        assert_eq!(d.zero_bytes_past_eof(), 3);
        assert_eq!(d.position(), 4);
    }

    #[test]
    fn a_frequency_total_past_the_range_is_corrupt_not_a_division_by_zero() {
        let mut d = dec(&[0x40, 0, 0, 0, 0, 0, 0, 0]);
        d.range = 100;
        assert!(matches!(
            d.get_current_count(1000),
            Err(Error::CorruptStream { .. })
        ));
        assert!(d.faulted());
        // Sticky.
        assert!(d.get_current_count(1).is_err());
        assert!(d.decode_binary(1, 2).is_err());

        let mut d = dec(&[0u8; 8]);
        assert_eq!(d.get_threshold(0), u32::MAX);
        assert!(d.faulted());

        let mut d = dec(&[0u8; 8]);
        d.range = (1 << 14) - 1;
        let _ = d.decode_bit(1 << 13);
        assert!(d.faulted());
    }

    #[test]
    fn a_zero_symbol_size_faults_instead_of_spinning() {
        let mut d = dec(&[0u8; 8]);
        let _ = d.get_threshold(1 << 10);
        d.decode(0, 0);
        assert!(d.faulted());
        assert_ne!(d.state().range, 0);
    }

    /// `decode_bit` is exactly `get_threshold(1 << 14)` followed by one of
    /// the two `decode` calls: the shift and the division agree.
    #[test]
    fn decode_bit_matches_threshold_then_decode() {
        let input = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22];
        for size0 in [1u32, 100, 4096, 8192, 16000, 16383] {
            let mut bit = dec(&input);
            let mut long = dec(&input);
            let got = bit.decode_bit(size0);
            let want = u32::from(!long.decode_binary(size0, BIN_TOTAL).unwrap());
            assert_eq!(got, want);
            assert_eq!(bit.state(), long.state());
            assert_eq!(bit.position(), long.position());
        }
    }

    #[test]
    fn resuming_from_state_reads_no_init_bytes() {
        let input = [0x40, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78];
        let mut first = dec(&input[..4]);
        let _ = first.get_threshold(256);
        first.decode(1, 3);
        let state = first.state();
        let resumed = CarrylessRangeDecoder::from_state(&input[4..], state);
        assert_eq!(resumed.state(), state);
        assert_eq!(resumed.position(), 0);
    }

    /// Encoder normalization writes the settled top byte; `finish` writes
    /// the four bytes of `low`.
    #[test]
    fn encoder_shifts_out_the_top_byte_and_flushes_low() {
        let mut e = CarrylessRangeEncoder::new(Vec::new());
        // range = 0xFFFFFFFF / 256 = 0x00FFFFFF; low = 0x12 * that = 0x11FFFFEE;
        // low + range = 0x12FFFFED: top bytes 0x11 and 0x12 differ, no shift.
        e.encode(0x12, 1, 256);
        assert_eq!((e.low, e.range), (0x11FF_FFEE, 0x00FF_FFFF));
        assert!(e.out.is_empty());
        // total 0x10101: range = 0x00FFFFFF / 0x10101 = 0xFF exactly;
        // low + range = 0x120000ED: top byte unsettled, range < BOT, so
        // range = -low & 0x7FFF = 0x12 and 0x11 is shifted out. Then
        // low = 0xFFFFEE00, range = 0x1200: low + range wraps to 0 and
        // range < BOT, so range = -low & 0x7FFF = 0x1200 and 0xFF is shifted
        // out. Then low = 0xFFEE0000, range = 0x120000 >= BOT: stop.
        e.encode(0, 1, 0x10101);
        assert_eq!(e.out, [0x11, 0xFF]);
        assert_eq!((e.low, e.range), (0xFFEE_0000, 0x12_0000));
        let out = e.finish().unwrap();
        assert_eq!(out, [0x11, 0xFF, 0xFF, 0xEE, 0x00, 0x00]);
    }

    #[test]
    fn encoder_faults_are_reported_by_finish() {
        let mut e = CarrylessRangeEncoder::new(Vec::new());
        e.range = 100;
        e.encode(0, 1, 1000);
        assert!(e.faulted());
        assert!(matches!(e.finish(), Err(Error::CorruptStream { .. })));
        let mut e = CarrylessRangeEncoder::new(Vec::new());
        e.encode_bit(0, 0);
        assert!(e.faulted());
    }
}
