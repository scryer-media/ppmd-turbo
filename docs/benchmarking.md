# Benchmarking

How ppmd-turbo is measured: the corpora, the two drivers, the Go harness and
its profiles, and how to read a report.

Every ratio is written reference/contender, so a ratio above 1 means the
contender (ppmd-turbo or ppmd-rust) is faster, smaller or lighter than the
reference. The reference is 7-Zip's `7zz` on 7z rows and RARLAB's `unrar` on
RAR rows.

## What is measured

| Operation | ppmd-turbo | ppmd-rust | 7zz | unrar |
| --- | --- | --- | --- | --- |
| 7z decode (`.7z` archive) | candidate | contender | reference (`7zz t`) | - |
| 7z decode (raw stream) | candidate | reference for ppmd-turbo | - | - |
| RAR 2.9-4.x decode | candidate | contender | reference without unrar, if built with the RAR codecs, else secondary | reference (`unrar t`) |
| 7z encode | candidate | contender | reference (`7zz a`) | - |

The carry-less (RAR-style) encoder is excluded from benchmarking by design.
It exists so the crate can write streams for its own correctness tests; no
profile has a RAR encode row and `ppmd-bench` has no `encode-rar`.

ppmd-turbo rows appear only for the operations the crate provides:
`ppmd-bench info` reports them, and the harness plans from that. Until the
decoders land, a run measures ppmd-rust against 7zz, which is the baseline
ppmd-turbo is then held to.

## Corpora

### Conformance corpus (`tests/fixtures`)

Small and committed: 13 payloads, 81 raw streams, one RARLAB-written PPMd
member, two hostile RAR PPMd blocks and 24 corruption recipes, 1.9 MB in all.
`tests/fixtures/manifest.json` records each payload's SHA-256, each stream's
coder, order, memory size, end-marker flag and producer, the 7zz release and
what ppmd-rust does with every corrupted stream. `tests/conformance_7z.rs` and
`tests/conformance_rar.rs` run over it.

Regenerate it with:

```sh
cargo build --locked --release -p ppmd-corpus
./target/release/ppmd-corpus conformance \
    --sevenzip "$(command -v 7zz)" \
    --rar-source <rarpar>/crates/unrar-rs/tests/fixtures/rar4
```

The output is byte-reproducible for a given 7zz release. The RAR source is
the directory holding rarpar's `rar4_ppm_oldmv.rar` volume set and
libarchive's `test_read_format_rar_ppmd_use_after_free*.rar`; the tool only
reads them.

### Bench corpus (`bench/fixtures/<profile>`)

Generated, never committed. `ppmd-corpus bench --profile quick|full` (or
`ppmd-turbo-bench fixtures`, which runs it) writes:

- one `.7z` per row, written by 7zz with `-m0=PPMd:o=N:mem=M` and a plain
  header, so the harness and `ppmd-bench` can both read it;
- raw ppmd-rust streams for parameters 7zz will not write (7zz shrinks a
  memory size far larger than the input, so a 1 GiB model needs a raw
  stream);
- the uncompressed payloads the encode rows compress;
- `manifest.json`, with each archive's real order and memory size from its
  coder properties and each payload's CRC-32.

Payloads are seeded and deterministic, in five kinds: text (word salad over an
invented vocabulary), binary (fixed-layout records), mixed (alternating 4 KiB
chunks of text, binary and noise), repetitive and random.

| Corpus | Archives | Raw | Sources | Size |
| --- | --- | --- | --- | --- |
| quick | every kind at 1 MiB, order 6, 16 MiB model; text at orders 2, 16, 32 and a 1 MiB model | text 1 MiB, order 8, 1 GiB | text 1 MiB | 6 MB |
| full | text 16 MiB at orders 2, 4, 6, 8, 16, 32 by models 1, 16, 256 MiB; the other kinds at 16 MiB, orders 6 and 16, models 16 and 256 MiB; text 1 MiB | text 16 MiB, order 8, 1 GiB | text and mixed 16 MiB | 240 MB |

### RAR corpus

RARLAB's `rar` is the only RAR writer, so the RAR rows do not generate their
input. They read two archives from rarpar's `ppmd-perf` test-corpus profile,
which rarpar's generators write with RARLAB rar 6.24 from its pinned
`rarpar-bench-rarlab:6.24` image (`bench/rarpar-bench/config/toolchains.json`
in rarpar):

| Archive | rarpar generator | Content | Profiles |
| --- | --- | --- | --- |
| `rar4_ppm_solid_restart.rar` | `ppmd_solid` | 1 600 000 bytes of base64 text, compressed over a small model so the sub-allocator restarts several times | all |
| `rar4_ppm_order16_32m.rar` | `ppmd_perf` | 32 MiB of base64 text, order 16, 16 MiB model (`rar a -ma4 -m5 -mc16:16t+ -md4m`) | full, fleet |

In a rarpar checkout they are under `crates/unrar-rs/tests/fixtures/rar4`;
`cargo xtask test-corpus generate --only ppmd_perf --only ppmd_solid`
rewrites them with the pinned image. Point the harness at that directory with
`--rar-corpus` or `PPMD_BENCH_RAR_CORPUS`. Without one the run has no RAR
rows. Nothing is downloaded at run time; `ppmd-bench decode-rar` checks each
member against the size and CRC-32 in its header.

