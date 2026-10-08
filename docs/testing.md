# Testing

ppmd-turbo must be bit-exact with 7-Zip and unrar and must never panic,
read out of bounds or do unbounded work on any input. Each layer below
checks part of that. All of them are deterministic: fixed seeds, fixed
iteration counts, no sleeps and no clocks.

| layer | where | runs in |
|---|---|---|
| unit and conformance tests | `src/`, `tests/conformance_*.rs`, `tests/fixtures/` | `cargo test`, every platform |
| encoder tests | `tests/encode_7z.rs`, `tests/encode_carryless.rs` | `cargo test`, every platform; Miri runs the small cases |
| hostile-input tests | `tests/hostile_decode.rs`, `tests/hostile_memory.rs`, `tests/hostile_fixtures/` | `cargo test`, Miri, ASan |
| fuzzing, ten targets | `fuzz/` | nine in the `fuzz` CI lane (short) and `fuzz-extended.yml` (long); `chunking_invariance` on demand |
| in-process differential | the fuzz targets, against ppmd-rust 1.5.0 | with fuzzing |
| out-of-process oracles | `tools/ppmd-oracle/`, `tests/differential_binaries.rs`, `tests/encode_oracle_7zz.rs` | on demand, against `7zz` and `unrar` |
| Miri | `MIRIFLAGS=-Zmiri-disable-isolation cargo miri nextest run` | the `miri-x86_64` CI lane |
| AddressSanitizer | the hostile tests, and every fuzz target | the `asan-linux-x86_64` and `fuzz-*` CI lanes |

## Hostile-input tests

`tests/hostile_decode.rs` drives both decoders with damaged and adversarial
input. Every case runs under `catch_unwind`, and a failure prints the
offending input in hex. The cases are:

- truncation at every length up to 64 bytes, plus 16 seeded random cut
  points per fixture;
- each of the first 64 bytes flipped;
- order and memory out of range on both APIs;
- tiny arenas at high orders, which force restarts like 7-Zip's, and
  200-reset restart storms;
- zero-length input;
- declared sizes larger than the payload;
- the carry-less coder with its range below the context total, which must
  be a fault and never a division by zero;
- the 7z end-marker cases: no marker with a known size, a marker with an
  unknown size, a marker before the known size, data after the marker, and
  no marker with an unknown size.

`tests/hostile_memory.rs` installs a counting global allocator, tallied per
thread so the test harness's own threads cannot move a count. It bounds
peak live heap to the arena plus a fixed slack for long garbage into a tiny
arena and for the restart storm reusing its arena, checks that every codec
allocates exactly `Params::memory_footprint` and reports it, and that an
arena handed on with `into_arena` and `with_arena` is not allocated again.

Every suite reaches the codecs through `tests/common/api.rs`, which feeds
the step API uneven input and output pieces (from one byte to 64 KiB, and
pieces just around the per-symbol margin), so each conformance and hostile
case also exercises the step contract.

The fixtures in `tests/hostile_fixtures/` are generated, with invented
payloads, by `fuzz/src/seeds.rs`. Do not edit them by hand.

```sh
cargo test --locked --test hostile_decode --test hostile_memory
RUSTFLAGS=-Zsanitizer=address cargo +nightly test --locked \
  --target x86_64-unknown-linux-gnu --test hostile_decode --test hostile_memory
```

## Fuzzing

The harness is its own crate (`fuzz/`), built by cargo-fuzz on nightly with
AddressSanitizer. Its library (`fuzz/src/`) holds the byte layouts, the
invented payload generators, the ppmd-rust reference wrapper and the
agreement rule, so every target stays a few lines long.

