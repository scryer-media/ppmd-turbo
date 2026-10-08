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
