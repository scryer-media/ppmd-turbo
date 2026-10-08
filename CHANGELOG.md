# Changelog

All notable changes to this project are documented in this file. Each heading
is a released version.

## 0.1.0 - 2026-10-08

- Initial crate skeleton.
- Hostile-input tests (truncation, bit flips, parameter range, restart
  storms, size lies, end-marker cases, a heap bound) with generated fixtures.
- Six cargo-fuzz targets with committed seed corpora and bounded memory:
  `decode_7z`, `decode_rar`, `decode_differential_7z`, `roundtrip_7z`,
  `roundtrip_carryless` and `structure_7z`.
- `tools/ppmd-oracle`: a minimal PPMd `.7z` writer and reader and
  out-of-process checks against `7zz` and `unrar`.
- CI: a deterministic fuzz run of every target, the hostile tests under
  AddressSanitizer, and a dispatch-only `fuzz-extended` workflow.