| target | input | property |
|---|---|---|
| `decode_7z` | 7-byte header (order, memory, flags, size) + stream | no panic; output never exceeds the declared or capped size; illegal parameters are `InvalidParameters`; complete means exactly the known size |
| `decode_rar` | up to 64 blocks, each with a 7-byte header + coder bytes | per block, consumed ≤ input, output ≤ remaining, bounded work per byte; data needs a legal reset since the last fresh decoder |
| `decode_differential_7z` | as `decode_7z` | same verdict and bytes as ppmd-rust 1.5.0, allowing for ppmd-rust treating input end as data end |
| `roundtrip_7z` | 5-byte header + payload | ppmd-turbo's 7z stream is byte-identical to ppmd-rust's; ppmd-turbo and ppmd-rust each decode both streams to the payload |
| `roundtrip_carryless` | 3-byte header + payload | ppmd-turbo's raw carry-less stream decodes to the payload through the RAR block API and through ppmd-rust's `7a` decoder; ppmd-rust's `7a` stream decodes through the RAR block API |
| `structure_7z` | parameters, a payload generator and up to 8 edits (flip, truncate, insert, delete, append) applied to a valid stream | an unedited stream decodes to its payload; an edited one agrees with ppmd-rust |
| `range_coders` | 9-byte operations (a kind and two `u32`s) | both range coders on their own: decoding arbitrary bytes never panics, loops or divides by zero; clamped operations round-trip through each encoder and decoder |
| `chunking_invariance` | 12-byte header (codec, order, memory, flags, split seed) + payload | for 7z, carry-less and RAR (with `next_symbol` after each escape), encoded or raw streams decode to the same bytes, verdict and error position in arbitrary input and output pieces as in one; the encoders write the same stream either way; `NeedInput` only short of one symbol's input, `OutputFull` only on a full slice, errors repeat |
| `checked_vs_unchecked` | mode byte (codec, refill window 1..8, `FinishStream`) + a `decode_rar` or `decode_7z` input | the step decoder given the whole input in one call and a bare internals model driven one symbol at a time over a trickling input, with the framing spelled out, agree on every byte, verdict and input position |
| `model_ops` | up to 48 operations (block start, forget, cleanup, model start and restart, invalid parameters, symbol and block decodes) | invalid parameters change nothing; after any history a block start, model start or restart decodes a fresh known stream exactly |

Every target reaches ppmd-turbo through the shim in `fuzz/src/api.rs`, which
drives each step codec through input and output pieces chosen by a seeded
generator, so a failing input reproduces its own split points.
`range_coders`, `checked_vs_unchecked` and `model_ops` also call the model
and the coders through `ppmd_turbo::internals` directly.
`chunking_invariance` has no committed seeds and is not in the CI matrices;
run it by hand.

Memory per iteration is bounded: decode arenas cap at 64 MiB, round-trip
arenas at 16 MiB, RAR arenas at 16 MiB, generated payloads at 16 KiB and
decoded output at 1 MiB. No target has a `.dict` file: the input is
range-coded, so it has no tokens for libFuzzer to splice.

Committed seeds live in `fuzz/seeds/<target>/`, between 6 and 33 per target for the nine CI targets. New inputs go to `fuzz/corpus/<target>/`, which is ignored by git.

```sh
cargo +nightly fuzz build
mkdir -p fuzz/corpus/decode_7z
cargo +nightly fuzz run decode_7z fuzz/corpus/decode_7z fuzz/seeds/decode_7z -- \
  -runs=20000 -seed=1 -rss_limit_mb=2048
```

CI runs each of the nine seeded targets on x86_64 and aarch64 Linux from
its seeds, with `-seed=1` and a fixed `-runs` per target (20000 for the
decoders, 5000 for the round trips, 2000 for `structure_7z`, 50000 for
`range_coders`, 10000 for `checked_vs_unchecked` and 5000 for `model_ops`).
`fuzz-extended.yml` is
dispatch-only. It takes `runs` and `seed` inputs, uploads crashes and the
grown corpus, and can also run the `7zz` oracles.

### Seeds

The seeds and the hostile fixtures are generated by `fuzz/src/seeds.rs`. The
`seeds_are_current` test (in the `fuzz-harness` CI lane) fails if any file
differs from what the generator produces, or if a file it does not produce
is present. After changing a layout or a fixture, regenerate:

