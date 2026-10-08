# ppmd-turbo optimisation backlog

Ratios are reference time / our time, so a value above 1 means ours is
faster. Below, "7zz" is 7-Zip 26.03 and "unrar" is RARLAB unrar 7.20. The
"Seen in" field records where a technique was observed; it is not a formal
attribution. Algorithm attribution lives in `algorithms.md`.

## 0. Starting points (measured, from the brief)

| path | arch / OS | reference/ours |
|---|---|---|
| 7z PPMd decode, sevenz-turbo + ppmd-rust 1.5.0 vs 7zz | Apple M5 Max | 0.38 |
| same | Intel Arrow Lake-H, Linux | 0.60 |
| same | Windows x86-64 | 0.19 |
| RAR4 PPM decode, unrar-rs vs unrar | x86-64 | 0.75-0.90 |
| same | Arm64 | 1.02-1.08 (unrar-rs already faster) |

Target: above 1.0 on every row, for decode first.

Two consequences follow:

- The 7z gap is mostly in plumbing, not in the model. ppmd-rust is a
  line-for-line port of the same `Ppmd7.c` that 7zz runs.
- The RAR gap is in the model hot path. unrar-rs already reads from a slice,
  so its remaining cost is per-access validation and code layout.

### Diagnosed causes on the 7z path

1. **One `read_exact` call per compressed byte.**
   - ppmd-rust `internal/ppmd7/range_coding.rs:54-56` (and `:180-182` for 7a)
     reads into a 1-byte array.
   - sevenz-turbo hands the decoder `Box<dyn Read>` over
     `BoundedReader<source>` (`src/reader.rs:30-56`, constructed at `:2016`;
     decoder built at `src/decoder.rs:215-220`).
   - Each byte is therefore one virtual call plus a bounds bookkeeping step.
     If the source is unbuffered, it is also one `read` syscall per byte.
   - Syscalls cost most on Windows, which matches the 0.19 outlier.
   - 7zz reads through a 1 MiB buffer (`PpmdDecoder.cpp:42`,
     `CWrappers.cpp:182-211`).
2. **`Result<i32, io::Error>` per symbol and a per-byte `Read` loop**
   (`decoder_7.rs:69`, `internal/ppmd7/decoder.rs:5`). The error path is
   threaded through every normalisation.
3. **A fresh `alloc_zeroed` arena for every decoder**
   (`internal/ppmd7.rs:170`).
   - There is no reuse across 7z folders.
   - Large arenas page-fault on first touch inside the timed loop.
   - 7zz keeps the arena when the size matches (`Ppmd7.c:92-103`) and reuses
     its coders across folders.
4. **Encoder: one `write_all(&[byte])` per output byte**
   (`internal/ppmd7/range_coding.rs:272-277`, `:427`, `:479`).

---

## Decode backlog (ranked)

Every item below lists the same fields:

- **What:** the change;
- **Where:** which paths it applies to (7z, RAR, 7a, or all);
- **Effect:** expected effect and why;
- **Bit-exact risk:** risk to bit-exactness;
- **Unsafe:** whether unsafe code is needed;
- **Seen in:** where the technique was observed;
- **Measure:** how to measure it.

### D1. Slice / owned-buffer input with branch-light normalisation

- **What:**
  - The decoder core consumes `&[u8]`. Callers with a `Read` go through an
    internal 64 KiB–1 MiB refill buffer.
  - Normalisation reads `buf[pos]` and checks `pos < end` once per refill
    window instead of once per byte.
  - Use a padded tail: after real input ends, feed zeros and record an overrun
    count. unrar-rs does this with `read_byte_or_zero` and
    `zero_bytes_past_eof`. 7zz does the same with its `Extra` flag.
  - Error checks happen per call, not per byte.
- **Where:** all paths. Biggest effect on 7z through sevenz-turbo.
- **Effect:** Removes a dyn call (and possibly a syscall) per compressed byte.
  Expected to recover most of the Windows 0.19 gap and a large part of
  0.38/0.60. Compressed bytes run at about 0.2–0.4 per symbol for text, so
  per-byte overhead dominates once the model is equal.
