# ppmd-turbo

PPMd variant H (7-Zip's "Ppmd7") compression and decompression in Rust,
bit-exact with 7-Zip and RAR, performance-first.

It covers the variant H context model, sub-allocator and secondary escape
estimation, both range coders that carry it (RAR's carry-less coder and
7-Zip's LZMA-style coder) and both framings (RAR's block stream and the 7z
`PPMD` method).

## Status

Pre-release. The crate is a skeleton; the API is unstable until 1.0.

| Format | Decode | Encode |
| --- | --- | --- |
| RAR 2.9/3.x PPMd blocks (RAR3) | planned | planned |
| RAR5 | not applicable: RAR5 has no PPMd | not applicable |
| 7z `PPMD` method | planned | planned |

RAR streams decode to exactly what unrar produces; 7z output is byte-identical
to 7-Zip's for the same order and memory size.

## Benchmarks

The harness is a Go program under `bench/ppmd-turbo-bench`. Ratios are
written reference/ours, so a ratio above 1 means ppmd-turbo is better.

## Credits

PPMd variant H is Dmitry Shkarin's design; the 7-Zip implementation and the 7z
range coder are Igor Pavlov's. See [ATTRIBUTION.md](ATTRIBUTION.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-Apache) or
[MIT license](LICENSE-MIT) at your option.
