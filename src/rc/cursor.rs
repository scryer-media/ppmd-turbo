//! Where the range decoders read their bytes from: a cursor over the
//! caller's input slice.
//!
//! The codecs never own input. Each step call lends its slice, a decoder
//! is rebuilt over it from the registers it saved, and the cursor's
//! position afterwards is exactly the `consumed` count the call reports.
//!
//! The per-byte path is `data.get(pos)`: one predictable bounds comparison
//! that doubles as the end-of-input test, so the hot path needs no `unsafe`.
//! The batched loop (`engine::run`) only decodes a symbol while at least
//! the coder's per-symbol bound of input remains, so in that phase the
//! comparison never fails. Past the end, which only the last input's edge
//! phase reaches, the cursor returns zeros and counts them: what RARLAB
//! unrar does, and what 7-Zip's byte reader does as it raises its `Extra`
//! flag. The framing decides whether that padding is legal (RAR tolerates
//! a little, 7z none).

/// The byte stream a range decoder reads, one byte per normalization step.
///
/// One implementation ([`SliceInput`]); the trait exists so the coders and
/// the model stay generic, which costs nothing after monomorphization.
pub trait RangeInput {
    /// Takes the next byte, or 0 once the input has ended (counted by
    /// [`zero_bytes_past_eof`](Self::zero_bytes_past_eof)).
    fn next_byte(&mut self) -> u8;

    /// Bytes taken from the input so far, not counting the zeros fed past
    /// its end.
    fn position(&self) -> usize;

    /// Zero bytes fed past the end of the input so far.
    fn zero_bytes_past_eof(&self) -> u32;
}

/// Converts a value into the [`RangeInput`] a decoder reads from.
pub trait IntoRangeInput {
    /// The input this value becomes.
    type Input: RangeInput;

    /// Performs the conversion.
    fn into_range_input(self) -> Self::Input;
}

/// A cursor over a borrowed slice.
#[derive(Clone, Debug)]
pub struct SliceInput<'a> {
    data: &'a [u8],
    pos: usize,
    zero_bytes_past_eof: u32,
}

impl<'a> SliceInput<'a> {
    /// Reads `data` from its first byte.
    #[inline]
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

    /// The whole slice's length.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the slice is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[cold]
    #[inline(never)]
    fn past_eof(&mut self) -> u8 {
        self.zero_bytes_past_eof = self.zero_bytes_past_eof.saturating_add(1);
        0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_is_read_then_padded_with_counted_zeros() {
        let mut input = SliceInput::new(&[7, 8]);
        assert_eq!((input.next_byte(), input.next_byte()), (7, 8));
        assert_eq!((input.next_byte(), input.next_byte()), (0, 0));
        assert_eq!((input.position(), input.zero_bytes_past_eof()), (2, 2));
        assert!(input.remaining().is_empty());
    }
}