```sh
cargo test --locked --manifest-path fuzz/Cargo.toml --lib -- --ignored regenerate_seeds
```

### A crash, start to finish

1. Download the `fuzz-findings-*` artifact and reproduce:
   `cargo +nightly fuzz run <target> crash-<sha>`.
2. Minimise it: `cargo +nightly fuzz tmin <target> crash-<sha>`.
3. Commit the minimised input as `fuzz/regressions/<target>/<what-it-is>`.
   Both fuzz workflows replay that directory on every run.
4. Add a named case to `tests/hostile_decode.rs` that pins down the root
   cause in decoder terms (parameters and stream, printed in hex on
   failure). The fuzz input format is a harness detail; the hostile test is
   the lasting record.
5. Fix the decoder.

## Miri

```sh
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run --locked --all-features
```

Isolation is off because the hostile tests read their fixtures from
`tests/`; nothing in the suite reads a clock or the environment. The vector
state searches are compiled out under Miri, so it checks the scalar paths.
Long loops run shortened under `cfg(miri)`, and the corpus conformance and
coder-differential suites are left out (`#![cfg(not(miri))]`): each takes
Miri over half an hour, and the same decoders run under Miri through the
hostile and library tests. The unchecked arena accessors in `src/alloc.rs`
rely on a caller invariant that only debug assertions check (see
`ValidatedArenaSpan`); `cargo +nightly fuzz build --debug-assertions` runs the
fuzz targets with those checks on.

## Out-of-process oracles

ppmd-rust is only a proxy. `7zz` and `unrar` are the references.
`tools/ppmd-oracle` (a workspace member, not published) holds:

- `sevenz`: a minimal single-folder PPMd `.7z` writer and reader. It has a
  signature header with both CRCs, a plain `kHeader` (archives from `7zz`
  need `-mhc=off`), one pack stream, one folder with coder `03 04 01` and 5
  property bytes (order, then memory as little-endian `u32`), and one file.
- `binaries`: finds `7zz` (`$PPMD_ORACLE_7ZZ`, `/opt/homebrew/bin/7zz`,
  `/usr/local/bin/7zz`, `/usr/bin/7zz`, then `PATH`) and `unrar`
  (`$PPMD_ORACLE_UNRAR`, then `PATH`), and drives them.
- a CLI, `ppmd-oracle check-7z`, which runs three checks per input and
  parameter pair. First, the codec's stream is byte-identical to 7-Zip's.
  Second, the codec decodes 7-Zip's stream. Third, 7-Zip extracts the
  codec's stream from the tool's container. 7-Zip's encoder accepts orders
  2..=32 and arenas that are a multiple of 4 and at least 64 KiB, and it
  lowers the arena for small inputs. The checks therefore use the
  parameters it actually wrote, and outside that window only the third
  check runs. `--codec reference` (ppmd-rust) validates the tool itself.
  It passes all 704 default checks against 7-Zip 26.01.

```sh
cargo run --locked --release -p ppmd-oracle -- tools
cargo run --locked --release -p ppmd-oracle -- check-7z --codec reference
cargo run --locked --release -p ppmd-oracle -- check-7z --codec turbo [FILE...]
cargo run --locked --release -p ppmd-oracle -- check-rar ARCHIVE...
PPMD_TURBO_ORACLES=1 cargo test --locked --release --test differential_binaries -- --ignored
```

`tests/differential_binaries.rs` includes the tool's container code by path.
Every test in it is ignored, and without `PPMD_TURBO_ORACLES=1` each one
returns at once. With the variable set, a missing `7zz` is a failure. RAR
checks skip when `unrar` is not on `PATH`.

