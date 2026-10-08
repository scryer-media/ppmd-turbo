//! The 7z range coder: Igor Pavlov's LZMA-style coder with carry
//! propagation, as 7-Zip pairs it with PPMd variant H for the 7z `PPMD`
//! method (`Ppmd7z_RangeDec_*` in `C/Ppmd7Dec.c`, `Ppmd7z_RangeEnc_*` in
//! `C/Ppmd7Enc.c`; public domain).
//!
//! Every register is a `u32` (the encoder's `low` is a `u64` to hold the
//! carry) and every operation is the reference's integer arithmetic,
//! wrapping where C's unsigned arithmetic wraps.
//!
//! **Normalization schedule.** 7-Zip does not normalize "while
//! `range < kTopValue`". It normalizes with exactly two conditional steps
//! (`RC_NORM`) after a symbol found in a multi-symbol context or by SEE, and
//! at the top of the escape loop (`RC_NORM_REMOTE`) after an escape or a
//! binary miss; and with exactly one step (`RC_NORM_1`) after a binary hit.
//! Here each operation applies its step count before returning. The escape
//! loop's remote normalization comes before anything else in the loop,
//! including the end-of-stream return, so applying it eagerly reads the same
//! bytes at the same points. A "while" loop agrees only while two steps
//! always suffice: with a total above 2^16 two steps can leave the range
//! below 2^24, and the reference keeps going with that range. Copying the
//! step counts makes the coder exact whatever totals the model hands it.

use super::{BIN_TOTAL_BITS, IntoRangeInput, RangeDecoder, RangeEncoder, RangeInput, RangeOutput};
use super::{TOP, corrupt};
use crate::error::{Error, Result};

/// The 7z range decoder (`CPpmd7_RangeDec` with the `Ppmd7z_` operations).
///
/// Reads through any [`RangeInput`]. Past the end of the input it is fed
/// zeros and counts them ([`zero_bytes_past_eof`](Self::zero_bytes_past_eof));
/// 7-Zip raises its `Extra` flag in the same situation, and the 7z framing
/// treats any such byte as an error.
#[derive(Debug)]
pub struct SevenZipRangeDecoder<I: RangeInput> {
    range: u32,
    code: u32,
    faulted: bool,
    input: I,
}

impl<I: RangeInput> SevenZipRangeDecoder<I> {
    /// `Ppmd7z_RangeDec_Init`: reads five bytes, the first of which must be
    /// zero, the other four big-endian into `code`, with `range = 0xFFFFFFFF`.
    ///
    /// Errors: [`Error::Truncated`] if the input holds fewer than five bytes;
    /// [`Error::CorruptStream`] if the first byte is not zero or `code` is
    /// `0xFFFFFFFF` (the reference's `Code < 0xFFFFFFFF` check).
    pub fn new<T: IntoRangeInput<Input = I>>(input: T) -> Result<Self> {
        let mut input = input.into_range_input();
        let first = input.next_byte();
        let mut code = 0u32;
        for _ in 0..4 {
            code = (code << 8) | u32::from(input.next_byte());
        }
        if input.zero_bytes_past_eof() != 0 {
            return Err(input.take_io_error().map_or(Error::Truncated, Error::Io));
        }
        if first != 0 {
            return Err(corrupt("7z range coder: first byte is not zero"));
        }
        if code == u32::MAX {
            return Err(corrupt("7z range coder: initial code is 0xFFFFFFFF"));
        }
        Ok(Self {
            range: u32::MAX,
            code,
            faulted: false,
            input,
        })
    }

    /// `Ppmd7z_RangeDec_IsFinishedOK`: `code == 0`, which a stream encoded
    /// by 7-Zip's coder satisfies once its last symbol is decoded.
    #[inline]
    pub fn is_finished_ok(&self) -> bool {
        self.code == 0
    }

    /// The `range` register.
    #[inline]
    pub fn range(&self) -> u32 {
        self.range
    }

