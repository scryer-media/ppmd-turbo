# Unsafe-code validation

What CI runs against the crate's `unsafe` code, and what it does not. The
ordinary test matrix (`test-*` in `ci.yml`) runs every suite natively, through
Nextest, on Linux, macOS and Windows for x86-64 and ARM64; rustdoc runs the
doctests separately. The lanes below add instrumentation.

## Inventory

Every `unsafe` block carries a `// SAFETY:` proof. They are in four places:

- `src/arena.rs`: fallible allocation. `try_zeroed` takes a zeroed block from
  the global allocator and hands it to a `Vec<u8>`; `try_box` does the same
  for a `Box<T>`. A refused allocation is an error, never an abort.
- `src/alloc.rs`: the model's arena. The `Vec<u8>` is taken apart into its
  raw parts and rebuilt in `into_vec` and `Drop`; `Send` and `Sync` are
  implemented by hand; and the accessors (`at`, `ptr`, the byte, `u16`,
  `u32` and six-byte state reads and writes, `copy`, `swap`) address the
  arena through one pointer without bounds checks in release builds. Every
  offset comes from the model's own records, and debug builds check each
  access with `debug_assert!`.
- `src/model/simd.rs`: the escape-path kernels that sum and skip masked
  state frequencies, NEON on little-endian ARM64 and SSSE3, AVX2 and
  AVX-512 (VBMI and BW) on x86-64. They read 16 or 32 six-byte states at a
  time from a pointer into the arena. x86-64 picks the widest tier the CPU
  has at model construction (`Tier::detect`).
- `src/model.rs`: the calls into those kernels, with the range they may
  read.

The crate starts no threads. Its codecs are `Send` and move between threads
whole; none is shared, so there is no ThreadSanitizer lane.

## Lanes

| Lane | Scope | Build | Platforms |
| --- | --- | --- | --- |
| `unsafe-valgrind` | Every test in the crate (library, conformance, encoder, hostile and coder suites) under Memcheck, with full leak checking | release, all features | Linux x86-64 and ARM64 |
| `unsafe-asan` | The same tests under AddressSanitizer, with the standard library instrumented (`-Zbuild-std`) and leak detection on | release, all features, `nightly-2026-09-01` | Linux x86-64 and ARM64 |
| `unsafe-miri` | Every test the `cfg(miri)` build compiles in the library, `coder_differential` and `encode_carryless`, one job per test, with `-Zmiri-strict-provenance` and isolation on | debug, all features, `nightly-2026-09-01` | One Linux x86-64 runner per test |
| `fuzz-*` | Every seeded fuzz target, built by cargo-fuzz with AddressSanitizer | release, without debug assertions | Linux x86-64 and ARM64 |

Memcheck and ASan run the release build on purpose: debug builds check every
arena access with `debug_assert!`, so an out-of-bounds offset panics there
before any instrumentation sees it. The release build is the one without
those checks, and the one that ships.

`unsafe-miri-plan` lists the selection with `cargo miri nextest list` under
`cfg(miri)` and `.github/scripts/miri-matrix.py` turns it into the matrix,
keyed by binary and test name. The script fails when the selection is empty,
has duplicates, exceeds GitHub's 256-job matrix limit, picks up a binary it
does not expect, or no longer has tests under `alloc::tests`,
`arena::tests` and `model::tests`. A new test in the selection gets its own
job without a workflow change.

`unsafe-complete` fails if discovery, any Miri shard or any native lane
failed, was cancelled or was skipped; the required `ci-complete` gate
depends on it. No sanitizer suppression or Valgrind suppression file is
used, and no test is retried.

## Not instrumented

What no lane above checks, so that a pass is not read as more than it is:

- **The SIMD kernels under Miri.** They are compiled out under `cfg(miri)`
  (`not(miri)` on every kernel), so Miri checks the scalar escape path only.
  The kernels run under Memcheck and ASan instead, which see a read outside
  the arena's allocation but not a read of the wrong states inside it. That
  the kernels compute what the scalar path does is checked by every
  bit-exactness test and by the differential fuzz targets against ppmd-rust,
  whose escape path is scalar.
- **The SSSE3 and AVX-512 tiers.** Tests do not force a tier: each run takes
  the widest one its CPU has. Valgrind does not emulate AVX-512, so Memcheck
  on x86-64 runs AVX2. ASan on x86-64 runs whatever the hosted runner has,
  AVX-512 on most and AVX2 on the rest. SSSE3 runs only on a CPU without
  AVX2, which no hosted runner is.
- **Reads inside the arena.** The arena is one allocation. Memcheck and ASan
  report an access outside it, and Memcheck an uninitialised read (the arena
  is zeroed, so there are none to find), but neither can tell a wrong offset
  that stays inside it. That is what the debug assertions (every debug-build
  test) and Miri's provenance and bounds checks on the scalar path are for.
- **No guard-page lane.** lzma-turbo and sevenz-turbo place every allocation
  against a no-access page with a test-only global allocator. ppmd-turbo has
  no such test yet: the allocator, its Unix and Windows mappings and the
  child-process proof that the guard faults come to well over the size this
  change allowed for. It would add macOS and Windows coverage of the release
  build's arena accesses, which only the native test matrix has today.
- **Suites outside the Miri selection.** `conformance_7z`, `conformance_rar`
  and `coder_differential` are `#![cfg(not(miri))]`, and the larger cases in
  the library and `encode_carryless` are ignored under `cfg(miri)`.
  `hostile_decode`, `hostile_memory` and `encode_7z` compile under Miri but
  are not selected: twelve of the `hostile_decode` cases each ran past ten
  minutes under Miri on an 18-core Apple M5 Max, which is past the job limit
  on a hosted runner. All of them run under Memcheck and ASan.
- **The binary oracles.** `differential_binaries` and `encode_oracle_7zz`
  are ignored everywhere but the dispatch-only `fuzz-extended.yml` oracle
  job, which runs them uninstrumented.
- **macOS and Windows** have the native test matrix only; every instrumented
  lane is Linux.
