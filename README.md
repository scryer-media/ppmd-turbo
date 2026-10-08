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
| 7z `PPMD` method | implemented | implemented |

RAR streams decode to exactly what unrar produces; 7z output is byte-identical
to 7-Zip's for the same order and memory size, decoding and encoding.

## Usage

Every codec is a step machine over caller slices: a call takes an input
slice and an output slice, returns how many bytes it consumed and produced
and why it stopped, and never keeps caller bytes. A container drives it from
its own buffers; `ppmd_turbo::io` wraps the 7z pair in `BufRead` and
`Write` adapters for code that wants streams.

```rust
use ppmd_turbo::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};

let params = Params::new(6, 16 << 20)?;
let data = b"an invented sentence, an invented sentence";

let mut enc = SevenZEncoder::new(params)?;
let mut packed = vec![0u8; 256];
let step = enc.encode(data, &mut packed)?;
let mut len = step.produced;
len += enc.finish(&mut packed[len..], false)?.produced;

let mut dec = SevenZDecoder::new(params, Some(data.len() as u64))?;
let mut out = vec![0u8; data.len()];
let step = dec.decode(&packed[..len], true, &mut out)?;
assert_eq!(step.status, SevenZStatus::ReachedSize);
assert_eq!(&out[..], &data[..]);
```

`RarPpmd` decodes RAR 2.9-4.x PPMd blocks for an unpacker that owns the
RAR framing: `start_block` per block header, `decode` for literals up to the
escape character, and `next_symbol` for the command bytes after it.
`Params::memory_footprint` is the exact heap a codec allocates, and an
`Arena` can move from one codec to the next.

## Testing

Beyond unit and conformance tests, every decoder and encoder entry point is
covered as follows:

- hostile-input tests: truncation, bit flips, out-of-range parameters,
  restart storms, size lies and an exact heap footprint under a counting
  allocator;
- eight cargo-fuzz targets, differential against ppmd-rust 1.5.0, including
  one that checks arbitrary input and output splits decode and encode as
  one piece does;
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