- **Bit-exact risk:** None for the bytes consumed. The end-of-stream
  semantics must match:
  - 7z: an Extra overrun is an error; FinishStream requires Code == 0 and
    consumed == packSize (`PpmdDecoder.cpp:105-125`, `:163-164`).
  - RAR: zero padding past the end of the volume is legal mid-block, as
    unrar-rs found (CHANGELOG 0.9.2, 64-zero-byte EOF guard).
- **Unsafe:** No. With `get_unchecked` on a padded buffer it is optional.
- **Seen in:** 7-Zip CByteInBufWrap; unrar-rs `RangeDecoder` and
  `BitReadRangeDecoder`.
- **Measure:** 7z PPMd decode of text and binary corpora at orders 6/16/32,
  from a `File` (unbuffered) and from memory, on Windows, Linux and macOS.
  Track wall time and syscall count (strace -c, dtruss, ETW).

### D2. Batch API: `decode_into(&mut [u8]) -> Result<usize>`

- **What:**
  - One call decodes up to N symbols into the output slice.
  - Hot state stays in locals or registers for the whole batch: Range, Code,
    input pos, MinContext, FoundState, OrderFall, RunLength.
  - State is written back on exit, and errors are a status checked once per
    batch.
  - The RAR wrapper gets a variant that stops at EscChar so the framing layer
    can take over.
- **Where:** all paths.
- **Effect:** Removes per-symbol call, `Result` and `Read` machinery
  (diagnosed cause 2), and keeps the compiler from spilling coder state.
  7-Zip ships a commented-out `Ppmd7z_DecodeSymbols` (`PpmdDecoder.cpp:98-101`).
- **Bit-exact risk:** None if the per-symbol logic is unchanged.
- **Unsafe:** No.
- **Seen in:** 7-Zip (commented hook); LZMA-style decoders generally.
- **Measure:** symbols/s on in-memory input with output to a preallocated
  buffer; perf/Instruments IPC.

### D3. Arena reuse and allocation strategy

- **What:**
  - **Reuse.** A `Ppmd7` value that can be re-initialised in place. A pool
    keyed by memSize lets the 7z folder loop and solid blocks reuse it.
  - **No zeroing.** Allocate uninitialised (`Box<[MaybeUninit<u8>]>`) or
    reuse without clearing. The model never reads a byte it did not write:
    RestartModel initialises the root and its stats, and the allocator hands
    out only units it carved or stamped.
  - **Large arenas (≥ 32 MiB, common at 7z levels 7–9):**
    - Linux: `madvise(MADV_HUGEPAGE)`.
    - Windows: `VirtualAlloc(MEM_RESERVE | MEM_COMMIT)`, plus optional
      large pages.
    - macOS: `VM_FLAGS_SUPERPAGE_SIZE_ANY` where available.
    - Optional pre-fault outside the timed region.
- **Where:** all. Matters most for many small 7z folders (alloc and fault cost
  per folder) and for large mem sizes (TLB misses on random context access).
- **Effect:** Removes calloc and first-touch faults from short streams. Huge
  pages cut dTLB misses on the pointer-chasing model; similar codecs commonly
  gain 5–15%.
- **Bit-exact risk:** None.
- **Unsafe:** Yes, for uninitialised memory and OS allocation APIs. Each needs
  a SAFETY note proving the model writes before reading; under Miri, run with
  a zero-filled arena in test builds.
- **Seen in:** 7-Zip `Ppmd7_Alloc` reuse and `g_BigAlloc`/large-page support;
  ppmd-rust's absence of reuse.
- **Measure:**
  - 7z archive of 10 000 small files with one PPMd folder each vs one big
    folder.
  - dTLB-miss counters (perf `dTLB-load-misses`) at mem 256 MiB and 1 GiB.

