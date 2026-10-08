# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.1.0 - 2026-10-09

- PPMd variant H model, sub-allocator and SEE, seeded from the PPMd module
  of unrar-rs with its optimisations intact. The model drives decoding and
  encoding through the same update, SEE, rescale and restart code. The
  encoder is derived from 7-Zip's `Ppmd7z_EncodeSymbol` (Igor Pavlov) and
  Dmitry Shkarin's variant H encoder.
- A step API that never retains caller bytes. Every call takes an input
  slice and an output slice and returns `Progress { consumed, produced,
  status }`; the bytes after `consumed` belong to the caller. A call that
  neither consumes nor produces always says why (`NeedInput` short of one
  symbol's input, `OutputFull`, or a stop), so a caller loop never spins.
  `MAX_INPUT_PER_SYMBOL` (264 bytes) is the most input any decoder needs
  for one symbol.
- `Params`: the order and arena size, validated once, with the 7z property
  bytes, RAR block-header sizes, 7-Zip's `ReduceSize`, and
  `memory_footprint`, the exact heap a codec built from them allocates.
- `Arena`: model memory allocated fallibly and handed from one codec to the
  next with `into_arena` and `with_arena`. An arena is reused when it covers
  the need and is at most twice it.
- `Error { kind, at }`: `Copy`, with the input and output position the
  error was found at, and sticky (a codec repeats it until reset).
  `ErrorKind` is `Corrupt`, `Truncated`, `InvalidParameters`,
  `AllocationFailed` or `MemoryLimit`, and converts to `std::io::Error`.
  Error text avoids the words archive tooling classifies by.
- 7z `PPMD` streams: `SevenZDecoder` (optional known size, end-marker
  handling and an opt-in FinishStream check, as 7-Zip's `PpmdDecoder.cpp`)
  and `SevenZEncoder`, byte-identical to 7-Zip's for the same order and
  memory size. Checked against ppmd-rust 1.5.0 across orders 2-64 and arenas
  from 2 KiB through repeated arena restarts, and against 7-Zip 26.01:
  identical streams at every order and arena 7-Zip's encoder accepts.
- RAR 2.9-4.x PPMd: `RarPpmd`, bit-exact with unrar. The model persists
  across blocks and solid members, `decode` runs literals to the escape
  character and `next_symbol` reads the command bytes after it, a caller
  arena limit and padding allowance bound hostile headers, and unrar's
  CleanUp is available for its recovery path. An end marker reached after
  the coder ran past its input is a truncation, not the end of the data.
  A failed block poisons the decoder until the next model reset.
- Raw carry-less streams (Dmitry Subbotin's coder, no RAR framing):
  `CarrylessDecoder` and `CarrylessEncoder`, byte-identical to ppmd-rust's
  `7a` coder, so the carry-less and RAR paths can be round-trip tested.
  Writing RAR blocks or archives is out of scope.
- `io::SevenZReader` over any `BufRead` and `io::SevenZWriter` over any
  `Write` (feature `std`), which consume exactly the stream's bytes.
- `no_std` with `alloc` when the default `std` feature is off. The
  `internals` feature (hidden from the docs) exposes the model and range
  coders for the crate's own tests, fuzzers and tools.
- The 7z range coder (Igor Pavlov's `Ppmd7z_RangeDec` / `Ppmd7z_RangeEnc`)
  and the carry-less range coder (as RAR and Shkarin's `.pmd` use it),
  bit-exact including the reference's normalization step counts and
  `ShiftLow` carry propagation.
- Corrupt input is an error, never a panic, a hang, a division by zero or an
  endless normalization loop. That includes a frequency total past the
  range, a zero total and a zero symbol size, where the reference divides
  by zero.
- Decode: the escape decode gathers the unmasked states without a branch
  per state and sums their frequencies in a first pass, 1.2-1.35x faster on
  binary and mixed input and unchanged on text; its selection counts down
  from the threshold and reuses the gather's borrows.
- Encode: the escape pass sums the unmasked states and finds the symbol in
  one walk with no branch per state, 1.56x faster on binary input, 1.40x on
  mixed and 1.11x on 64 KiB of text.
- `ppmd-corpus` generates the conformance and bench corpora: seeded
  payloads, 7z PPMd streams from ppmd-rust and 7-Zip, and RAR PPMd members,
  each recorded in a manifest with its parameters and digests.
- Conformance fixtures under `tests/fixtures`, with 7z and RAR conformance
  suites over them; encoder suites and an opt-in `7zz` oracle.
- Hostile-input tests (truncation, bit flips, parameter range, restart
  storms, size lies, end-marker cases, an exact heap footprint) with
  generated fixtures. Every suite drives the codecs through uneven input
  and output splits.
- Eight cargo-fuzz targets with bounded memory: `decode_7z`, `decode_rar`,
  `decode_differential_7z`, `roundtrip_7z`, `roundtrip_carryless` and
  `structure_7z` with committed seed corpora, `range_coders` for the coders
  on their own, and `chunking_invariance`, which checks that arbitrary input
  and output splits give the same bytes, verdict and error position as one
  piece.
- `tools/ppmd-oracle`: a minimal PPMd `.7z` writer and reader and
  out-of-process checks against `7zz` and `unrar`.
- `ppmd-bench`, a one-operation-per-process driver for 7z decode, RAR
  decode and 7z encode that reports its own CPU time, peak heap and peak
  RSS; and the `ppmd-turbo-bench` Go harness with quick, full and fleet
  profiles against 7-Zip, ppmd-rust and RARLAB's unrar.
- CI: a deterministic fuzz run of the seeded targets, the hostile tests
  under AddressSanitizer, Miri, and a dispatch-only `fuzz-extended`
  workflow.