## Drivers

### `tools/ppmd-bench`

One operation per process, for the harness to time from outside:

```text
ppmd-bench decode-7z  --impl ppmd-turbo|ppmd-rust --in FILE [--order N --mem M [--size LEN]] [--out FILE]
ppmd-bench decode-rar --impl ppmd-turbo|ppmd-rust --in VOLUME [--in VOLUME ...] [--member N] [--out FILE]
ppmd-bench encode-7z  --impl ppmd-turbo|ppmd-rust --in FILE --order N --mem M [--end-marker] [--out FILE]
ppmd-bench info
```

`decode-7z` takes a single-file PPMd `.7z` (stream and parameters from the
header) or a raw stream with `--order` and `--mem`. `decode-rar` decodes a
member whose packed data is one PPMd block, through the RAR3 escape layer,
and checks the result against the size and CRC-32 in the member's header.
Output goes to `--out` or is only checksummed.

Each run prints one JSON line: `bytes_in`, `bytes_out`, `crc32` of the
output, `inproc_seconds` (the codec work alone, input already in memory),
`peak_alloc_bytes` (the high-water mark of live heap during that work, from a
counting allocator) and the process's own `maxrss_bytes`, `user_seconds` and
`sys_seconds`. Exit status 0 is success, 1 a decode or encode failure, 2
usage and 3 an operation the chosen implementation does not provide yet.

ppmd-rust is a dependency of `tools/ppmd-bench` and `tools/ppmd-corpus` only,
never of the crate.

### `bench/ppmd-turbo-bench`

The Go harness. It runs every row as its own child process and measures wall
time on its own clock and CPU and peak RSS from the kernel's accounting of the
exited child (rusage on Linux and macOS, the peak working set on Windows), so
every variant is measured the same way. Variants are interleaved within a
scenario and their order is reversed every repeat. Every decode is checked
against the payload's length and CRC-32; the first measured encode of each
contender is decoded back with ppmd-rust, untimed, and compared with the
source.

```sh
cargo build --locked --release -p ppmd-bench -p ppmd-corpus
cd bench/ppmd-turbo-bench && go build -o ppmd-turbo-bench . && cd ../..

./bench/ppmd-turbo-bench/ppmd-turbo-bench fixtures --profile quick
./bench/ppmd-turbo-bench/ppmd-turbo-bench run --profile quick --machine <label> \
    --rar-corpus <rarpar>/crates/unrar-rs/tests/fixtures/rar4
```

Commands: `fixtures` (generate a corpus), `toolchain` (print what a run would
record), `run`, `report` (rebuild report.json and report.md from raw.json)
and `merge` (one cross-host report from several report.json). Tools are found
from flags, then `PPMD_BENCH_DRIVER`, `PPMD_BENCH_SEVENZIP` and
`PPMD_BENCH_UNRAR`, then the checkout's `target/release` and `PATH`.
`--pin-cpus` confines every process to a CPU range on Linux and Windows, and
`--timeout` (an hour by default) records a process past it as DNF. The harness
needs no network at run time.

`run --list` prints the plan without running it: each scenario and its
variants, the scenario, row and process counts, and a projected duration on a
fleet x86 host, both for the plan as it stands and with ppmd-turbo rows for
every operation. The projection costs each row by its compressed size at the
slowest coding rate seen on the full corpus, so it overstates most rows.

`--only a,b` keeps the scenarios whose id contains one of the substrings.

## Profiles

| Profile | Corpus | Repeats | Warmups | Scenarios | Processes today | Processes with ppmd-turbo | Projected (with ppmd-turbo) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| quick | quick | 1 | 0 | 12 | 22 | 34 | under a minute |
| full | full | 5 | 1 | 48 | 558 | 846 | 93 min |
| fleet | full, text-only memory sweep | 3 | 1 | 40 | 308 | 468 | 47 min |

The counts include the two RAR rows and assume neither unrar nor a
RAR-capable 7zz; each adds one row per RAR scenario.

- **quick** is a smoke run: does every lane work, and roughly where does it
  stand. One repeat, so its ratios are indicative only.
- **full** is the record for a release on one machine.
- **fleet** is for the AWS fleet: one run per instance type, sized to finish
  in under an hour on one host. It drops the non-text payloads' 256 MiB rows,
  which cost the most and add little beyond the text memory sweep. Fleet runs
  are operator-triggered; nothing in this repository starts one.

Results go to `bench/results/<machine>-<profile>/`: `raw.json` (every run),
`report.json` and `report.md`. Only `report.md` and `report.json` are
committed.

## Reading a report

Each cell is the median [min-max] over the measured runs. A row's notes give
ppmd-bench's own in-process time and peak heap, which separate the codec's
cost from process start and file reading. On 7z decode rows 7zz also parses
the container and checks the CRC; ppmd-bench reads the whole file and
checksums the output, so the two do comparable work. On encode rows 7zz's
output is a `.7z` with about 130 bytes of container around the stream.

Peak RSS is one process's high-water mark. A large model is allocated but
touched only as the model grows, so a 1 GiB model over a 1 MiB input shows a
1 GiB peak heap and a few MiB of RSS.
