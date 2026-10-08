# PPMd variant H: algorithms used by ppmd-turbo

ppmd-turbo implements Dmitry Shkarin's PPMII model, variant H (the model 7-Zip
calls `Ppmd7`). It pairs that model with two entropy coders:

- the carry-less range coder used by RAR 2.9 to 4.x PPM blocks and by 7-Zip's
  `7a` (`.pmd`) framing;
- the LZMA-style range coder with carry propagation used by the 7z PPMD method
  (`03 04 01`).

Output must be bit-exact with the reference implementations. A single
difference in model state, sub-allocator free-list order, SEE rounding or coder
normalisation desynchronises the stream permanently.

This document describes the algorithm as the references implement it. Each
component gives line references into those sources:

- 7-Zip 26.03 C sources (`C/Ppmd*.{h,c}` and `CPP/7zip/Compress/Ppmd*.cpp`),
  public domain;
- RARLAB unrar 7.20 (`model.cpp`, `suballoc.cpp`, `coder.cpp`, `unpack30.cpp`).
  These are consulted for behaviour only; nothing in them is copied;
- the ppmd-rust 1.5.0 crate.

**Provenance of the implementation.** ppmd-turbo's model, sub-allocator and
SEE (`src/model.rs`, `src/model/encode.rs`, `src/alloc.rs`, `src/see.rs`) are
a direct, bit-exact translation of ppmd-rust 1.5.0's `internal/ppmd7` (the
ppmd-rust authors, CC0-1.0 OR MIT-0), which is itself a port of 7-Zip's
`C/Ppmd7.c`, `C/Ppmd7Dec.c` and `C/Ppmd7Enc.c` (Igor Pavlov, public domain).
They are not seeded from unrar-rs: an earlier model was, and it was replaced
by this translation. The range coders' arithmetic is 7-Zip's (section 4).
Where this document describes unrar's choices, they are behaviour that the
translation reproduces through 7-Zip's code, not code it contains.

