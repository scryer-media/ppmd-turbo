# ppmd-turbo

PPMd variant H (7-Zip's "Ppmd7") compression and decompression in Rust,
bit-exact with 7-Zip and RAR, performance-first.

It covers the variant H context model, sub-allocator and secondary escape
estimation, both range coders that carry it (RAR's carry-less coder and
7-Zip's LZMA-style coder) and both framings (RAR's block stream and the 7z
`PPMD` method).

## Status

Pre-release. The API is unstable until 1.0.

| Format | Decode | Encode |
| --- | --- | --- |
| RAR 2.9-4.x PPMd blocks (RAR3) | implemented | out of scope by design; a raw carry-less encoder exists for round-trip tests only |
| RAR5 | not applicable: RAR5 has no PPMd | not applicable |
| 7z `PPMD` method | implemented | planned |

RAR streams decode to exactly what unrar produces; 7z output is byte-identical
to 7-Zip's for the same order and memory size.

## Testing

Beyond unit and conformance tests, every decoder and encoder entry point is
covered as follows:

- hostile-input tests: truncation, bit flips, out-of-range parameters,
  restart storms, size lies and a heap bound under a counting allocator;
- six cargo-fuzz targets with committed seeds, differential against
  ppmd-rust 1.5.0;
- Miri and AddressSanitizer lanes;
- out-of-process checks against `7zz` and `unrar` (`tools/ppmd-oracle`).

See [docs/testing.md](docs/testing.md).

## Benchmarks

The harness is a Go program under `bench/ppmd-turbo-bench`. It runs
`tools/ppmd-bench` (ppmd-turbo and ppmd-rust), 7-Zip's `7zz` and, where
installed, RARLAB's `unrar` as separate processes over a generated corpus, and
records wall time, CPU time and peak RSS. Ratios are written reference/ours, so
a ratio above 1 means ppmd-turbo is better.

```sh
cargo build --locked --release -p ppmd-bench -p ppmd-corpus
(cd bench/ppmd-turbo-bench && go build -o ppmd-turbo-bench .)
./bench/ppmd-turbo-bench/ppmd-turbo-bench fixtures --profile quick
./bench/ppmd-turbo-bench/ppmd-turbo-bench run --profile quick --machine <label>
```

Profiles are `quick`, `full` and `fleet`; reports land in
`bench/results/<machine>-<profile>/`. Only the 7z coder is benchmarked for
encoding. See [docs/benchmarking.md](docs/benchmarking.md) for the corpora,
the RAR inputs and how to read a report.

## Credits

PPMd variant H is Dmitry Shkarin's design; the 7-Zip implementation and the 7z
range coder are Igor Pavlov's. See [ATTRIBUTION.md](ATTRIBUTION.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-Apache) or
[MIT license](LICENSE-MIT) at your option.
