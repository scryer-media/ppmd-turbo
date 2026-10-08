# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.2.0 - 2026-10-09

- PPMd variant H encoding: `Model::encode_symbol` codes a byte or the end
  marker through any `rc::RangeEncoder`, driving the model through the same
  update, SEE and rescale code the decoder uses. Derived from 7-Zip's
  `Ppmd7z_EncodeSymbol` and Dmitry Shkarin's variant H encoder.
- The 7z encoder: `Ppmd7Encoder` (over any `std::io::Write`, with
  `finish(with_end_marker)`) and `encode_7z` for a slice, next to the
  decoder in `ppmd7`. Output is byte-identical to 7-Zip's, checked against
  7-Zip 26.01 and ppmd-rust 1.5.0 across orders 2-64 and arenas from 2 KiB,
  through repeated arena restarts.
- `carryless::CarrylessEncoder` and `encode_carryless`: raw carry-less
  streams (no RAR framing) for round-trip testing of the carry-less and RAR
  decoders. A correctness tool, not a tuned encoder.
- `output_mut` on `SevenZipRangeEncoder` and `CarrylessRangeEncoder`.
- Encoder test suites (`tests/encode_7z.rs`, `tests/encode_carryless.rs`)
  and an opt-in `7zz` oracle (`tests/encode_oracle_7zz.rs`); the
  `roundtrip_7z` and `roundtrip_carryless` fuzz targets now run the crate's
  encoders.
- Encode: the escape pass sums the unmasked states and finds the symbol in
  one walk with no branch per state and no state waiting on the one before,
  1.56x faster on binary input, 1.40x on mixed and 1.11x on 64 KiB of
  text.

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
  suites over them.
- `ppmd-bench`, a one-operation-per-process driver for 7z decode, RAR decode
  and 7z encode that reports its own CPU time, peak heap and peak RSS.
- The `ppmd-turbo-bench` Go harness with quick, full and fleet profiles,
  against 7-Zip, ppmd-rust and RARLAB's unrar, and a quick baseline on an
  Apple M5 Max.
- Hostile-input tests (truncation, bit flips, parameter range, restart
  storms, size lies, end-marker cases, a heap bound) with generated fixtures.
- Six cargo-fuzz targets with committed seed corpora and bounded memory:
  `decode_7z`, `decode_rar`, `decode_differential_7z`, `roundtrip_7z`,
  `roundtrip_carryless` and `structure_7z`.
- `tools/ppmd-oracle`: a minimal PPMd `.7z` writer and reader and
  out-of-process checks against `7zz` and `unrar`.
- 7z `PPMD` decoding: `Ppmd7Decoder<R: Read>` (with an optional known
  unpacked size, end-marker handling and an opt-in FinishStream check, as
  7-Zip's `PpmdDecoder.cpp`) and `decode_7z` for a slice. Output is
  byte-identical to 7-Zip's.
- `From<Error> for std::io::Error`: corrupt streams are `InvalidData`,
  truncation is `UnexpectedEof`, bad parameters are `InvalidInput`, and I/O
  errors pass through. Coder initialisation reports the reader's own error
  instead of `Truncated`.
- `RangeCoderState::new`, to build carry-less coder registers directly.
- A RAR PPMd end marker reached after the coder ran past its input is
  `Error::Truncated`, not the end of the data.
- The fuzz harness, the hostile-input and conformance suites, `ppmd-bench
  --impl ppmd-turbo` and `ppmd-oracle --codec turbo` run against both
  decoders.
- CI: a deterministic fuzz run of every target, the hostile tests under
  AddressSanitizer, and a dispatch-only `fuzz-extended` workflow.
- Decode: the escape decode gathers the unmasked states without a branch
  per state, 1.2-1.35x faster on binary and mixed input (7z binary order 6:
  1293M to 911M cycles) and unchanged on text.
- Decode: the escape decode sums the unmasked frequencies in a first pass
  and walks the states again only to select or mask, 4-9% fewer cycles on
  binary and mixed input and unchanged on text.
- Decode: the escape decode's selection counts down from the threshold
  and reuses the gather's borrows, 2-4% fewer cycles.
