# ppmd-turbo

Fast PPMd compression for Rust. It reads and writes the PPMd that 7z
archives use and reads the PPMd inside RAR 2.9 to 4.x archives, with output
identical to 7-Zip's and unrar's.

```toml
[dependencies]
ppmd-turbo = "0.2"
```

PPMd here is variant H, the one 7-Zip calls "Ppmd7". The crate has no
dependencies, and without its default `std` feature it builds as `no_std`.
The API can still change before 1.0.

| Format | Decode | Encode |
| --- | --- | --- |
| 7z `PPMD` method | yes | yes |
| RAR 2.9 to 4.x PPMd blocks | yes | no: writing RAR is out of scope |
| RAR5 | RAR5 has no PPMd | – |

7z output is byte-identical to 7-Zip's for the same order and memory size,
and RAR streams decode to exactly what unrar produces.

## Usage

Each codec takes an input buffer and an output buffer, reports how much of
each it used and why it stopped, and keeps none of your bytes. A container
drives it from its own buffers. For code that wants streams,
`ppmd_turbo::io` wraps the 7z encoder and decoder as `Write` and `BufRead`
adapters.

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

`RarPpmd` decodes RAR's PPMd blocks for a RAR reader that handles the rest
of the format: `start_block` for each block header, `decode` for the data,
and `next_symbol` for the command bytes after an escape.

`Params::memory_footprint` is exactly how much memory a codec will
allocate, and an `Arena` can be handed from one codec to the next instead of
allocating again.

Every failure is an `Error` with a kind (corrupt data, bad parameters, a
memory limit, and so on) and the position in the stream where it happened.

## Testing

Besides unit tests and comparisons with 7-Zip and unrar:

- hostile input: truncation, flipped bits, out-of-range parameters, false
  sizes, and a check that the memory used is exactly what
  `memory_footprint` said;
- ten fuzz targets, several of them comparing results with ppmd-rust 1.5.0,
  one checking that input and output split anywhere decode and encode the
  same as in one piece;
- Miri, Valgrind and AddressSanitizer over the `unsafe` code;
- separate-process checks against `7zz` and `unrar` (`tools/ppmd-oracle`).

### Fixtures

The repository holds no binary files. Every test file and fuzz seed is
generated from committed recipes, by one command:

```sh
cargo run --locked --release -p ppmd-corpus -- fixtures
```

It writes the 7-Zip files with `7zz`, fetches the RAR test archives by
digest from rarpar's published test corpus (or reads them from
`--rar-source DIR`), and checks every file against
`tools/ppmd-corpus/fixtures.sha256`. A missing `7zz` or archive skips those
files with a note; `--require-all` makes that an error, as CI does.

`cargo test` runs without them: the files that need no outside tool are
built in memory, and the `7zz` and RAR tests are skipped until the command
has written their files. Set `PPMD_TURBO_REQUIRE_FIXTURES=1` to fail
instead. More in [docs/testing.md](docs/testing.md).

## Benchmarks

`bench/ppmd-turbo-bench` is a Go program that runs ppmd-turbo and ppmd-rust
(through `tools/ppmd-bench`), 7-Zip's `7zz` and, where installed, `unrar`, each
as its own process, and records wall time, CPU time and peak memory. Ratios
are the reference's number divided by ppmd-turbo's, so above 1 means
ppmd-turbo did better.

```sh
cargo build --locked --release -p ppmd-bench -p ppmd-corpus
(cd bench/ppmd-turbo-bench && go build -o ppmd-turbo-bench .)
./bench/ppmd-turbo-bench/ppmd-turbo-bench fixtures --profile quick
./bench/ppmd-turbo-bench/ppmd-turbo-bench run --profile quick --machine <label>
```

The run profiles are `quick`, `full` and `fleet`, and reports land in
`bench/results/<machine>-<profile>/`. Encoding is benchmarked for 7z only.
[docs/benchmarking.md](docs/benchmarking.md) has the inputs and how to read
a report.

## Credits

PPMd variant H is Dmitry Shkarin's design; the 7-Zip implementation and the 7z
range coder are Igor Pavlov's. See [ATTRIBUTION.md](ATTRIBUTION.md).

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE).
