# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.1.0 - 2026-10-08

- Initial crate skeleton.
- The 7z range coder (Igor Pavlov's, `Ppmd7z_RangeDec` / `Ppmd7z_RangeEnc`):
  `SevenZipRangeDecoder` and `SevenZipRangeEncoder`, bit-exact including the
  reference's normalization step counts and `ShiftLow` carry propagation.
- The carry-less range coder (Dmitry Subbotin's, as RAR and Shkarin's `.pmd`
  use it): `CarrylessRangeDecoder` (also `RarRangeDecoder`) and
  `CarrylessRangeEncoder`, with `RangeCoderState` for resuming across RAR
  solid members.
- Coder input from a borrowed slice, an owned 64 KiB refill buffer over
  `std::io::Read`, or a window over a shared `ByteSource`. Zeros are fed past
  the end of the input and counted. Coder output goes to a `Vec`, a fixed
  slice, or a buffered `std::io::Write`.
- A frequency total past the range, a zero total or a zero symbol size is
  reported as a corrupt stream. It is never a division by zero or an endless
  normalization loop.
- `range_coders` fuzz target.