### D4. Unchecked offset fast path, justified by the model invariant

- **What:**
  - Keep the arena as one allocation and access Contexts and States through
    unchecked raw offsets on the hot path.
  - One proof covers it: the model structure is input-independent. Every
    sequence of decoded symbols yields a valid model, because the decoder only
    chooses which existing state is found or whether to escape. So every
    stored offset is either 0 or one the allocator handed out.
  - The only input-dependent checks are threshold ≥ total (corrupt data) and
    range < total (carry-less paths).
  - Debug builds and a `checked` feature keep the validation, as unrar-rs's
    `ValidatedArenaSpan` does. Fuzzing runs with both.
- **Where:** all.
- **Effect:** unrar-rs validates every span against `p_text` and the arena end.
  That is the most likely remaining reason it trails unrar on x86-64
  (0.75–0.90) while leading on Arm, where its NEON batching pays for the
  checks. 7-Zip has no pointer validation. Expect a 5–15% win on the RAR path.
- **Bit-exact risk:** None if the invariant holds. If it fails, the result is
  memory unsafety, not wrong output, which is why D16 and Miri gate this item.
- **Unsafe:** Yes. Centralise it in a small `arena` module with typed
  `Ctx(u32)` / `StateRef(u32)` newtypes and documented invariants.
- **Seen in:** 7-Zip (no validation); unrar-rs (full validation, CHANGELOG
  0.4.0); unrar's coarse `(pText, HeapEnd]` check in DecodeChar.
- **Measure:**
  - A/B the `checked` and unchecked builds on the RAR4 PPM corpus.
  - Instruction count (perf stat, `instructions`).

### D5. Prefetch the next context

- **What:**
  - On a binary hit or first-symbol hit, the successor of the candidate state
    is known before the coder decides. Issue a prefetch of
    `Base + successor` (and its stats ref) as soon as the state is read.
  - On escape, prefetch `Suffix` and `Suffix.Stats` before the SEE
    computation.
  - Prefetch the next context's `NumStats/SummFreq` head.
- **Where:** all. Matters most at high orders and large mem, where
  MinContext jumps across a working set far bigger than L2.
- **Effect:** PPMd decode is latency-bound on dependent loads:
  context → stats → successor context. Hiding one miss per symbol is the
  largest model-side lever left once the plumbing is fixed.
- **Bit-exact risk:** None.
- **Unsafe:** Prefetch intrinsics are safe on the stable `core::arch`
  intrinsics, except in `unsafe` target-feature blocks. Prefetching does not
  dereference.
- **Seen in:** match-finder prefetch in LZ encoders; not present in 7-Zip
  `Ppmd7` or unrar.
- **Measure:** L2/LLC miss counters and cycles/symbol at mem 16 MiB vs
  256 MiB, order 8 vs 32.

### D6. Code layout: inline the fast paths, outline the rare ones

- **What:**
  - Force-inline the binary hit, the first-symbol hit and the NextContext fast
    path into the batch loop.
  - Mark `UpdateModel`, `CreateSuccessors`, `Rescale`, `RestartModel` and
    `AllocUnitsRare` as `#[cold]`/`#[inline(never)]`.
  - Put the escape loop in its own non-inlined function, which 7-Zip
    effectively does.
- **Where:** all.
- **Effect:** Smaller hot loop, fewer i-cache and branch-target misses, and
  better register allocation for D2's locals.
- **Bit-exact risk:** None.
- **Unsafe:** No.
- **Seen in:** 7-Zip `Z7_FORCE_INLINE` / `Z7_NO_INLINE` placement
  (`Ppmd7Enc.c:23`, `:60`, `:90`); unrar-rs CHANGELOG 0.5.x restructuring.
- **Measure:** cycles/symbol; perf `frontend_retired` or
  `icache` events; binary size of the hot function.

### D7. Pass the suffix state into CreateSuccessors (7z path)