    /// The `code` register.
    #[inline]
    pub fn code(&self) -> u32 {
        self.code
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

    /// The input, mutably (for example to take a [`ReadInput`](super::ReadInput)'s
    /// I/O error).
    pub fn input_mut(&mut self) -> &mut I {
        &mut self.input
    }

    /// Unwraps the input.
    pub fn into_input(self) -> I {
        self.input
    }

    /// One `RC_NORM_BASE` step's body: shift a byte into `code`.
    #[inline(always)]
    fn shift_in(&mut self) {
        self.code = (self.code << 8) | u32::from(self.input.next_byte());
        self.range <<= 8;
    }

    /// `RC_NORM_1`: one conditional step.
    #[inline(always)]
    fn norm_1(&mut self) {
        if self.range < TOP {
            self.shift_in();
        }
    }

    /// `RC_NORM` (and `RC_NORM_REMOTE`): two conditional steps. A zero range
    /// (a symbol size of zero) is a fault; the check sits on the branch that
    /// already knows the range is small.
    #[inline(always)]
    fn norm(&mut self) {
        if self.range < TOP {
            if self.range == 0 {
                self.fault();
            }
            self.shift_in();
            if self.range < TOP {
                self.shift_in();
            }
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

impl<I: RangeInput> RangeDecoder for SevenZipRangeDecoder<I> {
    /// `RC_GetThreshold(total)`: `Code / (Range /= total)`.
    #[inline(always)]
    fn get_threshold(&mut self, total: u32) -> u32 {
        self.range = self.range.checked_div(total).unwrap_or(0);
        if self.range == 0 {
            return self.fault();
        }
        self.code / self.range
    }

    /// `Ppmd7z_RD_Decode` (`Code -= start * Range; Range *= size`) followed by
    /// `RC_NORM`.
    #[inline(always)]
    fn decode(&mut self, start: u32, size: u32) {
        self.code = self.code.wrapping_sub(start.wrapping_mul(self.range));
        self.range = self.range.wrapping_mul(size);
        self.norm();
    }

    /// The binary-context path of `Ppmd7z_DecodeSymbol`:
    /// `size0 = (Range >> 14) * prob`; a hit sets `Range = size0` and applies
    /// `RC_NORM_1`, a miss sets `Code -= size0; Range -= size0` and applies
    /// the escape loop's `RC_NORM`.
    #[inline(always)]
    fn decode_bit(&mut self, size0: u32) -> u32 {
        let bound = (self.range >> BIN_TOTAL_BITS).wrapping_mul(size0);
        if self.code < bound {
            self.range = bound;
            self.norm_1();
            0
        } else {
            self.code = self.code.wrapping_sub(bound);
            self.range = self.range.wrapping_sub(bound);
            self.norm();
            1
        }
    }

    #[inline(always)]
    fn faulted(&self) -> bool {
        self.faulted
    }
}

/// The 7z range encoder (`CPpmd7z_RangeEnc` with the `Ppmd7z_RangeEnc_`
/// operations).
///
/// Writes through any [`RangeOutput`]; [`finish`](Self::finish) writes the
/// five flush bytes (`Ppmd7z_Flush_RangeEnc`) and flushes the output.
#[derive(Debug)]
pub struct SevenZipRangeEncoder<O: RangeOutput> {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    faulted: bool,
    out: O,
}

impl<O: RangeOutput> SevenZipRangeEncoder<O> {
    /// `Ppmd7z_Init_RangeEnc`: `Low = 0`, `Range = 0xFFFFFFFF`, `Cache = 0`,
    /// `CacheSize = 1`. The pending cache byte is the stream's leading zero.
    pub fn new(out: O) -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            faulted: false,
            out,
        }
    }

    /// The output.
    pub fn output(&self) -> &O {
        &self.out
    }

    /// `Ppmd7z_Flush_RangeEnc` (five `ShiftLow` calls), then flushes the
    /// output and returns it.
    ///
    /// Errors: the output's error, or [`Error::CorruptStream`] if the coder
    /// faulted (see [`RangeEncoder::faulted`]).
    pub fn finish(mut self) -> Result<O> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out.finish()?;
        if self.faulted {
            return Err(corrupt("7z range encoder: frequency total past the range"));
        }
        Ok(self.out)
    }

    /// `Ppmd7z_RangeEnc_ShiftLow`. When the top byte of `low` is settled
    /// (below `0xFF000000`) or a carry is pending (`low >> 32`), it writes
    /// the cached byte plus the carry and `CacheSize - 1` bytes of
    /// `0xFF + carry`, then caches the new top byte; otherwise it only
    /// counts one more pending `0xFF`. Then `low = (u32)low << 8`.
    #[inline]
    fn shift_low(&mut self) {
        let low32 = self.low as u32;
        if low32 < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.write_byte(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (low32 >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = u64::from(low32 << 8);
    }

    /// `RC_NORM_1` for the encoder: one conditional step.
    #[inline(always)]
    fn norm_1(&mut self) {
        if self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }

    /// `RC_NORM` for the encoder: two conditional steps.
    #[inline(always)]
    fn norm(&mut self) {
        if self.range < TOP {
            self.range <<= 8;
            self.shift_low();
            if self.range < TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn fault(&mut self) {
        self.faulted = true;
    }
}

impl<O: RangeOutput> RangeEncoder for SevenZipRangeEncoder<O> {
    /// `R->Range /= total` (done by the model in 7-Zip), then
    /// `Ppmd7z_RangeEnc_Encode` (`Low += start * Range; Range *= size`, the
    /// product in `u32`) and `RC_NORM`.
    #[inline(always)]
    fn encode(&mut self, start: u32, size: u32, total: u32) {
        let range = self.range.checked_div(total).unwrap_or(0);
        let range_out = range.wrapping_mul(size);
        if range_out == 0 {
            // Either the total is past the range or the size is zero: the
            // reference would divide by zero or emit an undecodable stream.
            return self.fault();
        }
        self.low = self.low.wrapping_add(u64::from(start.wrapping_mul(range)));
        self.range = range_out;
        self.norm();
    }

    /// The binary-context path of `Ppmd7z_EncodeSymbol`:
    /// `bound = (Range >> 14) * prob`; bit 0 sets `Range = bound` and applies
    /// `RC_NORM_1`; bit 1 sets `Low += bound; Range -= bound` and applies the
    /// escape loop's `RC_NORM`.
    #[inline(always)]
    fn encode_bit(&mut self, size0: u32, bit: u32) {
        let bound = (self.range >> BIN_TOTAL_BITS).wrapping_mul(size0);
        if bit == 0 {
            if bound == 0 {
                return self.fault();
            }
            self.range = bound;
            self.norm_1();
        } else {
            let range = self.range.wrapping_sub(bound);
            if range == 0 || bound > self.range {
                return self.fault();
            }
            self.low = self.low.wrapping_add(u64::from(bound));
            self.range = range;
            self.norm();
        }
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

    fn dec(bytes: &[u8]) -> SevenZipRangeDecoder<SliceInput<'_>> {
        SevenZipRangeDecoder::new(bytes).unwrap()
    }

    #[test]
    fn init_reads_a_zero_then_four_big_endian_bytes() {
        let d = dec(&[0x00, 0x12, 0x34, 0x56, 0x78, 0x9A]);
        assert_eq!((d.code, d.range), (0x1234_5678, 0xFFFF_FFFF));
        assert_eq!(d.position(), 5);
        assert!(!d.is_finished_ok());
    }

    #[test]
    fn init_rejects_a_nonzero_first_byte_an_all_ones_code_and_short_input() {
        let r = SevenZipRangeDecoder::new(&[0x01, 0, 0, 0, 0][..]);
        assert!(matches!(r, Err(Error::CorruptStream { .. })));
        let r = SevenZipRangeDecoder::new(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF][..]);
        assert!(matches!(r, Err(Error::CorruptStream { .. })));
        // 0xFFFFFFFE is the largest code the reference accepts.
        assert!(SevenZipRangeDecoder::new(&[0x00, 0xFF, 0xFF, 0xFF, 0xFE][..]).is_ok());
        for len in 0..5 {
            let r = SevenZipRangeDecoder::new(&[0u8; 5][..len]);
            assert!(matches!(r, Err(Error::Truncated)), "len {len}");
        }
    }

    /// Hand-computed from the C formulas:
    /// `Range = 0xFFFFFFFF / 256 = 0x00FFFFFF`,
    /// `GetThreshold = 0x12345678 / 0x00FFFFFF = 0x12` (remainder `0x34568A`),
    /// `Decode(0x12, 1)`: `Code = 0x34568A`, `Range = 0x00FFFFFF`; `RC_NORM`
    /// shifts once (`0x00FFFFFF < 2^24`) to `Code = 0x34568A9A`,
    /// `Range = 0xFFFFFF00`, and stops.
    #[test]
    fn threshold_decode_and_one_normalization_step() {
        let mut d = dec(&[0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        assert_eq!(d.get_threshold(256), 0x12);
        assert_eq!(d.range, 0x00FF_FFFF);
        d.decode(0x12, 1);
        assert_eq!((d.code, d.range), (0x3456_8A9A, 0xFFFF_FF00));
        assert_eq!(d.position(), 6);
    }

    #[test]
    fn a_range_of_exactly_k_top_value_is_not_normalized() {
        let mut d = dec(&[0x00, 0x00, 0x00, 0x00, 0x05, 0xAA, 0xBB]);
        d.range = 1 << 24;
        d.decode(0, 1);
        assert_eq!((d.code, d.range), (5, 1 << 24));
        assert_eq!(d.position(), 5);
        // One below shifts exactly once.
        d.range = (1 << 24) - 1;
        d.decode(0, 1);
        assert_eq!((d.code, d.range), (0x5AA, 0xFFFF_FF00));
        assert_eq!(d.position(), 6);
    }

    /// `RC_NORM` is two conditional steps, never a third: a range of 1 after
    /// a decode ends at 2^16, below `kTopValue`, exactly as in the reference.
    #[test]
    fn rc_norm_stops_after_two_steps() {
        let mut d = dec(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33]);
        d.range = 1;
        d.decode(0, 1);
        assert_eq!((d.code, d.range), (0x1122, 1 << 16));
        assert_eq!(d.position(), 7);
    }

    /// A binary hit normalizes once (`RC_NORM_1`), a miss twice.
    #[test]
    fn decode_bit_hit_and_miss_follow_the_reference_schedule() {
        // code = 0, range = 2^24: bound = (2^24 >> 14) * 100 = 102400.
        let mut d = dec(&[0x00, 0x00, 0x00, 0x00, 0x00, 0xA1, 0xA2, 0xA3]);
        d.range = 1 << 24;
        assert_eq!(d.decode_bit(100), 0);
        // range 102400 < 2^24: one step only, to 0x01900000.
        assert_eq!((d.code, d.range), (0xA1, 102_400 << 8));
        assert_eq!(d.position(), 6);

        // Miss: code = 0x01000000, range = 0x01000000, bound = 1024 * 16383.
        let mut d = dec(&[0x00, 0x01, 0x00, 0x00, 0x00, 0xB1, 0xB2]);
        d.range = 1 << 24;
        assert_eq!(d.decode_bit(16383), 1);
        let bound = 1024 * 16383;
        // range = 2^24 - bound = 1024, two steps -> 1024 << 16.
        assert_eq!(d.range, 1024 << 16);
        assert_eq!(d.code, (((1u32 << 24) - bound) << 16) | 0xB1B2);
        assert_eq!(d.position(), 7);
    }

    #[test]
    fn zero_totals_and_zero_sizes_fault_instead_of_dividing_by_zero() {
        let mut d = dec(&[0u8; 16]);
        assert_eq!(d.get_threshold(0), u32::MAX);
        assert!(d.faulted());

        let mut d = dec(&[0u8; 16]);
        d.range = 100;
        assert_eq!(d.get_threshold(1000), u32::MAX);
        assert!(d.faulted());

        let mut d = dec(&[0u8; 16]);
        let _ = d.get_threshold(10);
        d.decode(0, 0);
        assert!(d.faulted());
        assert_ne!(d.range, 0);
    }

    /// `ShiftLow` with a run of pending `0xFF` bytes, with and without a
    /// carry. Hand-computed from `Ppmd7Enc.c:24-42`.
    #[test]
    fn shift_low_propagates_a_carry_through_pending_ff_bytes() {
        for (carry, want) in [(false, [0x00, 0xFF, 0xFF]), (true, [0x01, 0x00, 0x00])] {
            let mut e = SevenZipRangeEncoder::new(Vec::new());
            // Two lows with top byte 0xFF and no carry: nothing is written,
            // CacheSize counts them.
            e.low = 0xFF00_0005;
            e.shift_low();
            assert_eq!((e.cache, e.cache_size, e.low), (0, 2, 0x0500));
            e.low = 0xFF12_3456;
            e.shift_low();
            assert_eq!((e.cache, e.cache_size, e.low), (0, 3, 0x1234_5600));
            assert!(e.out.is_empty());
            // Now a settled low, with or without a carry out of bit 32.
            e.low = 0x00AB_CDEF | if carry { 1 << 32 } else { 0 };
            e.shift_low();
            assert_eq!(e.out, want);
            assert_eq!((e.cache, e.cache_size, e.low), (0x00, 1, 0xABCD_EF00));
        }
    }

    #[test]
    fn an_empty_stream_flushes_to_the_leading_zero_and_four_zeros() {
        let e = SevenZipRangeEncoder::new(Vec::new());
        let out = e.finish().unwrap();
        assert_eq!(out, [0, 0, 0, 0, 0]);
        let d = dec(&out);
        assert!(d.is_finished_ok());
    }

    #[test]
    fn encoder_faults_are_reported_by_finish() {
        let mut e = SevenZipRangeEncoder::new(Vec::new());
        e.encode(0, 1, 0);
        assert!(e.faulted());
        assert!(matches!(e.finish(), Err(Error::CorruptStream { .. })));

        let mut e = SevenZipRangeEncoder::new(Vec::new());
        e.encode(0, 0, 10);
        assert!(e.faulted());

        let mut e = SevenZipRangeEncoder::new(Vec::new());
        e.encode_bit(0, 0);
        assert!(e.faulted());
    }
}