The section [Attribution index](#attribution-index) names the authors of the
algorithms that conventionally carry their author's name.

---

## 1. Model

### 1.1 Data structures

Everything lives in one byte arena. All references are 32-bit offsets from the
arena base (7-Zip `Ppmd.h:103-117`; `PPMD_32BIT` raw pointers are used only on
32-bit hosts). Offset 0 means NULL.

**Context** (12 bytes, `Ppmd7.h:26-44`):

| bytes | field                                                            |
|-------|------------------------------------------------------------------|
| 0-1   | `NumStats` u16 (1..256)                                          |
| 2-3   | union: `SummFreq` u16 when NumStats > 1, or the `OneState` Symbol and Freq bytes |
| 4-7   | union: `Stats` ref (an array of States) when NumStats > 1, or the `OneState` successor |
| 8-11  | `Suffix` ref (the context one order shorter)                     |

A binary context (NumStats == 1) stores its only State inline at byte
offset 2. The 6-byte State there overlays SummFreq and Stats. 7-Zip
deliberately accesses that region through a union to avoid strict-aliasing
reordering (`Ppmd.h:79-99`).

**State** (6 bytes, `Ppmd.h:57-63`): `Symbol` u8, `Freq` u8, and `Successor`
stored as two u16 halves (`Successor_0`, `Successor_1`). The halves keep the
struct at 2-byte alignment. On little-endian hosts, 7-Zip assembles the value
low half first (`Ppmd.h:151-156`). It deliberately avoids an unaligned u32 load
because of miss latency (`Ppmd.h:125-131`); for ppmd-turbo that is a choice to
measure, not a format rule.

**Successor kinds** (`Ppmd7.c:1031-1113` documents the memory map). They are
distinguished by address, ordered `NULL < RAW < UnitsStart <= RECORD`:

- `NULL`: not yet created;
- `RAW`: a pointer into the text area, i.e. the position of the next input byte
  after this state was last seen, kept so CreateSuccessors can build the
  context lazily;
- `RECORD`: a real context in the units area.

**Real-context test.** 7-Zip tests `(Byte *)c > p->Text` (`Ppmd7.c`, NextContext
around `:960-990`), unrar tests `> pText`, and ppmd-rust tests
`>= units_start` (`internal/tagged_offset.rs:98`). All three are equivalent
under the model invariant. The text pointer is only rolled back
(`Text -= (MaxContext != MinContext)`) on a path where the max successor is
already a real context, so no RAW successor can point at or above the current
Text.

**Model globals** (`Ppmd7.h:53-103`): MinContext, MaxContext, FoundState,
OrderFall, InitEsc, PrevSuccess, MaxOrder, HiBitsFlag, RunLength, InitRL,
Size, GlueCount, Base, LoUnit, HiUnit, Text, UnitsStart, AlignOffset,
Indx2Units[38], Units2Indx[128], FreeList[38], NS2Indx[256], NS2BSIndx[256],
HB2Flag[256], DummySee, See[25][16], BinSumm[128][64].

### 1.2 Constants

| name | value | source |
|---|---|---|
| `MAX_O` | 64 | `Ppmd7.h:14-18` |
| `MAX_FREQ` | 124 | `Ppmd7.h` |
| `UNIT_SIZE` | 12 | `Ppmd7.c` |
| `INT_BITS`, `PERIOD_BITS` | 7, 7 | `Ppmd.h:25-26` |
| `BIN_SCALE` | 2^14 | `Ppmd.h:27` |
| `kExpEscape` | {25,14,9,7,5,5,4,4,4,3,3,3,2,2,2,2} | `Ppmd7.c` |
| `kInitBinEsc` | {0x3CDD,0x1F3F,0x59BF,0x48F3,0x64A1,0x5ABC,0x6632,0x6051} | `Ppmd7.c` |
| `GET_MEAN(prob)` | `(prob + (1 << (PERIOD_BITS-2))) >> PERIOD_BITS` | `Ppmd.h:29-30` |
| `UPDATE_PROB_0(p)` | `p + 128 - GET_MEAN(p)` | `Ppmd.h:31` |
| `UPDATE_PROB_1(p)` | `p - GET_MEAN(p)` | `Ppmd.h:32` |

**Static tables** (Construct, `Ppmd7.c:52-81`):

- **Indx2Units.** Bins of step 1 ×4, step 2 ×4, step 3 ×4, then step 4 ×26,
  giving 1,2,3,4,6,8,10,12,15,18,21,24,28,32,...,128 units across 38 indexes
  (`Ppmd.h:34-38`). Units2Indx is its inverse, rounding up.
- **NS2BSIndx.** `[0]=0`, `[1]=2`, `[2..10]=4`, `[11..255]=6`. These are
  doubled indexes into BinSumm's second dimension.
- **NS2Indx.** 0,1,2 for the first three entries, then runs of growing length
  (step, then step+1, ...), up to index 24.
- **HB2Flag.** 0 for symbols below 0x40, 8 at 0x40 and above. 7-Zip computes it
  inline as `PPMD7_HiBitsFlag_3(sym) = ((sym + 0xC0) >> 5) & 8`, and
  `HiBitsFlag_4` gives the 0x10 variant (`Ppmd7.h:123-133`). unrar builds it as
  a table in `StartModelRare` (`model.cpp:74-116`).

### 1.3 RestartModel (`Ppmd7.c:348-428`; unrar `model.cpp:38-71`)

RestartModel runs:

- at init;
- when the sub-allocator cannot satisfy a request;
- when the text area is exhausted (`Text >= UnitsStart`).

It does the following:

1. Clears FreeList and resets the arena.
   - Text = Base + AlignOffset. HiUnit = Text + Size. LoUnit = UnitsStart =
     HiUnit - Size/8/12*7*12. GlueCount = 0.
2. Builds the order-0 root context.
   - NumStats = 256, SummFreq = 257, Suffix = 0. Stats is allocated
     (256 states = 128 units) at the bottom of the units area.
   - State i is `{Symbol=i, Freq=1, Successor=0}`.
3. Sets the model globals.
   - OrderFall = MaxOrder. RunLength = InitRL = -(MaxOrder < 12 ? MaxOrder : 12) - 1.
     PrevSuccess = 0. FoundState = Stats[0]. MinContext = MaxContext = the root.
4. Initialises BinSumm: `BinSumm[i][k+m] = BIN_SCALE - kInitBinEsc[k] / (i + 2)`
   for i < 128, k < 8, m in 0..64 step 8.
5. Initialises SEE: `See[i][k] = {Summ = (5*i + 10) << (PERIOD_BITS - 4), Shift = PERIOD_BITS - 4, Count = 4}`.
   That is Summ = (5i+10)<<3, Shift = 3, Count = 4.
6. Initialises DummySee: `{Summ=0, Shift=PERIOD_BITS, Count=64}`. It is used
   with escFreq = 1 for the 256-symbol root.

### 1.4 Coding a symbol (decoder view; `Ppmd7Dec.c:64-279`)

**Multi-symbol context** (NumStats > 1, `Ppmd7Dec.c:68-131`):

1. Compute `count = rc.GetThreshold(SummFreq)` and walk Stats accumulating
   frequencies.
2. **First-symbol hit** (`count < Stats[0].Freq`): decode `(0, Freq)`, then
   Update1_0.
   - Update1_0 sets PrevSuccess = `2*Freq > SummFreq`, RunLength +=
     PrevSuccess, SummFreq += 4, Freq += 4. If Freq > MAX_FREQ, call Rescale.
     Then NextContext.
3. **Later symbol hit:** decode `(lo, Freq)`, then Update1.
   - Update1 does Freq += 4 and SummFreq += 4. If the state now beats its
     predecessor, the two are swapped. If Freq > MAX_FREQ, Rescale. Then
     NextContext.
4. **Escape** (`count >= SummFreq` after all states): decode
   `(hiCnt, SummFreq - hiCnt)`.
   - HiBitsFlag = HiBitsFlag_3(FoundState.Symbol). Every symbol in the context
     is masked. PrevSuccess = 0.
   - The escape loop runs.
   - A threshold `>= SummFreq` is corrupt data. 7-Zip returns SYM_ERROR; unrar
     checks `count >= scale` (`model.cpp:412-454`).

**Binary context** (NumStats == 1, `Ppmd7Dec.c:132-182`):

1. Select the probability `prob = BinSumm[Freq-1][idx]`, where
   `idx = PrevSuccess + ((RunLength >> 26) & 0x20) + NS2BSIndx[Suffix.NumStats - 1] + HiBitsFlag_4(Symbol) + HiBitsFlag_3(FoundState.Symbol)`.
   - `(RunLength >> 26) & 0x20` is the sign bit of RunLength scaled to 0x20.
   - 7-Zip computes this in GetBinSumm (`Ppmd7.h:123-133`); unrar in
     decodeBinSymbol (`model.cpp:364-393`).
2. Compute `bound = (Range >> 14) * prob` and compare it with Code.
3. **Bit 0 (hit):**
   - prob = UPDATE_PROB_0(prob); Freq += (Freq < 128); FoundState = s;
     PrevSuccess = 1; RunLength++.
   - Then NextContext. Its fast path is `OrderFall == 0 && Successor > Text`,
     which sets `MaxContext = MinContext = successor`; anything else goes to
     UpdateModel.
4. **Bit 1 (escape):**
   - prob = UPDATE_PROB_1(prob); InitEsc = ExpEscape[prob >> 10]; mask the
     symbol; PrevSuccess = 0.
   - Then the escape loop.

**Escape loop** (`Ppmd7Dec.c:184-278`):

1. Walk the Suffix chain, incrementing OrderFall at each step, until a context
   with NumStats > numMasked is found. If the root's Suffix (NULL) is reached,
   the stream is at its end: 7-Zip returns `PPMD7_SYM_END` (-1). The 7z coder
   never writes one; an encoder only reaches it when asked to code symbol -1.
2. Get the SEE estimate (MakeEscFreq, `Ppmd7.c:930-957`). If NumStats != 256,
   take `see = See[NS2Indx[nonMasked-1]] + extras`:
   - `(nonMasked < Suffix.NumStats - NumStats)`
   - `+ 2*(SummFreq < 11*NumStats)`
   - `+ 4*(numMasked > nonMasked)`
   - `+ HiBitsFlag`

   Then `r = Summ >> Shift; Summ -= r; escFreq = r + (r == 0)`. 7-Zip truncates
   Summ to u16 here (`Ppmd7Enc.c:234-238`). unrar's `SEE2_CONTEXT::getMean`
   uses a signed short (`model.hpp`); the two agree because Summ is always
   below 2^15 in practice. That is an invariant to keep, not something to
   rely on silently.
3. Sum the unmasked frequencies to `hiCnt`. Then decode against
   `total = hiCnt + escFreq`:
   - **Hit:** Ppmd_See_UPDATE (`Ppmd.h:51-54`): if `Shift < 7 && --Count == 0`,
     then `Summ <<= 1; Count = 3 << Shift++`. Then Update2: Freq += 4,
     SummFreq += 4, Freq > MAX_FREQ → Rescale, RunLength = InitRL, then
     UpdateModel. The found state is not moved.
   - **Escape:** `see.Summ += total`, mask all states of this context, and loop.
4. Masking.
   - 7-Zip keeps a local 256-byte `charMask` set to 0xFF
     (`PPMD_SetAllBitsIn256Bytes`, `Ppmd.h:163-165`) and sums `Freq & mask`
     branch-free (`Ppmd7Enc.c:252-291`).
   - unrar keeps a persistent `CharMask[256]` of generation stamps. A symbol is
     masked when `CharMask[sym] == EscCount`; EscCount is incremented once per
     coded symbol that went through an escape. On wrap to 0 the array is
     cleared (ClearMask). This saves a 256-byte fill per escape. Both schemes
     select the same set.
   - **In ppmd-turbo** the mask is 7-Zip's local byte array.

### 1.5 Rescale (`Ppmd7.c:808-927`; unrar `model.cpp:119-168`)

1. Move FoundState to the front, shifting the states before it up by one.
2. Add 4 to its Freq. Compute `EscFreq = SummFreq - Freq`.
3. `adder = (OrderFall != 0)`. Halve every Freq: `Freq = (Freq + adder) >> 1`
   (the first state also gets the +4 before halving; 7-Zip writes this as
   `(Freq + 4 + adder) >> 1`). After each halving, keep the array sorted by
   descending Freq with an insertion step.
4. Remove trailing states whose Freq dropped to 0. The freed tail is returned
   with ShrinkUnits, and EscFreq is adjusted.
5. If only one state remains, the context becomes binary. Freq becomes
   `min(MAX_FREQ/3, Freq - (EscFreq>>1) ... )`; the exact loop is
   `do { Freq -= Freq>>1; EscFreq >>= 1 } while (EscFreq > 1)`. The Stats block
   is freed and the state copied inline.
6. Otherwise `SummFreq = sum + EscFreq - (EscFreq >> 1)`.
7. Finally, FoundState = Stats[0].

RAR and 7-Zip write steps 2-3 differently, with identical results.

### 1.6 UpdateModel (`Ppmd7.c:568-803`; unrar `model.cpp:246-355`)

Called after every coded symbol except binary or first-symbol hits that take
the NextContext fast path.

1. **Suffix update.** If FoundState.Freq < MAX_FREQ/4 and MinContext has a
   Suffix, find FoundState.Symbol in the suffix context, call that state p1.
   - If the suffix is binary, its OneState Freq += (Freq < 32).
   - Otherwise, if p1 is not first and `p1.Freq >= p1[-1].Freq`, swap them.
     Then p1.Freq += 2 and SummFreq += 2 if `p1.Freq < MAX_FREQ - 9`.

   unrar and the original Shkarin code pass p1 into CreateSuccessors so it
   need not search again (`model.cpp:171-243`). 7-Zip recomputes it inside
   CreateSuccessors. The result is identical; reusing p1 is a pure speed-up.
2. **OrderFall == 0 case.** Call CreateSuccessors and set
   `MinContext = MaxContext = FoundState.Successor`. A NULL result means
   RestartModel.
3. **Append to text.** Write FoundState.Symbol to `*Text++`. The successor is
   `Text`, a RAW pointer. If `Text >= UnitsStart`, RestartModel.
4. **Successor resolution.**
   - If FoundState.Successor is a RAW pointer (`<= Text`), call
     CreateSuccessors to make it a real context. Restart on failure.
   - If OrderFall then decrements to 0, the successor becomes the new
     MinContext/MaxContext, `Text -= (MaxContext != MinContext)` (the rollback
     mentioned in 1.1), and the function returns.
   - If the successor was NULL, set it to the RAW Text pointer.
5. **Add the symbol** to every context from MaxContext down to (but excluding)
   MinContext. Let `ns = NumStats` and `s0 = SummFreq - ns - (FoundState.Freq - 1)`.
   - **Context with ns > 1:**
     - Grow Stats when ns is even (`(ns & 1) == 0`), via ExpandUnits or
       AllocUnits + copy.
     - SummFreq += `2*(ns1 < ns) + 2*((4*ns1 <= ns) & (SummFreq <= 8*ns1))`,
       where ns1 is MinContext.NumStats.
   - **Binary context:** allocate 1 unit for two states. Copy the OneState and
     set its Freq to `Freq < MAX_FREQ/4 - 1 ? 2*Freq : MAX_FREQ - 4`. Set
     SummFreq = `Freq + InitEsc + (ns > 3)`.
   - **New state frequency** `cf = 2*fFreq*(SummFreq+6)`, `sf = s0 + SummFreq`:
     - if `cf < 6*sf`, then `cf = 1 + (cf > sf) + (cf >= 4*sf)` and SummFreq += 3;
     - else `cf = 4 + (cf >= 9*sf) + (cf >= 12*sf) + (cf >= 15*sf)` and
       SummFreq += cf.
   - Append `{Symbol, cf, successor}` to the context's Stats.
6. Set MaxContext = MinContext = the FoundState's successor context.

### 1.7 CreateSuccessors (`Ppmd7.c:458-558`; unrar `model.cpp:171-243`)

1. Starting from MinContext, walk Suffix links while OrderFall allows, i.e.
   order < MaxOrder. Collect up to MAX_O states whose successor is still the
   RAW pointer `upBranch` (FoundState.Successor). Stop at the first one whose
   successor is real.
2. With `upBranch` pointing at the next text byte, compute the new state the
   chain will carry:
   - Symbol = `*upBranch`; Successor = `upBranch + 1`.
   - The frequency is taken from the corresponding state in the stopping
     context c:
     - if c is binary, its OneState Freq;
     - otherwise compute `cf = s.Freq - 1`,
       `s0 = c.SummFreq - c.NumStats - cf`, and
       `Freq = 1 + (2*cf <= s0 ? (5*cf > s0) : (2*cf + 3*s0 - 1) / (2*s0))`.
   - 7-Zip writes the last formula as `(2*cf + s0 - 1)/(2*s0) + 1`, which is
     equivalent.
3. For each collected state, from the deepest outward, allocate a binary
   context: NumStats = 1, OneState = the new state, Suffix = c. Set the
   collected state's successor to it, and let c = the new context.
4. If allocation fails, return NULL, and the caller restarts.

unrar adds an explicit depth guard (the CVE-2017-17969 fix): `if (pps >= ps + MAX_O) return NULL`.
7-Zip bounds the loop by its array size. ppmd-turbo bounds the walk as well
(a 64-entry array, `PPMD7_MAX_ORDER`), and bounds every symbol search in
CreateSuccessors and UpdateModel by NumStats, where the reference searches
without a bound. Under the invariants of section 1.9 no bound is ever hit; if
one were, the model restarts instead of reading past the array.

### 1.8 Order and memory parameters

| parameter | 7z decode | 7z encode | RAR |
|---|---|---|---|
| MaxOrder | 2..64 | 2..32 | 2..64 (from flags, 1 = error) |
| memory | 2^11 .. 0xFFFFFFFF - 36 | ≥ 2^16, multiple of 4 | (MaxMB + 1) MiB, MaxMB 0..255 |

Sources: `PpmdDecoder.cpp:31-47`, `PpmdEncoder.cpp:72-128`, `unpack30.cpp`
DecodeInit (`model.cpp:571-599`).

**In ppmd-turbo.** `Model::new` and `Model::start` accept the 7z decode
ranges and return `InvalidParameters` for anything else;
`RarDecoder::init_model` takes the order and the size in MiB and rejects
orders outside 2..64 and sizes outside 1..256 MiB. The unrar-rs seed clamped
out-of-range values instead of rejecting them.


### 1.9 Model consistency

The model's only input is the coder's choice among the outcomes the model
offers: which state of a context, or escape. Whatever bytes the stream holds,
the model therefore evolves only through its own update rules, and every
corrupt stream is, to the model, some sequence of legitimate choices. Those
rules keep four invariants:

- **(I1)** the symbols of a context's states are distinct;
- **(I2)** the symbols of a context are a subset of its suffix's;
- **(I3)** the order-0 context holds all 256 symbols;
- **(I4)** between symbols, `MinContext == MaxContext`, and throughout,
  `order(MinContext) + OrderFall == MaxOrder`, where a context's order is its
  depth in the suffix chain.

(I4) holds at RestartModel (the order-0 context, `OrderFall = MaxOrder`); each
escape step moves to the suffix and raises OrderFall; UpdateModel either moves
to the (order + 1) successor and lowers OrderFall, stays at the order-0
context (the null-successor case), or at `OrderFall == 0` stays at MaxOrder,
as NextContext's fast path does.

Rescale is the only rule that removes symbols. It removes zero-frequency
states only when `OrderFall == 0`, which by (I4) means MinContext is a
MaxOrder context. No context has a MaxOrder child, so removing symbols there
keeps (I2) for every child, and the context itself only shrinks. It never
runs at the order-0 context with `OrderFall == 0` (MaxOrder is at least 2), so
(I3) holds.

UpdateModel adds the found symbol to every context from MaxContext down to,
but not including, MinContext. The coder escaped out of each of them, so each
of their symbols was masked before the escape chain reached MinContext, and
the symbol was found in MinContext among its unmasked states: none of those
contexts holds it, which keeps (I1). Each of them gains it, and so does
MinContext, so (I2) holds along the chain. CreateSuccessors' new contexts hold
one symbol that their suffix holds.

Consequences that the implementation relies on:

- In every context the escape loop stops at, the unmasked count is
  `NumStats - numMasked`, at least 1. Summing every state with `Freq & mask`
  (7-Zip, ppmd-rust and ppmd-turbo) and taking the first `NumStats -
  numMasked` unmasked states (unrar) select the same states, so the
  "inconsistent model" case unrar guards against cannot occur, for any input.
- `Suffix.NumStats - NumStats` in MakeEscFreq never wraps (7-Zip's unsigned
  and unrar's signed arithmetic agree), and `NS2Indx[nonMasked - 1]` is in
  range.
- The decoder's search for the count found below `hiCnt` ends inside the
  state array, and every symbol search in CreateSuccessors and UpdateModel
  finds its symbol.
- Every record offset the model follows is one the allocator returned (inside
  the unit area), a text position below UnitsStart, or null.

**Abandoned symbols.** Three things end a symbol part way: a count at or past
the total (SYM_ERROR), the end marker (an escape out of the order-0 context),
and a coder fault on a range scaled to zero (the coder's arithmetic stays
defined, so the model either takes a legitimate choice or reaches one of the
first two, and the symbol is reported corrupt). After an escape, MinContext is below
MaxContext and OrderFall has been raised; carrying on from there would break
(I4) and could add a symbol twice. 7-Zip never continues past such a symbol.
ppmd-turbo's callers may (RAR's decoder does after a corrupt block), so every
abandon path puts `MinContext = MaxContext` and OrderFall back to their values
at the start of the symbol. No valid stream codes a symbol after one of these,
so output is unchanged; the unit test
`noise_keeps_the_model_consistent_across_aborted_symbols` checks (I1) to (I4)
after every symbol of arbitrary input, carrying on through abandoned symbols.

---

## 2. Sub-allocator

### 2.1 Layout

The arena is `Size` bytes plus slack: 7-Zip allocates `AlignOffset + size + UNIT_SIZE`
(`Ppmd7.c:92-103`). Ppmd7_Alloc keeps the existing block when the size matches,
and 7-Zip reuses coders across 7z folders, so the arena is reused.

- **Text area** is the bottom 1/8: `[Base + AlignOffset, UnitsStart)`. It grows
  upward.
- **Units area** is the top 7/8, sized `Size/8/UNIT_SIZE*7*UNIT_SIZE`. It holds
  two regions that grow toward each other:
  - state arrays go upward from LoUnit;
  - contexts (1 unit each) go downward from HiUnit.
- RAR builds the same layout. `StartSubAllocator` sizes the block as
  `t/12*UNIT_SIZE + 2*UNIT_SIZE`, and `InitSubAllocator` mirrors 7-Zip's split
  using `FIXED_UNIT_SIZE = 12`.
  - On hosts where RAR's in-memory unit is larger than 12, the `FakeUnitsStart`
    accounting keeps the model decisions (when to restart, when the text area
    is full) identical to 7-Zip's. ppmd-turbo uses 12-byte units, so it needs
    no fake accounting.
- **In ppmd-turbo** the arena is 7-Zip's: `AlignOffset + Size` bytes with
  `AlignOffset = (4 - Size) & 3`, the text area from `AlignOffset`, and the
  order-0 context in the last unit. It is allocated once and never grows; a
  restart with the same size reuses it, as `Ppmd7_Alloc` does. Records are
  addressed by 32-bit offsets from the arena base and read without bounds
  checks in release builds, on the strength of the invariants in section
  1.9; debug builds, Miri and the fuzz targets check every access.

### 2.2 Free lists

There are 38 singly-linked free lists, one per Indx2Units class.

- **7-Zip** links nodes through their first 4 bytes, and its InsertNode and
  RemoveNode push and pop at the head (`Ppmd7.c:117-147`).
- **SplitBlock** returns the tail of an oversized block to the free list of
  the remainder's class. If the remainder has no exact class, it is split into
  the largest class that fits plus a smaller remainder.
- **unrar** keeps a doubly-linked list during gluing (`suballoc.cpp`).

### 2.3 AllocUnits and AllocContext (`Ppmd7.c:254-301`)

**AllocContext:** pop FreeList[0]. Otherwise take `HiUnit -= UNIT_SIZE` if
`HiUnit != LoUnit`. Otherwise fall through to AllocUnitsRare(0).

**AllocUnits(indx):**

1. Pop FreeList[indx] if it is non-empty.
2. Otherwise take `LoUnit += I2U(indx)*12` if that fits below HiUnit.
3. Otherwise call AllocUnitsRare.

**AllocUnitsRare** (`Ppmd7.c:254-290`):

1. If GlueCount == 0, then GlueCount = 255 and GlueFreeBlocks. If
   FreeList[indx] is now non-empty, pop it.
2. Search the larger classes `indx+1..37` for a non-empty list. If one is
   found, pop it and SplitBlock it down.
3. If none is found, steal from the text gap: `GlueCount--`, and if
   `UnitsStart - Text > numBytes`, then `UnitsStart -= numBytes` and return the
   old UnitsStart.
4. If none of that works, return NULL, and the caller restarts the model.

**ExpandUnits, ShrinkUnits and MoveUnitsUp:**

- **ExpandUnits:** allocate the larger class, copy, free the old block.
- **ShrinkUnits:** move the block into a smaller class (via a free block of
  the target class if one exists), or split it in place.
- **FreeUnits:** push the block onto its class list.

Frequent model edits depend on these, and their exact choice of block
determines future addresses. Addresses do not affect coded output directly.
They do affect it when they decide whether an allocation succeeds (and so
when a restart happens), or whether a successor compares above Text.

### 2.4 GlueFreeBlocks (`Ppmd7.c:162-250`)

GlueFreeBlocks coalesces adjacent free blocks:

1. Stamp each free block. 7-Zip writes Stamp = 0 and the unit count in the
   block's NU field. unrar writes Stamp = 0xFFFF.
2. Chain all free lists into one list, then walk the list, merging each block
   with the physically following free blocks while the combined size stays
   below 0x10000 units.
3. Re-file the merged blocks into the class lists, splitting any non-class
   sizes.

A sentinel stops the walk at the HiUnit/LoUnit boundary:

- 7-Zip sets `Stamp = 1` on the unit at LoUnit, after `if (LoUnit != HiUnit)`.
- unrar writes `*LoUnit = 0`.

Stamps can be distinguished from live data because the first bytes of a live
State or Context are never the stamp: a State has Freq != 0 and a Context has
NumStats != 0. A design that puts Suffix at offset 0 (as unrar-rs does) relies
on a different argument: the low 16 bits of a context offset are a multiple
of 12 and cannot equal 0xFFFF.

**Bit-exactness hazard.** After gluing, the order of blocks in each free list
determines every later allocation address, and therefore when the text area
runs out and when RestartModel fires.

- 7-Zip rewrote the glue to a singly-linked, single-direction walk, with the
  comment that Glue and Fill must walk the list in the same direction.
- unrar uses a doubly-linked list.
- Both produce the same final list order on any input. That has to be
  preserved, not re-derived.
- **In ppmd-turbo** gluing is 7-Zip's singly-linked, single-direction walk,
  translated from ppmd-rust, so the list order is the reference's by
  construction.

---

## 3. SEE (secondary escape estimation)

PPMII escapes are estimated from two tables, both indexed by small context
features.

**Binary contexts use `BinSumm[128][64]`** (u16 probabilities with
BIN_SCALE = 2^14).

- The row is `Freq - 1` of the OneState. Freq is at most 128 in a binary
  context because the bit-0 update saturates at 128.
- The column packs:
  - PrevSuccess (+1);
  - NS2BSIndx[suffix NumStats - 1] (+0/2/4/6);
  - HiBitsFlag_3 of the previous FoundState symbol (+8);
  - HiBitsFlag_4 of the context's symbol (+16);
  - the sign of RunLength (+32).
- Adaptation uses `UPDATE_PROB_0/1`, with time constant 2^PERIOD_BITS.
- After an escape, InitEsc = ExpEscape[prob >> 10] seeds the SummFreq of the
  next context that gets a second symbol (1.6 step 5).

**Multi-symbol contexts use `See[25][16]`** (`CPpmd_See`, `Ppmd.h:44-54`).

- The row is NS2Indx[nonMasked - 1].
- The column packs:
  - whether this context has fewer unmasked symbols than its suffix adds (+1);
  - whether the context is low-frequency relative to its size, i.e.
    SummFreq < 11·NumStats (+2);
  - whether more symbols are masked than remain (+4);
  - HiBitsFlag (+8).
- Each cell holds a scaled running sum Summ with an adaptive Shift:
  - the estimate is `Summ >> Shift`, and Summ decays by that estimate;
  - on an escape, Summ grows by the whole context total;
  - on a hit, Count drops and Shift grows toward 7, which slows adaptation
    and doubles Summ to keep the estimate continuous.

**Root context.** The root (NumStats = 256) uses DummySee with escFreq = 1,
because escaping from the root means end-of-stream.

**In ppmd-turbo** (`see.rs`) row `i` starts at
`Summ = (5*i + 10) << (PERIOD_BITS - 4)`, as in both unrar and 7-Zip.

SEE comes from Charles Bloom's PPMZ. Shkarin's PPMII adds the binary-context
SEE (BinSumm) and the indexing above.

---

## 4. Range coders

Both coders use 32-bit Range and Code, 8-bit renormalisation, and
`kTop = 2^24`. Frequencies are at most 2^16 per context total, so
`Range / total` always leaves at least 8 bits of precision.

**In ppmd-turbo** the model decodes through the `RangeDecoder` trait
(`rc.rs`): `get_threshold(total)`, `decode(start, size)` and
`decode_bit(size0, total)`, the three operations 7-Zip's model calls, plus a
sticky `faulted()` flag. `decode` and `decode_bit` normalize before they
return, so the model never normalizes itself. The unrar-rs seed normalized
explicitly after each decode; every decode there was followed by exactly one
normalize before the next threshold, so moving it inside is bit-exact.

### 4.1 7z coder (Pavlov; LZMA-style, `Ppmd7Dec.c`, `Ppmd7Enc.c`)

**Decoder init** (`Ppmd7Dec.c:16-26`):

- Code = 0, Range = 0xFFFFFFFF.
- The first byte must be 0. Then 4 bytes are shifted into Code.
- Init fails if the first byte is non-zero or `Code == 0xFFFFFFFF`.

**Decoding:**

- `GetThreshold(total) = Code / (Range /= total)`.
- `Decode(start, size)`: `Code -= start*Range; Range *= size`.
- Binary: `size0 = (Range >> 14) * prob`.
  - If `Code < size0`: Range = size0 (bit 0).
  - Otherwise: `Code -= size0; Range -= size0` (bit 1).

**Normalisation is part of the format** (`Ppmd7Dec.c:28-53`). One step is
`if (Range < kTop) { Code = Code<<8 | byte; Range <<= 8; }`. The decoder
applies:

- up to two steps (`RC_NORM`) after a decoded multi-symbol (found or
  SEE-path hit);
- one step (`RC_NORM_1`) after a binary bit 0. One suffices because BinSumm
  never drops below 95/2^14, so one shift restores Range ≥ 2^24;
- none after an escape decode; the escape loop normalises twice at its top
  (`RC_NORM_REMOTE`).

The encoder mirrors this exactly (`Ppmd7Enc.c:44-50`, `:79`, `:166`, `:206`).
A port that normalises "while Range < kTop" everywhere gives the same result
only if the extra iterations never trigger. They can trigger at the remote
point. Copy the schedule; do not re-derive it.

**Encoder** (`Ppmd7Enc.c:15-74`). The cache-and-carry output scheme follows
the byte-oriented range coder described by Michael Schindler
(http://www.compressconsult.com/rangecoder/), itself an instance of
G. N. N. Martin's 1979 range encoding.

- Low is u64; Range, Cache u8, and CacheSize start at 1.
- `Encode(start, size)`: `Low += start*Range; Range *= size`.
- Binary bit 1: `Low += bound; Range -= bound`.
- ShiftLow (`:24-42`):
  - If `(u32)Low < 0xFF000000` or there is a carry (`Low >> 32 != 0`), output
    `Cache + carry` and then CacheSize-1 bytes of `0xFF + carry`, and set
    `Cache = Low >> 24`.
  - CacheSize++; `Low = (u32)Low << 8`.
- Flush is 5 ShiftLow calls (`:69-74`).

The 7z method writes no end marker (`PpmdEncoder.cpp:166`).

### 4.2 Carry-less coder (Subbotin; RAR and 7-Zip `7a`)

- `TOP = 2^24`, `BOT = 2^15`. State is Low, Code, Range, all u32.
- **Decode:**
  - `GetCurrentCount = (Code - Low) / (Range /= total)`.
  - `Decode(start, size)`: `Low += start*Range; Range *= size`.
- **Normalise** (`coder.hpp` ARI_DEC_NORMALIZE; `Ppmd7aDec.c:28-32`):

  ```
  while ((Low ^ (Low + Range)) < TOP
         || (Range < BOT && ((Range = -Low & (BOT - 1)), 1)))
    { Code = Code << 8 | byte; Range <<= 8; Low <<= 8; }
  ```

  When the top byte is settled, it shifts. When Range is small but the top
  byte is not settled, Range is truncated to the distance to the next BOT
  boundary. That truncation throws away a little code space so the encoder
  never needs a carry.
- **7-Zip's 7a decoder** stores `Code` relative to Low (`Code -= start*Range`
  alongside `Low += start*Range`), saving one subtraction per threshold.
  unrar keeps absolute Low and Code. Both are bit-exact.
- **Binary escape:** RAR computes `range >>= 14` before the compare and, on
  bit 1, `Low += bound; Range = Range*(BIN_SCALE - prob)` on the already
  shifted range. 7-Zip writes this as `Range = (Range & ~(BIN_SCALE-1)) - size0`
  (`Ppmd7aDec.c:176`). The two are identical.
- **Corrupt-data checks.**
  - 7-Zip's 7a decoder checks `summFreq > Range` and `freqSum > Range` before
    dividing (`Ppmd7aDec.c:75`, `:230`). The carry-less Range can fall below
    the total, and then `Range / total` is 0.
  - unrar does not check this (a SIGFPE on crafted input); its decodeSymbol1
    only checks `count >= scale`.
  - ppmd-turbo must treat both conditions as corrupt data on every carry-less
    path.
  - **In ppmd-turbo** the RAR decoder faults when `Range / total` (or the
    binary `Range / 2^14`) is 0, and the model reports `CorruptStream`. It
    also faults when a decode leaves Range at 0, which the normalisation
    loop above would never leave. A count of `total` or more is corrupt, as
    in unrar. None of these checks fires on well-formed input.

The **encoder** for the carry-less coder is the mirror image. It exists only
for the `.pmd`/7a framing (see the RAR licensing note in 5.1).

### 4.3 ppmd-turbo's implementation (`src/rc/`)

- **Generic, not dynamic.** Each coder is generic over its input
  (`RangeInput`) or output (`RangeOutput`), and the model is generic over the
  coder (`RangeDecoder`, `RangeEncoder`). After monomorphization there is no
  call through a trait object anywhere on the per-symbol or per-byte path.
- **Input buffering.** A normalization step reads one byte with a single
  comparison against the end of the current buffer. Refills are `#[cold]`
  and out of line. There are three backings:
  - `SliceInput` borrows the whole stream and never refills;
  - `ReadInput` owns a refill buffer (64 KiB by default) over
    `std::io::Read`, so an unbuffered file costs one `read` per refill;
  - `SourceInput` copies a 256-byte window out of a `ByteSource` shared
    with another reader (RAR's LZ bit stream). On drop it consumes exactly
    the bytes the coder took.

  No `unsafe` is needed. The slice and `Vec` lookups use `get(pos)`, whose
  bounds check is the end-of-buffer test, and the window index is masked
  to its power-of-two size.
- **Past the end of the input** every backing feeds zeros and counts them,
  as unrar (`read_byte_or_zero`) and 7-Zip (its `Extra` flag) do. The
  framing reads the count: RAR tolerates a little padding mid-block, 7z
  none. A prefix of a valid stream therefore decodes exactly as the prefix
  followed by zeros does, and reports how many zeros it used.
- **Batch decoding** (backlog D1/D2). The coder registers are plain fields
  and every operation is `#[inline(always)]`. A batch loop in the model
  decodes any number of symbols against the current buffer and goes back
  to a trait only to refill, once per buffer.
- **Normalization schedule.** The 7z coder applies exactly the reference's
  step counts: two after a decode and after a binary miss, one after a
  binary hit. The step at the top of the escape loop is applied eagerly at
  the end of the escape decode; nothing reads the coder in between, so the
  same bytes are read at the same points. The carry-less coder normalizes
  with Subbotin's loop after every operation, which is idempotent.
- **Corrupt scale.** A range scaled to zero means a total past the range,
  a zero total or a zero symbol size. Either decoder records it as a sticky
  fault instead of dividing by zero, or, for the carry-less coder, instead
  of normalizing forever. It leaves `range = 1` so later arithmetic stays
  defined, and the model reports `CorruptStream`. The carry-less coder's
  RAR-style `get_current_count` returns the error directly. The encoders
  fault on the same conditions, and `finish` reports the fault.
- **Initialization.**
  - The 7z decoder rejects a non-zero first byte and a code of
    `0xFFFFFFFF`, as `Ppmd7z_RangeDec_Init` does.
  - The carry-less decoder accepts any code, as unrar does. `new_7a` adds
    `Ppmd7a_RangeDec_Init`'s `0xFFFFFFFF` check.
  - Either decoder returns `Truncated` when the input is shorter than its
    initialization.
- **Resuming.** `RangeCoderState` saves and restores the carry-less
  registers across RAR solid members, without re-reading the four
  initialization bytes.

---

## 5. Framings

### 5.1 RAR 2.9 / 3.x / 4.x PPM blocks

PPMd appears only in the RAR 2.9–4.x compression format (unrar `Unpack29`,
`unpack30.cpp`). The RAR 5.0 format has no PPM mode; `unpack50*.cpp` contains
no PPM code.

**Block start** (`ReadTables30`, `unpack30.cpp` ~640). After byte-aligning the
bit stream, bit 0x8000 of the next 16-bit field selects a PPM block. Otherwise
the block is LZ with Huffman tables.

**DecodeInit** (`model.cpp:571-599`) reads a flags byte:

- **0x20 (reset):** the next byte is MaxMB, and memory = (MaxMB + 1) MiB.
- **0x40:** the next byte is the new EscChar.
- **Order:** the low 5 bits give `order = (flags & 0x1F) + 1`; if > 16 then
  `order = 16 + (order - 16)*3`. The maximum is 64. Order 1 is rejected.
- **Range coder:** 4 bytes of coder init follow.
- **No reset:**
  - If no model exists yet, the block is an error.
  - Otherwise the model, sub-allocator and coder state carry over. In a solid
    archive this is the normal path. On a non-solid file unrar also accepts
    it, reusing the stale model, and a decoder that wants identical output on
    such input must do the same.

**EscChar** defaults to 2 at file start (`UnpInitData30`,
`unpack30.cpp` ~740).

**Symbol loop** (`unpack30.cpp:72-131`). DecodeChar returns a byte or -1.

- A non-EscChar byte is a literal.
- After EscChar, the next symbol selects:

  | next | meaning |
  |---|---|
  | 0 | end of PPM block; read new tables (LZ or PPM) |
  | 2 | end of file in this volume set |
  | 3 | RarVM filter code follows (`ReadVMCodePPM`, `unpack30.cpp:326-360`) |
  | 4 | match: 3 bytes big-endian distance, then 1 byte length; `CopyString(len + 32, dist + 2)` |
  | 5 | run: 1 byte length; `CopyString(len + 4, 1)` |
  | other | literal EscChar |

**DecodeChar** (`model.cpp:602-641`) checks that MinContext and its Stats lie
in `(pText, HeapEnd]` before use. unrar keeps that check because its pointers
are raw.

**Error recovery.** If DecodeChar returns -1, unrar calls `CleanUp`
(`model.cpp:563-568`): the sub-allocator restarts with 1 MiB and order 2, and
decoding switches back to LZ tables. Reaching this path means the data is
corrupt.

**In ppmd-turbo** `RarDecoder::decode_symbol` returns `Ok(None)` for the -1
and `RarDecoder::cleanup` performs CleanUp, so a caller can reproduce
unrar's recovery output exactly; the unrar-rs RAR3 unpacker does. Coder
faults (section 4.2) are `Err(CorruptStream)` instead. The model does not
check its own pointers, as unrar does: section 1.9 shows they cannot go
wrong, whatever the input.

**Solid archives.** The model and coder continue across file boundaries,
mid-block. ppmd-turbo's `RarDecoder` holds the model across blocks and
members; the coder's registers are saved with `RarRangeDecoder::state` and
restored with `from_state`, which reads no init bytes.

**Licensing.** The unRAR licence forbids using unrar source to build a
RAR-compatible compressor. ppmd-turbo's RAR path is therefore decode-only, and
nothing in this crate derives from unrar source text.

### 5.2 7z PPMD method (`03 04 01`)

**Properties** are 5 bytes: `order: u8`, then `memSize: u32 LE`
(`PpmdDecoder.cpp:31-47`; encoder `PpmdEncoder.cpp:130-137`).

**Encoder parameters** (`PpmdEncoder.cpp:16-39`):

- Default memSize is `1 << (level + 19)`.
- Order is `kOrders[level] = {3,4,4,5,5,6,8,16,24,32}`.
- ReduceSize (the known input size) lowers memSize to the smallest
  `2^i ≥ 16·ReduceSize`, for `i` in 16..31.
- Explicit memory must be ≥ 2^16 and a multiple of 4. 2^32 maps to 2^32 - 1 KiB.

**Stream:**

- The coder initialises with the leading 0 byte.
- No end marker is written. The decoder stops at the folder's unpack size.

**Finish semantics** (`PpmdDecoder.cpp:57-129`, `:163-164`). The wrapper's
Extra flag means the decoder read past the input; that is an error.

- In FinishStream mode, after exactly outSize bytes, Code must be 0.
- Otherwise the next symbol must be `SYM_END` with Code == 0 (an explicit end
  marker is accepted though never written).
- The consumed input must equal the packed size.

**Buffering in the reference** (`PpmdDecoder.cpp:15`, `:42`;
`CWrappers.cpp:182-211`). 7-Zip reads input through a 1 MiB byte buffer and
decodes into a 64 KiB output buffer. The encoder uses 1 MiB in and 1 MiB out.

### 5.3 7a / `.pmd` and other carriers (reference only)

7-Zip's `Ppmd7a` pairs variant H with the carry-less coder for the `.pmd`
container. libarchive's `archive_ppmd7.c`, derived from Pavlov's public-domain
2010 `Ppmd7.c`, exposes both coders through a vtable for its 7z and RAR
readers.

---

## 6. Variant I (Ppmd8) differences

Variant I is used by ZIP method 98 and is out of scope for ppmd-turbo's first
release. It is listed so the crate does not accidentally share code that
differs. Sources: `Ppmd8.h`, `Ppmd8.c`, `PpmdZip.cpp`.

- **Context layout.** NumStats is u8 (stored as NumStats - 1) and is followed
  by a Flags byte. Flags caches the HiBits flags of the symbols present and
  feeds SEE and BinSumm indexing.
- **MaxOrder** is at most 16.
- **Restore methods:**
  - RESTART: same as variant H.
  - CUT_OFF: prune old contexts with CutOff/Refresh/RestoreModel instead of
    restarting.
  - FREEZE: disabled in 7-Zip.
- **BinSumm is `[25][64]`.** The row is NS2Indx[Freq-1]; the column includes
  Flags. The initial value uses `kInitBinEsc[k] / (i + 1)`.
- **See is `[24][32]`.** It has different context features. Stamps are used
  for the sub-allocator.
- **Suffix update** increments by 1 rather than 2.
- **Coder.** It uses the carry-less coder with PPMD8_CORRECT_SUM_RANGE checks.
- **ZIP properties.** A u16: `(order - 1) | ((memMB - 1) << 4) | (restore << 12)`.
  An end marker is written.
- **Known bug.** ppmd-rust 1.4.1 fixed an out-of-bounds read in its variant I
  decoder (GHSA-rqc2-j9v2-j22v). This is a reminder that variant I's extra
  model surgery is where corrupt input bites.

---

## Attribution index

Only algorithms that conventionally carry their author's name are listed.
General-purpose implementation techniques are not attributed.

| algorithm | author | licence / status | role in ppmd-turbo |
|---|---|---|---|
| PPMd variant H (PPMII: information inheritance, binary-context SEE, the sub-allocator design) | Dmitry Shkarin. "PPM: one step to practicality", Proc. Data Compression Conference 2002, pp. 202-211 | Variant H source released into the public domain (as recorded in the 7-Zip and unrar file headers) | The model (sections 1-3) |
| Carry-less range coder (1999) | Dmitry Subbotin | Public domain | RAR and 7a coder (section 4.2) |
| SEE, secondary escape estimation, from PPMZ | Charles Bloom: https://www.cbloom.com/papers/ppmz.pdf; retrospective at http://cbloomrants.blogspot.com/2018/05/secondary-estimation-from-ppmz-see-to.html | Published papers | Origin of the escape-estimation scheme (section 3) |
| 7-Zip `Ppmd7` implementation, the 7z range-coder pairing (`Ppmd7z`), and 7z method framing | Igor Pavlov | `C/Ppmd*.{h,c}` public domain | Implementation reference for the model, sub-allocator, 7z coder and 7z framing; the translated model's origin |
| ppmd-rust 1.5.0 (`internal/ppmd7`), the Rust port of 7-Zip's `Ppmd7` | The ppmd-rust authors (github.com/hasenbanck/ppmd-rust) | CC0-1.0 OR MIT-0 | The model, sub-allocator and SEE are translated from it (sections 1-3) |
| RAR 2.9-4.x PPM integration (block framing, EscChar protocol) | Eugene Roshal (format); RARLAB unrar source, copyright Alexander Roshal | unRAR licence: extraction use only; using the source to build a RAR-compatible compressor is forbidden | Behavioural reference only for the RAR path; no code copied |