- **What:** UpdateModel already finds `p1`, the FoundState's symbol in the
  suffix context. Pass it into CreateSuccessors instead of re-scanning the
  suffix's Stats, which 7-Zip does (`Ppmd7.c:458-558`).
- **Where:** 7z and 7a (the RAR path already does this).
- **Effect:** Removes a linear scan of up to 256 states per CreateSuccessors
  call. That call is frequent at high order on fresh data.
- **Bit-exact risk:** Low. It is the same state; the original var.H source and
  unrar pass it. The differential fuzzer against ppmd-rust proves it.
- **Unsafe:** No.
- **Seen in:** unrar `model.cpp:171-243` / `:246-355`; unrar-rs `update_model`
  (`model.rs:1581`).
- **Measure:** count of CreateSuccessors scans and cycles in UpdateModel on
  order-16/32 text.

### D8. Escape masking: generation stamps plus a compact unmasked list

- **What:**
  - Use a persistent `[u8; 256]` stamp array with an EscCount generation, as
    RAR does, instead of filling 256 bytes per escaped symbol.
  - In the second pass, gather the unmasked states into a small scratch array
    of (index, freq) as they are summed. The decode search then walks that
    array instead of re-testing the mask.
  - Clear only on generation wrap and on restart.
- **Where:** all.
- **Effect:** An escape chain no longer pays a 256-byte fill, and the
  find-symbol pass after the threshold drops from O(NumStats) masked tests to
  O(unmasked).
- **Bit-exact risk:** None if the masked set is identical. One edge to
  reproduce: the stamp must be cleared on RestartModel and on generation wrap.
- **Unsafe:** No.
- **Seen in:** unrar `CharMask`/`EscCount` and ClearMask; unrar-rs
  `unmasked_scratch[256]` (packed index/head, reused without zeroing) and
  `clear_mask` (`model.rs:2227`).
- **Measure:** cycles in the escape loop on binary or high-entropy data
  (escape-heavy) vs text.

### D9. Wide loads for context heads and successors

- **What:**
  - Read `NumStats | SummFreq` (or the OneState) as one u32, and a Context
    head as one u64.
  - Read State `Successor` as an unaligned u32 instead of two u16 halves.
  - Write back with matching wide stores where the layout allows.
- **Where:** all.
- **Effect:** Fewer load uops on the critical path. unrar-rs measured gains
  from u64 context-head loads. 7-Zip chose split u16 reads for cache-miss
  latency (`Ppmd.h:125-131`), so measure on all three architectures.
- **Bit-exact risk:** None (little-endian byte order must be fixed explicitly;
  the crate is little-endian-only in layout, or uses `from_le_bytes`).
- **Unsafe:** No, with `from_le_bytes` on slices; optionally yes, with
  unaligned reads via raw pointers under D4.
- **Seen in:** unrar-rs `model.rs` (u64 context-head loads, CHANGELOG 0.4.0);
  7-Zip's comment arguing the opposite.
- **Measure:** micro-benchmarks on M-series, Arrow Lake-H and Windows x86;
  instructions/symbol.

### D10. Division

- **What:** Every multi-symbol step pays one `Range / total` u32 division,
  and every escape step one more. Options:
  - (a) Leave it alone; integer division is 10–20 cycles on current cores.
  - (b) Use `f64` division. It is exact for u32 operands once truncated, and
    can be cheaper on some x86 parts.
  - (c) Reciprocal tables for small totals (≤ 2^16).
- **Where:** all.
- **Effect:** Small. Recent x86 and Apple cores divide fast. Measure before
  doing anything.
- **Bit-exact risk:** (b) and (c) need an exhaustive proof over the whole
  operand range. (c) needs an extra correction step.
- **Unsafe:** No.
- **Seen in:** general practice; not in 7-Zip or unrar.
- **Measure:** cycle breakdown in perf annotate on the division instruction.

### D11. SIMD symbol search and cumulative frequency, per architecture