`ppmd-oracle check-7z --codec turbo` runs the decoder check and passes all
144 of its default decoder checks against 7-Zip 26.01. Its encoder and
container checks still report themselves unavailable: the tool's turbo
encode arm is not wired to the crate's 7z encoder yet (see
[Encoder wiring still owed](#encoder-wiring-still-owed)). The encoder is
checked against `7zz` by `tests/encode_oracle_7zz.rs` instead: 288 streams
over the corpus at orders 2 to 32 and arenas of 64 KiB to 16 MiB are
byte-identical to 7-Zip 26.01's, and 7-Zip extracts ppmd-turbo's streams at
orders up to 64 and arenas down to 2 KiB, which its own encoder refuses.

```sh
PPMD_TURBO_ORACLES=1 cargo test --locked --release --test encode_oracle_7zz -- --ignored
```

`check-rar` extracts every non-solid PPMd member with ppmd-turbo
(`ppmd_corpus::rar` parses the archive, one volume at a time) and compares
it with `unrar p`. Members that are stored, solid continuations, or
that leave PPMd for an LZ block or a RarVM filter are reported unavailable.

## Where ppmd-turbo and ppmd-rust differ

The suites hold ppmd-turbo to 7-Zip (`PpmdDecoder.cpp`, `PpmdEncoder.cpp`)
and unrar, not to ppmd-rust. Where ppmd-rust 1.5.0 behaves differently, the
test or the fuzz agreement rule (`fuzz/src/outcome.rs`, `agree`) records it.

Decoding:

- **7z input ends before the data does.** ppmd-turbo returns
  `ErrorKind::Truncated` (`UnexpectedEof` through `io`), as 7-Zip does when it
  reads past the input. ppmd-rust treats the end of input as the end of the
  data and returns what it has. `decode_differential_7z` and `structure_7z`
  allow for this.
- **Error classes.** Where both fail, the agreement rule compares only that
  both failed and the common prefix of the output; ppmd-turbo's error kinds
  (`Truncated`, `Corrupt`, `InvalidParameters`) do not map one to one
  onto ppmd-rust's.
- **7z end marker before a known size.** The decoder returns
  `SevenZStatus::EndMarker` there, as ppmd-rust ends its output, and the
  caller decides what a short stream means. With 7-Zip's FinishStream mode
  (opt-in, as in 7-Zip) it is a corrupt stream, and the coder's code must
  also be zero at the size.
- **Input consumption.** The decoders consume exactly the stream's bytes and
  report the count, where ppmd-rust reads a byte at a time from a reader.
  `io::SevenZReader` consumes from its `BufRead` only what the coder took,
  so the bytes after the stream stay in the reader.
- **RAR end marker reached on padding.** A PPMd end marker decoded after the
  coder has run past the end of its input is `ErrorKind::Truncated`. Without
  this check a member cut short could decode to `Ok` on the zeros the coder
  feeds past the end.
- **RAR block of zero bytes.** The test shim's `decode_block` returns
  `Ok(0)` for empty coder input without starting a block: there is nothing
  to decode. The conformance cut at length 0 expects that, and every later
  cut expects an error.

Encoding (the coded bytes are identical; the differences are in the
`Write` adapter around them):

- **`flush` mid-stream.** ppmd-rust's `flush` flushes the range coder,
  writing its final bytes, so a stream flushed before `finish` no longer
  matches 7-Zip's. `io::SevenZWriter::flush` only hands the bytes the coder
  has settled to the writer; the coder's pending bytes stay until `finish`.
  `tests/encode_7z.rs` writes through the adapter in pieces, finishes twice
  and checks the output matches the one-call encoder's.
- **Writer errors.** ppmd-rust writes each coded byte to the writer as it
  settles and reports a failure from that `write`. `io::SevenZWriter`
  batches coded bytes (64 KiB) and reports the writer's error from the
  `write`, `flush` or `finish` that hands the batch over.
- **Parameters.** Both accept orders 2..=64 and arenas from 2 KiB, wider
  than 7-Zip's encoder (orders up to 32, arenas from 64 KiB, multiples of
  4). Streams outside 7-Zip's encoder window are checked by 7-Zip
  extracting them (`tests/encode_oracle_7zz.rs`).
