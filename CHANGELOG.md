# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.2.0 - 2026-10-09

- PPMd variant H encoding. The encoder drives the model through the same
  update, SEE, rescale and restart code the decoder uses, and codes bytes
  and an optional end marker over either range coder. Derived from 7-Zip's
  `Ppmd7z_EncodeSymbol` (Igor Pavlov) and Dmitry Shkarin's variant H
  encoder.
- 7z `PPMD` stream encoding, streamed to any writer or into memory,
  byte-identical to 7-Zip's for the same order and memory size. Checked
  against ppmd-rust 1.5.0 across orders 2-64 and arenas from 2 KiB through
  repeated arena restarts, and against 7-Zip 26.01: identical streams at
  every order and arena 7-Zip's encoder accepts, and 7-Zip extracts the
  streams outside that window. Flushing mid-stream hands over settled bytes
  without ending the stream.
- Raw carry-less stream encoding (Dmitry Subbotin's coder, no RAR framing),
  byte-identical to ppmd-rust's `7a` encoder, so the carry-less and RAR
  decoders can be round-trip tested. For correctness only; it is never
  benchmarked, and writing RAR blocks or archives is out of scope.
- Both range encoders give mutable access to their output, so settled bytes
  can be flushed without finishing the coder.
- Encoder test suites and an opt-in `7zz` oracle; the `roundtrip_7z` and
  `roundtrip_carryless` fuzz targets run the crate's encoders.
- Encode: the escape pass sums the unmasked states and finds the symbol in
  one walk with no branch per state, 1.47x faster on binary input, 1.33x
  on mixed and 1.11x on 64 KiB of text.

## 0.1.0 - 2026-10-08

- PPMd variant H model, sub-allocator and SEE, seeded from the PPMd module
  of unrar-rs with its optimisations intact. The arena is allocated once
  and never grows; a restart at the same size reuses it.
- Range-coder seams between the model and the coders, statically
  dispatched, so the model decodes over either coder.
- The 7z range coder (Igor Pavlov's `Ppmd7z_RangeDec` / `Ppmd7z_RangeEnc`),
  decoder and encoder, bit-exact including the reference's normalization
  step counts and `ShiftLow` carry propagation.
- The carry-less range coder (Dmitry Subbotin's, as RAR and Shkarin's
  `.pmd` use it), decoder and encoder, with RAR-style operations, 7-Zip's
  `7a` initialization check, and registers that can be saved, built
  directly and restored across RAR solid members without re-reading the
  initialization bytes.
- Coder input from a borrowed slice, an owned 64 KiB refill buffer over
  `std::io::Read`, or a window over a byte source shared with another
  reader. Zeros are fed past the end of the input and counted. Coder output
  goes to a `Vec`, a fixed slice, or a buffered `std::io::Write`.
- RAR 2.9-4.x PPMd decoding, bit-exact with unrar: the model persists across
  blocks and solid members, a same-sized arena is reused on reset, and
  unrar's CleanUp is available for its recovery path. An end marker reached
  after the coder ran past its input is a truncation, not the end of the
  data.
- 7z `PPMD` stream decoding, byte-identical to 7-Zip's, streamed from any
  reader or from memory, with an optional known unpacked size, end-marker
  handling and an opt-in FinishStream check, as 7-Zip's `PpmdDecoder.cpp`.
- Corrupt input is an error, never a panic, a hang, a division by zero or an
  endless normalization loop. That includes a frequency total past the
  range, a zero total and a zero symbol size, where the reference divides
  by zero.
- Errors convert to `std::io::Error`: corrupt streams are `InvalidData`,
  truncation is `UnexpectedEof`, bad parameters are `InvalidInput`, and I/O
  errors pass through. Coder initialisation reports the reader's own error
  instead of a truncation.
- `ppmd-corpus` generates the conformance and bench corpora: seeded
  payloads, 7z PPMd streams from ppmd-rust and 7-Zip, and RAR PPMd members,
  each recorded in a manifest with its parameters and digests.
- Conformance fixtures under `tests/fixtures`, with 7z and RAR conformance
  suites over them.
- Hostile-input tests (truncation, bit flips, parameter range, restart
  storms, size lies, end-marker cases, a heap bound) with generated
  fixtures.
- Seven cargo-fuzz targets with bounded memory: `decode_7z`, `decode_rar`,
  `decode_differential_7z`, `roundtrip_7z`, `roundtrip_carryless` and
  `structure_7z` with committed seed corpora, and `range_coders` for the
  coders on their own.
- `tools/ppmd-oracle`: a minimal PPMd `.7z` writer and reader and
  out-of-process checks against `7zz` and `unrar`.
- `ppmd-bench`, a one-operation-per-process driver for 7z decode, RAR
  decode and 7z encode that reports its own CPU time, peak heap and peak
  RSS; and the `ppmd-turbo-bench` Go harness with quick, full and fleet
  profiles against 7-Zip, ppmd-rust and RARLAB's unrar, with a quick
  baseline on an Apple M5 Max.
- The fuzz harness, the hostile-input and conformance suites, the bench
  driver and the oracle tool run against both decoders.
- CI: a deterministic fuzz run of the six seeded targets, the hostile tests
  under AddressSanitizer, Miri, and a dispatch-only `fuzz-extended`
  workflow.
- Decode: the escape decode gathers the unmasked states without a branch
  per state, 1.2-1.35x faster on binary and mixed input (7z binary order 6:
  1293M to 911M cycles) and unchanged on text.
- Decode: the escape decode sums the unmasked frequencies in a first pass
  and walks the states again only to select or mask, 4-9% fewer cycles on
  binary and mixed input and unchanged on text.
- Decode: the escape decode's selection counts down from the threshold
  and reuses the gather's borrows, 2-4% fewer cycles.