- **What:**
  - Decode: a vectorised prefix sum over Freq bytes at stride 6 to locate the
    threshold.
  - Escape pass: a vectorised masked sum.
  - Gather symbols and freqs from the stride-6 State layout with NEON `vld3`
    on u16 lanes, or SSSE3 `pshufb`.
- **Where:** all. Benefits contexts with many states (order 0–2, binary data).
- **Effect:** Architecture-dependent. In unrar-rs, NEON batched heads won on
  Arm and the `pshufb` gather lost on x86 (code bloat, so x86 stayed scalar).
  Expected modest gains on Arm, possibly nothing on x86.
- **Bit-exact risk:** None; it is the same arithmetic.
- **Unsafe:** Yes, for intrinsics with runtime dispatch (one binary, runtime
  feature detection).
- **Seen in:** unrar-rs `alloc.rs` (NEON `vld3q_u16` state heads ×8, SSSE3
  symbol gather) and `decode_symbol2` (`model.rs:1063`).
- **Measure:** per-architecture A/B on order-2 and order-4 text and binary
  corpora; keep only per-arch wins.
- **Stance:** the earlier x86 attempts in the seed are not the last word. The
  seed could only retrofit vectors onto a layout chosen for scalar code; this
  crate can choose its layouts. Revisit with an open mind once the scalar
  plumbing (D1 to D3) is in and profiles are fresh, in particular:
  - layouts that make the gather free: a structure-of-arrays shadow of the
    Symbol and Freq bytes for wide contexts (kept in step on update), so the
    search is a plain byte compare and a prefix sum over contiguous bytes
    instead of a stride-6 gather;
  - a SIMD escape pass over the mask of seen symbols (256-bit test on the
    `charMask` bytes), which is independent of the State layout;
  - the x86 ISA tiers the seed never tried: AVX2 compare plus `movemask` for
    the symbol search, AVX-512 VBMI for the gather, and the 128-bit-only
    path on Zen 2 and Denverton class cores;
  - SVE2 and NEON `tbl` on Arm beyond the `vld3` heads;
  - rescale and model restart as the clearest vector candidates (contiguous
    byte halving and arena clearing).
  Each tier lives behind runtime dispatch and is kept only on a measured win
  on that architecture; a loss or wash is removed, not left behind a flag.

### D12. Restart cost and restart storms

- **What:**
  - Build the initial BinSumm and See tables once as a const or static, then
    `copy_from_slice` on restart instead of recomputing divisions.
  - Initialise the root's 256 states with one bulk write.
  - At tiny memory sizes (2 KiB–64 KiB, legal for 7z decode), restarts can
    happen every few hundred symbols. Make sure nothing in RestartModel scales
    with arena size: no clearing of the arena.
- **Where:** all.
- **Effect:** Restart becomes about 3 KiB of memcpy. That matters only for
  small-mem streams and adversarial inputs, but it bounds worst-case work.
- **Bit-exact risk:** None if the table values match `Ppmd7.c:348-428`.
- **Unsafe:** No.
- **Seen in:** 7-Zip RestartModel (computes in place); unrar StartModelRare.
- **Measure:** decode throughput at mem = 2^11, 2^16 and 1 MiB on a 64 MiB
  input; restart count.

### D13. Sub-allocator hot paths

- **What:**
  - Keep `AllocContext`, `AllocUnits(indx)` and the free-list pop and push
    branch-light and inlined.
  - Keep GlueFreeBlocks and the splitting in AllocUnitsRare cold.
  - Use Units2Indx and Indx2Units as `[u8; 128]` / `[u8; 38]` lookups.
- **Where:** all (only during model growth).
- **Effect:** Small, steady gain on fresh, high-order data, where UpdateModel
  allocates often.
- **Bit-exact risk:** High if free-list order changes in any way. Glue order
  must match 7-Zip's singly-linked walk exactly; regression-pin it with the
  unrar-rs `glue_free_blocks_reference` test ported to the 7z path.
