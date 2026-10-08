# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.1.0 - 2026-10-08

- Initial crate skeleton.
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
