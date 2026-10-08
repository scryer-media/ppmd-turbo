# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.1.0 - 2026-10-08

- Initial crate skeleton.
- PPMd variant H model, sub-allocator and SEE (`model::Model`), extracted
  from unrar-rs with its optimisations intact. The arena is allocated once
  and never grows.
- `rc::RangeDecoder`, the seam between the model and the range coders, and
  the RAR carry-less decoder `rc::RarRangeDecoder` with resumable registers
  and any `rc::ByteSource` as input.
- RAR 2.9-4.x PPMd decoding (`rar::RarDecoder`): the model persists across
  blocks and solid members, a same-sized arena is reused on reset, and
  unrar's CleanUp is available. Output is bit-exact with unrar.
- Corrupt input is reported as an error, never a panic or a hang, including
  a range scaled to zero, which the reference divides by.
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
- `ppmd-corpus` generates the conformance and bench corpora: seeded payloads,
  7z PPMd streams from ppmd-rust and 7-Zip, and RAR PPMd members, each
  recorded in a manifest with its parameters and digests.
- Conformance fixtures under `tests/fixtures`, and 7z and RAR conformance
  suites over them; the decoder tests wait on the decoders.
- `ppmd-bench`, a one-operation-per-process driver for 7z decode, RAR decode
  and 7z encode that reports its own CPU time, peak heap and peak RSS.
- The `ppmd-turbo-bench` Go harness with quick, full and fleet profiles,
  against 7-Zip, ppmd-rust and RARLAB's unrar, and a quick baseline on an
  Apple M5 Max.