- **Unsafe:** Same as D4.
- **Seen in:** 7-Zip `Ppmd7.c:117-301`; unrar-rs `alloc.rs`.
- **Measure:** cycles in UpdateModel and the allocator on order-32 text.

### D14. PGO and target-specific builds

- **What:** Profile-guided optimisation of the hot loop, using the bench
  corpus as the training set. Use `-C target-cpu` only where the deployment
  model allows; the policy is one binary with runtime dispatch.
- **Where:** all.
- **Effect:** Typically 5–15% on branchy decoders.
- **Bit-exact risk:** None.
- **Unsafe:** No.
- **Seen in:** general practice.
- **Measure:** A/B on the full bench matrix; check that the training corpus
  does not overfit (use a held-out corpus).

### D15. Archive-level parallelism (outside this crate)

- **What:** PPMd is inherently sequential within a stream. Parallelism comes
  from decoding independent 7z folders, or independent non-solid RAR members,
  concurrently. That belongs to the archive layers (sevenz-turbo, unrar-rs).
  ppmd-turbo's job is to make it cheap: `Send` decoders and D3's arena pool.
- **Where:** 7z multi-folder, RAR non-solid.
- **Effect:** Near-linear wall-time scaling across folders. CPU per byte is
  unchanged.
- **Bit-exact risk:** None.
- **Unsafe:** No.
- **Seen in:** 7-Zip multi-threaded extraction of independent folders.
- **Measure:** wall time on a multi-folder archive at 1, 2, 4 and 8 threads.

### D16. Correctness gates that unlock D4, D9 and D11 (required before shipping them)

These are listed in the decode ranking because the unsafe fast paths depend on
them. See [Tests and fixtures](#tests-and-fixtures) and
[Fuzzing](#fuzzing).

---

## Encode backlog (ranked)

Encode targets the 7z coder (and the 7a/`.pmd` carry-less encoder). There is
**no RAR encoder**: the unRAR licence forbids using its source to build a
RAR-compatible compressor, and the RAR PPM framing has no other public
specification. Building one would need an operator and legal decision.

### E1. Owned output buffer

- **What:** Encode into `&mut Vec<u8>` or a fixed 1 MiB buffer, and flush to
  the `Write` sink in bulk. ShiftLow's carry loop writes the cache byte, then
  `0xFF + carry` runs, into the buffer.
- **Where:** 7z, 7a.
- **Effect:** Removes `write_all(&[byte])` per output byte (ppmd-rust
  `range_coding.rs:272-277`). This is the encoder's main plumbing cost.
- **Bit-exact risk:** None if the ShiftLow logic is unchanged
  (`Ppmd7Enc.c:24-42`).
- **Unsafe:** No.
- **Seen in:** 7-Zip `CByteOutBufWrap` with a 1 MiB buffer
  (`PpmdEncoder.cpp:148`).
- **Measure:** encode MB/s from memory; write-syscall count.

### E2. Batch encode API with hot state in locals

- **What:** `encode(&[u8])` loops internally with Low, Range, Cache, CacheSize
  and the model pointers in locals (mirrors D2).
- **Where:** 7z, 7a.
- **Effect:** Same as D2. 7-Zip's `Ppmd7z_EncodeSymbols` (`Ppmd7Enc.c:319-325`)
  is exactly this.
- **Bit-exact risk:** None.
- **Unsafe:** No.
- **Seen in:** 7-Zip.
- **Measure:** encode MB/s.

### E3. Symbol search in multi-symbol contexts

- **What:** The encoder must find `symbol` among NumStats states (stride 6).
  Use SIMD byte-compare over the gathered Symbol bytes, or a scalar loop
  unrolled two at a time as 7-Zip does. In the escape pass, fuse the
  masked-sum with the search, as 7-Zip does (`Ppmd7Enc.c:252-291`).
- **Where:** 7z, 7a.
- **Effect:** Moderate on low-order and binary data, where contexts hold many
  states.
- **Bit-exact risk:** None.
- **Unsafe:** Yes, for intrinsics; optional.
- **Seen in:** 7-Zip unrolled loops; unrar-rs gathers.
- **Measure:** encode cycles/symbol at orders 2–6 on binary data.

### E4. Share the model fast paths with decode

- **What:** D4–D9, D12 and D13 apply unchanged to the encoder, which drives the
  same model. Build the model once and parameterise the coder.
- **Where:** 7z, 7a.
- **Effect:** As in decode.
- **Bit-exact risk:** As in decode.
- **Unsafe:** As in decode.
- **Measure:** encode MB/s.

### E5. Exact 7z stream finish

- **What:** Flush is 5 ShiftLow calls and no end marker
  (`PpmdEncoder.cpp:166-169`). Props are `[order, memSize LE]`. The level
  mapping and ReduceSize follow `PpmdEncoder.cpp:16-39`; memSize must be
  ≥ 2^16 and a multiple of 4.
- **Where:** 7z.
- **Effect:** Required for byte-identical archives versus `7zz a -m0=PPMd`.
- **Bit-exact risk:** This item is the bit-exactness requirement itself.
- **Unsafe:** No.
- **Measure:** byte comparison against `7zz` output at levels 0–9, with and
  without a known size (ReduceSize).

---

## Tests and fixtures

All fixtures use invented content: generated text, synthetic binaries, or
random data with controlled entropy. No real media names. Reference outputs
come from the reference binaries (`7zz`, `unrar`; WinRAR/`rar` only to create
RAR4 PPM inputs).

### 7z (variant H, 7z coder)

1. **Order and memory grid.**
   - Orders: 2, 3, 4, 6, 8, 16, 24, 32, and the decode-only 33, 48, 64.
   - Memory: 2^11 (decode-only), 2^16, 1 MiB, 16 MiB, 256 MiB, 1 GiB, and the
     decode maximum `0xFFFFFFFF - 36` where the host allows.
   - Every cell decodes byte-identically with 7zz.
2. **Encoder bit-exactness.** Encode with ppmd-turbo and with `7zz` at levels
   0–9, with and without ReduceSize. Compare the compressed bytes and the props.
3. **Model events, each with a fixture known to trigger it** (counters
   available under a `debug-stats` feature):
   - arena exhaustion → RestartModel;
   - text-area exhaustion (`Text >= UnitsStart`) → RestartModel;
   - AllocUnitsRare stealing from the text gap;
   - GlueFreeBlocks with GlueCount reaching 0;
   - a glue that hits the 0x10000-unit cap;
   - Rescale removing zero-frequency states and collapsing to binary;
   - restart storms at mem 2^11;
   - order-64 CreateSuccessors chains.
4. **End and finish semantics.**
   - Exact outSize with Code == 0 is OK.
   - An explicit end marker followed by Code == 0 is OK.
   - Trailing garbage gives Code != 0, which is an error in finish mode.
   - Truncation sets the Extra flag, which is an error.
   - A first byte != 0 and `Code == 0xFFFFFFFF` at init are errors.
   - Threshold ≥ SummFreq gives SYM_ERROR.
5. **Free-list order.** Port unrar-rs `glue_free_blocks_reference` and add a
   7z-path equivalent that snapshots the FreeList heads after a scripted
   alloc/free sequence against an instrumented 7-Zip build.
6. **Reuse.** One decoder instance decodes folder A, then folder B with the
   same and with a different memSize. Output must equal fresh-decoder output
   (guards D3).

### 7a / `.pmd` (variant H, carry-less coder)

7. Round-trip and cross-decode against ppmd-rust's 7a encoder/decoder and
   7-Zip's 7a. Include crafted inputs that make `summFreq > Range` and
   `freqSum > Range`; both must be corrupt-data errors, never a division by
   zero.

### RAR 2.9–4.x PPM (decode only)

8. **Header and block cases.**
   - Non-solid single file, PPM only.
   - Mixed LZ ↔ PPM block switches (EscChar then 0).
   - Reset blocks with MaxMB 0, 1, 16 and 255.
   - Order flags covering 2..16 and the >16 mapping up to 64.
   - Order 1 rejected.
9. **Escape protocol.**
   - EscChar sequences 0, 2, 3 (a RarVM filter, e.g. E8/delta), 4 (match with
     maximum distance and length) and 5 (run).
   - A literal EscChar.
   - EscChar changed mid-stream via flag 0x40.
   - The default EscChar 2 at file start.
10. **Solid archives.** Multi-member solid archives where:
    - the model and coder continue mid-block across members;
    - a later member starts a PPM block without reset (model reuse);
    - a no-reset block appears on a non-solid file, which unrar accepts.
11. **Corruption.**
    - A no-reset block with no prior model is an error.
    - Threshold ≥ scale is an error.
    - Zero bytes past the end of the volume are legal up to the guard.
    - Expected behaviour is an error result where unrar would CleanUp,
      matching unrar-rs's chosen policy. Also cover the CVE-2017-17969 class
      of deep CreateSuccessors chains.
12. **RAR 5.0 archives** never reach the PPM path. RAR 5.0 has no PPMd. Fix
    the incorrect claim in the unrar-rs `ppmd/mod.rs` comment when the code
    is extracted.

### Cross-cutting

13. **Miri.** Small-mem (2^11–2^16) decode and encode of short inputs, plus the
    allocator unit tests, under Miri. This covers every unsafe block in D3, D4
    and D9.
14. **No-panic.** `#![deny(clippy::panic, clippy::unwrap_used,
    clippy::indexing_slicing)]` in non-test code paths that handle input.
    Corrupt data returns `Error::Corrupt`.
15. **Bounded work.** For every input, decoded output ≤ declared size, and
    work per output byte is bounded. In particular the escape walk is ≤ 64
    contexts, and the restart rate is bounded.

---

## Fuzzing

All targets run under cargo-fuzz (libFuzzer) with ASan, plus a nightly
`-Zsanitizer=memory` or Miri run on the corpus. Seed corpora come from the
fixtures above. The existing `fuzz/fuzz_targets/decode_7z.rs` becomes target F1.

| target | input | oracle / property |
|---|---|---|
| F1 `decode_7z` | props (order, mem clamped to ≤ 16 MiB) + stream + outSize | no panic or UB; output identical to ppmd-rust 1.5.0 including the error/OK verdict (the error class may differ) |
| F2 `decode_7a` | same, carry-less | identical to ppmd-rust 7a, except range-check cases where ppmd-turbo must report corrupt data |
| F3 `roundtrip_7z` | arbitrary bytes + order + mem | `decode(encode(x)) == x`; `encode(x)` byte-identical to ppmd-rust's encoder |
| F4 `decode_rar_ppm` | structure-aware: generated flags byte, MaxMB, EscChar, then raw coder bytes, wrapped in a minimal LZ/PPM bitstream | no panic or UB; identical to unrar-rs (the extraction seed) during transition |
| F5 `checked_vs_unchecked` | any F1/F4 input | the `checked` build (validated spans) and the unchecked fast path produce identical output and verdicts; any validation failure in `checked` is a bug in the D4 invariant |
| F6 `model_ops` | sequence of symbols driving the model directly | allocator invariants hold after every step: disjoint free lists, units within `[UnitsStart, HiUnit)`, every successor 0, RAW or a real context |

**Out-of-process oracles (scheduled, not per-commit):**

- Periodically replay the minimised corpus through `7zz` and `unrar` binaries.
  These are the true references; ppmd-rust and unrar-rs are only in-process
  proxies.
- Generated 7z and RAR4 archives are run through `7zz t` / `unrar t` and
  compared with the ppmd-turbo output hash.

**Coverage goals:** the RestartModel, GlueFreeBlocks, AllocUnitsRare-steal and
Rescale-to-binary branches are all reached from the corpus. Track this with
`cargo fuzz coverage`.
