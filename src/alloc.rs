//! The sub-allocator.
//!
//! Variant H's unit allocator (Dmitry Shkarin's design), translated from
//! ppmd-rust 1.5.0's `internal/ppmd7.rs` (CC0-1.0 / MIT-0), which is itself a
//! translation of Igor Pavlov's `C/Ppmd7.c` in 7-Zip (public domain). The
//! layout, the free lists, the glue pass and the rare-allocation fallback are
//! the reference's line for line; only the representation differs (32-bit
//! offsets from the arena base instead of pointers).
//!
//! One contiguous arena of `align_offset + size` bytes, where
//! `align_offset = (4 - size) & 3`:
//!
//! - the **text region** starts at `align_offset` and grows up; it holds the
//!   raw symbols the model has not yet turned into contexts;
//! - the **unit region** is the upper seven eighths, in 12-byte units, from
//!   `units_start` to the end. `lo_unit` grows up for state arrays, `hi_unit`
//!   grows down for contexts, and freed blocks go to 38 indexed free lists.
//!   The order-0 context is always the arena's last unit.
//!
//! The layout is part of the format: when the arena fills, the model restarts,
//! and the point at which it fills depends on every allocation and on the
//! free lists' order. RAR's unrar reaches the same restart points through its
//! "fake" 12-byte unit accounting, so one layout serves both framings.
//!
//! **Safety.** The arena is addressed without bounds checks in release
//! builds. Every offset the model hands to the accessors here comes from the
//! model's own records: the allocator only returns blocks inside the unit
//! region, the text pointer stays below `units_start`, and the model's
//! structure is consistent whatever the input (see the proof in
//! `docs/algorithms.md`, "Model consistency"). Debug builds, Miri and the fuzz
//! targets check every access against the arena with `debug_assert!`.

use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use alloc_crate::vec::Vec;

use crate::arena::{ArenaSource, Lent, Storage, fresh_storage};
use crate::error::Result;

/// Size of one allocation unit in bytes.
pub(crate) const UNIT_SIZE: u32 = 12;

/// Number of free lists (`PPMD_NUM_INDEXES`).
pub(crate) const NUM_INDEXES: usize = 38;

/// `Index2Units` and `Units2Index`, as `Ppmd7_Construct` builds them.
const fn unit_tables() -> ([u8; NUM_INDEXES], [u8; 128]) {
    let mut index2units = [0u8; NUM_INDEXES];
    let mut units2index = [0u8; 128];
    let mut k = 0usize;
    let mut i = 0usize;
    while i < NUM_INDEXES {
        let step = if i >= 12 { 4 } else { (i >> 2) + 1 };
        let mut s = 0;
        while s < step {
            units2index[k] = i as u8;
            k += 1;
            s += 1;
        }
        index2units[i] = k as u8;
        i += 1;
    }
    (index2units, units2index)
}

const TABLES: ([u8; NUM_INDEXES], [u8; 128]) = unit_tables();

/// Units in the blocks of each free list.
pub(crate) static INDEX2UNITS: [u8; NUM_INDEXES] = TABLES.0;

/// Entry `nu - 1` is the first free list whose blocks hold `nu` units or more.
pub(crate) static UNITS2INDEX: [u8; 128] = TABLES.1;

/// `I2U`.
#[inline(always)]
pub(crate) fn i2u(index: u32) -> u32 {
    INDEX2UNITS[index as usize] as u32
}

/// `U2I`: the free list for a block of `nu` units (`1..=128`).
#[inline(always)]
pub(crate) fn u2i(nu: u32) -> u32 {
    UNITS2INDEX[nu as usize - 1] as u32
}

/// The glue pass's block header (`CPpmd7_Node`): `stamp` (u16) at 0, `nu`
/// (u16) at 2, `next` (u32) at 4. A free-list entry keeps only its link, at 0.
const NODE_STAMP: u32 = 0;
const NODE_NU: u32 = 2;
const NODE_NEXT: u32 = 4;
const EMPTY_NODE: u16 = 0;

/// The model's arena and the allocator state over it.
///
/// The memory is either a `Vec<u8>` taken apart into its raw parts or the
/// caller's [`ArenaMemory`](crate::ArenaMemory); either way the hot accessors address it through
/// one pointer, and the memory can be handed back
/// ([`into_arena`](Self::into_arena)) for a later model to reuse.
/// Allocation is fallible (`crate::arena`): a refused arena is an error,
/// never an abort. A reused allocation is kept when its capacity is at
/// least the length needed and at most twice it; the layout depends only on
/// the requested size, so a reused arena behaves exactly as a fresh one.
pub(crate) struct Arena {
    base: NonNull<u8>,
    /// Bytes in use: `align_offset + size`.
    len: usize,
    /// The memory's capacity: the `Vec`'s, for rebuilding it, or the
    /// length the caller memory was asked for.
    capacity: usize,
    /// The size requested (`p->Size`).
    size: u32,
    align_offset: u32,
    pub(crate) lo_unit: u32,
    pub(crate) hi_unit: u32,
    pub(crate) text: u32,
    pub(crate) units_start: u32,
    glue_count: u32,
    free_list: [u32; NUM_INDEXES],
    /// The caller's memory `base` points into, or `None` when `base`,
    /// `len` and `capacity` are the crate's own `Vec`.
    caller: Option<Lent>,
    /// Where an arena above the in-RAM limit comes from on a rebuild.
    source: Option<ArenaSource>,
}

// SAFETY: the arena's memory is reached only through `&self`/`&mut self`;
// nothing else holds the pointer (for caller memory, `ArenaMemory`'s
// contract promises it, and `Lent: Send`), so moving the owner to another
// thread moves sole access with it.
unsafe impl Send for Arena {}
// SAFETY: as above; `&Arena` exposes reads of the arena only, and never
// touches `caller` or `source` (they are read only through `&mut self` or by
// value).
unsafe impl Sync for Arena {}

impl Drop for Arena {
    fn drop(&mut self) {
        if self.caller.is_some() {
            // The caller's memory drops with the field.
            return;
        }
        // SAFETY: with no caller memory, `base`, `len` and `capacity` are
        // the raw parts of a `Vec<u8>` taken apart in `from_storage` and
        // rebuilt only here or in `into_parts`, which forgets `self`.
        drop(unsafe { Vec::from_raw_parts(self.base.as_ptr(), self.len, self.capacity) });
    }
}

impl Arena {
    /// The arena length a requested size lays out to: `align_offset + size`
    /// with `align_offset = (4 - size) & 3`, as `Ppmd7_Alloc` sizes it.
    pub(crate) const fn arena_bytes(size: u32) -> usize {
        (4u32.wrapping_sub(size) & 3) as usize + size as usize
    }

    /// Whether an allocation of `capacity` bytes may back an arena of
    /// `needed` bytes: large enough, and not more than twice the need, so a
    /// small stream does not pin a huge arena.
    pub(crate) const fn reusable(capacity: usize, needed: usize) -> bool {
        capacity >= needed && capacity / 2 <= needed
    }

    /// `Ppmd7_Alloc`: an arena of `align_offset + size` bytes, in `arena`'s
    /// memory when it is [`reusable`](Self::reusable), otherwise in fresh
    /// memory: the caller's when `arena` carries an allocator and the arena
    /// is above its in-RAM limit, else a zeroed allocation. The crate's own
    /// bytes are zeroed so none is ever read uninitialized; the model never
    /// relies on their values (every record it reads it wrote first, and
    /// the glue pass's read past the guard stamp is discarded), so a reused
    /// arena's old bytes, or caller memory's, change nothing.
    ///
    /// Errors: allocation failed, an arena to lend over the ceiling, or
    /// caller memory of the wrong length.
    pub(crate) fn try_new(size: u32, arena: crate::Arena) -> Result<Self> {
        debug_assert!(size >= crate::PPMD7_MIN_MEM_SIZE);
        let len = Self::arena_bytes(size);
        let storage = Self::fit(arena.storage, len, arena.source.as_ref())?;
        Ok(Self::from_storage(storage, size, arena.source))
    }

    /// An arena of `size` bytes, in test code where allocation cannot fail.
    #[cfg(test)]
    pub(crate) fn new(size: u32) -> Self {
        match Self::try_new(size, crate::Arena::empty()) {
            Ok(arena) => arena,
            Err(e) => panic!("test arena: {e}"),
        }
    }

    /// Re-lays the arena out for `size` bytes, keeping the memory when it
    /// is reusable. On error the arena is unchanged: a replacement is
    /// allocated while the old memory is still live.
    pub(crate) fn rebuild(&mut self, size: u32) -> Result<()> {
        let len = Self::arena_bytes(size);
        if Self::reusable(self.capacity, len) {
            let placeholder = Self::from_storage(Storage::Owned(Vec::new()), 0, None);
            let (storage, source) = core::mem::replace(self, placeholder).into_parts();
            let storage = Self::fit(storage, len, source.as_ref())?;
            *self = Self::from_storage(storage, size, source);
        } else {
            let storage = fresh_storage(len, self.source.as_ref())?;
            let source = self.source.take();
            *self = Self::from_storage(storage, size, source);
        }
        Ok(())
    }

    /// Sets where an arena above the in-RAM limit comes from on the next
    /// rebuild. The memory in use stays.
    pub(crate) fn set_source(&mut self, source: Option<ArenaSource>) {
        self.source = source;
    }

    /// The memory and its source, for a later model to reuse.
    pub(crate) fn into_arena(self) -> crate::Arena {
        let (storage, source) = self.into_parts();
        crate::Arena { storage, source }
    }

    fn into_parts(self) -> (Storage, Option<ArenaSource>) {
        let mut this = ManuallyDrop::new(self);
        let source = this.source.take();
        let storage = match this.caller.take() {
            Some(mem) => Storage::Caller(mem),
            // SAFETY: with no caller memory, the raw parts of the `Vec`
            // taken apart in `from_storage`; `this` is never dropped, so
            // the allocation has one owner again.
            None => Storage::Owned(unsafe {
                Vec::from_raw_parts(this.base.as_ptr(), this.len, this.capacity)
            }),
        };
        (storage, source)
    }

    /// Bytes the memory holds.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the memory is the caller's.
    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn is_caller_memory(&self) -> bool {
        self.caller.is_some()
    }

    /// `storage` fitted to exactly `len` bytes when it is reusable,
    /// otherwise fresh storage (the old is freed first).
    fn fit(storage: Storage, len: usize, source: Option<&ArenaSource>) -> Result<Storage> {
        if let Some(src) = source {
            src.admit(len)?;
        }
        if !Self::reusable(storage.capacity(), len) {
            drop(storage);
            return fresh_storage(len, source);
        }
        match storage {
            Storage::Owned(mut buf) => {
                // Within capacity: no allocation.
                crate::arena::fit(&mut buf, len)?;
                Ok(Storage::Owned(buf))
            }
            Storage::Caller(lent) => Ok(Storage::Caller(lent)),
        }
    }

    /// Takes `storage` apart and lays an arena of `size` bytes out over it.
    /// An owned `Vec`'s length must be `arena_bytes(size)` (or 0 for the
    /// placeholder `rebuild` swaps in); caller memory must hold at least
    /// `arena_bytes(size)` bytes.
    fn from_storage(storage: Storage, size: u32, source: Option<ArenaSource>) -> Self {
        let (base, len, capacity, caller) = match storage {
            Storage::Owned(buf) => {
                let mut buf = ManuallyDrop::new(buf);
                // `as_mut_ptr` never returns null: an empty `Vec` holds a
                // dangling, non-null pointer.
                let base = NonNull::new(buf.as_mut_ptr()).unwrap_or(NonNull::dangling());
                (base, buf.len(), buf.capacity(), None)
            }
            Storage::Caller(lent) => {
                // The pointer and length were taken once, and checked, when
                // the allocator returned the memory.
                let capacity = lent.len();
                let len = Self::arena_bytes(size);
                assert!(len <= capacity, "caller memory shorter than its arena");
                (lent.ptr(), len, capacity, Some(lent))
            }
        };
        let mut arena = Self {
            base,
            len,
            capacity,
            size,
            align_offset: 4u32.wrapping_sub(size) & 3,
            lo_unit: 0,
            hi_unit: 0,
            text: 0,
            units_start: 0,
            glue_count: 0,
            free_list: [0; NUM_INDEXES],
            caller,
            source,
        };
        if len != 0 {
            arena.reset();
        }
        arena
    }

    /// The size the arena was created with.
    #[inline]
    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    /// Address of the arena allocation, for tests that check a same-size
    /// restart or a handed-back arena keeps it.
    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn arena_addr(&self) -> usize {
        self.base.as_ptr() as usize
    }

    /// The layout part of `RestartModel`: empty free lists, the text pointer
    /// at the start, and the unit region at the upper seven eighths. The
    /// arena's bytes are left as they are.
    pub(crate) fn reset(&mut self) {
        self.free_list = [0; NUM_INDEXES];
        self.text = self.align_offset;
        self.hi_unit = self.text + self.size;
        self.units_start = self.hi_unit - self.size / 8 / UNIT_SIZE * 7 * UNIT_SIZE;
        self.lo_unit = self.units_start;
        self.glue_count = 0;
    }

    // ---- raw access --------------------------------------------------------

    #[inline(always)]
    fn at(&self, off: u32, len: usize) -> *mut u8 {
        debug_assert!(
            off as usize + len <= self.len,
            "arena access {off}+{len} past {}",
            self.len
        );
        // SAFETY: `off + len` lies inside the allocation (module invariant,
        // checked above in debug builds), so the offset stays in bounds.
        unsafe { self.base.as_ptr().add(off as usize) }
    }

    #[inline(always)]
    pub(crate) fn u8(&self, off: u32) -> u8 {
        // SAFETY: in bounds (see `at`); every byte is initialized.
        unsafe { *self.at(off, 1) }
    }

    /// [`u8`](Self::u8) at a `usize` offset, for loops that walk records:
    /// a 64-bit index lets the compiler step a pointer instead of
    /// re-extending a 32-bit offset that might wrap.
    #[inline(always)]
    pub(crate) fn byte(&self, off: usize) -> u8 {
        debug_assert!(off < self.len, "arena access {off} past {}", self.len);
        // SAFETY: in bounds (module invariant, checked above in debug
        // builds); every byte is initialized.
        unsafe { *self.base.as_ptr().add(off) }
    }

    /// A read pointer to `len` bytes at `off`, for the vector passes.
    #[cfg(any(
        all(target_arch = "aarch64", target_endian = "little", not(miri)),
        all(target_arch = "x86_64", not(miri))
    ))]
    #[inline(always)]
    pub(crate) fn ptr(&self, off: usize, len: usize) -> *const u8 {
        debug_assert!(off + len <= self.len);
        // SAFETY: `off + len` lies inside the allocation (module invariant,
        // checked above in debug builds).
        unsafe { self.base.as_ptr().add(off) }
    }

    #[inline(always)]
    pub(crate) fn set_u8(&mut self, off: u32, v: u8) {
        // SAFETY: in bounds (see `at`); `&mut self` is the only access.
        unsafe { *self.at(off, 1) = v }
    }

    #[inline(always)]
    pub(crate) fn u16(&self, off: u32) -> u16 {
        // SAFETY: in bounds (see `at`); unaligned read of initialized bytes.
        unsafe { self.at(off, 2).cast::<u16>().read_unaligned() }
    }

    #[inline(always)]
    pub(crate) fn set_u16(&mut self, off: u32, v: u16) {
        // SAFETY: in bounds (see `at`); `&mut self` is the only access.
        unsafe { self.at(off, 2).cast::<u16>().write_unaligned(v) }
    }

    #[inline(always)]
    pub(crate) fn u32(&self, off: u32) -> u32 {
        // SAFETY: in bounds (see `at`); unaligned read of initialized bytes.
        unsafe { self.at(off, 4).cast::<u32>().read_unaligned() }
    }

    #[inline(always)]
    pub(crate) fn set_u32(&mut self, off: u32, v: u32) {
        // SAFETY: in bounds (see `at`); `&mut self` is the only access.
        unsafe { self.at(off, 4).cast::<u32>().write_unaligned(v) }
    }

    /// Copies `nu` whole units from the block at `src` to the block at `dst`
    /// (`MEM_12_CPY`). The callers copy a live block into one just taken
    /// from the allocator, so the two never overlap.
    ///
    /// One unit per step, as an 8-byte and a 4-byte move, inline: the
    /// blocks are short (at most 128 units), and a call to the C library's
    /// `memmove` cost more than the copy itself. The two moves differ in
    /// size, so the compiler keeps the loop rather than turning it back
    /// into that call.
    #[inline(always)]
    pub(crate) fn copy_units(&mut self, src: u32, dst: u32, nu: u32) {
        let len = (nu * UNIT_SIZE) as usize;
        debug_assert!(
            src as usize + len <= dst as usize || dst as usize + len <= src as usize,
            "unit blocks {src} and {dst} overlap"
        );
        let s = self.at(src, len).cast_const();
        let d = self.at(dst, len);
        let mut i = 0;
        while i < len {
            // SAFETY: `i + UNIT_SIZE <= len`, so both moves stay inside the
            // two `len`-byte ranges, which lie inside the allocation (see
            // `at`); the reads are unaligned reads of initialized bytes. Each
            // unit is read in full before it is written, so the copy is
            // correct even if the blocks did overlap with `dst` below `src`.
            unsafe {
                let lo = s.add(i).cast::<u64>().read_unaligned();
                let hi = s.add(i + 8).cast::<u32>().read_unaligned();
                d.add(i).cast::<u64>().write_unaligned(lo);
                d.add(i + 8).cast::<u32>().write_unaligned(hi);
            }
            i += UNIT_SIZE as usize;
        }
    }

    /// Asks the cache for the line holding `off`, a record the model reads
    /// soon. A hint only: it changes no byte and no result, and on targets
    /// without the hint it does nothing.
    #[inline(always)]
    pub(crate) fn prefetch(&self, off: u32) {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            let p = self.base.as_ptr().wrapping_add(off as usize);
            // SAFETY: a prefetch reads nothing the program can see and
            // never faults, whatever the address; `wrapping_add` keeps the
            // address computation itself defined.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(p.cast_const().cast()) }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        let _ = off;
    }

    /// Swaps the 6-byte records at `a` and `b`.
    #[inline(always)]
    pub(crate) fn swap6(&mut self, a: u32, b: u32) {
        let pa = self.at(a, 6);
        let pb = self.at(b, 6);
        // SAFETY: both records are in bounds (see `at`); `ptr::swap` allows
        // the ranges to overlap.
        unsafe { core::ptr::swap(pa.cast::<[u8; 6]>(), pb.cast::<[u8; 6]>()) }
    }

    /// Reads the 6-byte record at `off`.
    #[inline(always)]
    pub(crate) fn read6(&self, off: u32) -> [u8; 6] {
        // SAFETY: in bounds (see `at`); `[u8; 6]` has alignment 1.
        unsafe { self.at(off, 6).cast::<[u8; 6]>().read() }
    }

    /// Writes the 6-byte record at `off`.
    #[inline(always)]
    pub(crate) fn write6(&mut self, off: u32, v: [u8; 6]) {
        // SAFETY: in bounds (see `at`); `[u8; 6]` has alignment 1.
        unsafe { self.at(off, 6).cast::<[u8; 6]>().write(v) }
    }

    // ---- the allocator (`Ppmd7.c`) ------------------------------------------

    /// `InsertNode`.
    #[inline(always)]
    pub(crate) fn insert_node(&mut self, node: u32, index: u32) {
        let head = self.free_list[index as usize];
        self.set_u32(node, head);
        self.free_list[index as usize] = node;
    }

    /// `RemoveNode`: the list must be non-empty.
    #[inline(always)]
    pub(crate) fn remove_node(&mut self, index: u32) -> u32 {
        let node = self.free_list[index as usize];
        debug_assert_ne!(node, 0);
        self.free_list[index as usize] = self.u32(node);
        node
    }

    /// Whether free list `index` holds a block.
    #[inline(always)]
    pub(crate) fn has_free(&self, index: u32) -> bool {
        self.free_list[index as usize] != 0
    }

    /// `SplitBlock`: keeps the first `I2U(new_index)` units of the block and
    /// frees the rest.
    pub(crate) fn split_block(&mut self, ptr: u32, old_index: u32, new_index: u32) {
        let nu = i2u(old_index) - i2u(new_index);
        let ptr = ptr + i2u(new_index) * UNIT_SIZE;
        let mut i = u2i(nu);
        if i2u(i) != nu {
            i -= 1;
            let k = i2u(i);
            self.insert_node(ptr + k * UNIT_SIZE, nu - k - 1);
        }
        self.insert_node(ptr, i);
    }

    /// `GlueFreeBlocks`. The first u16 of every 12-byte record is its type
    /// stamp: a state array's first state has a nonzero frequency, a context
    /// a nonzero `NumStats`, a free block 0, and the guard at `lo_unit` 1.
    /// The arena's last record is always the order-0 context.
    fn glue_free_blocks(&mut self) {
        let mut n = 0u32;
        self.glue_count = 255;

        if self.lo_unit != self.hi_unit {
            self.set_u16(self.lo_unit + NODE_STAMP, 1);
        }

        // One list of every free block, last list first.
        for (i, &units) in INDEX2UNITS.iter().enumerate() {
            let nu = units as u16;
            let mut next = self.free_list[i];
            self.free_list[i] = 0;
            while next != 0 {
                let node = next;
                next = self.u32(node);
                self.set_u16(node + NODE_STAMP, EMPTY_NODE);
                self.set_u16(node + NODE_NU, nu);
                self.set_u32(node + NODE_NEXT, n);
                n = node;
            }
        }

        // Glue and fill walk the list in the same direction.
        let head = self.glue_blocks(n);
        self.fill_list(head);
    }

    /// The glue half of `GlueFreeBlocks`: merges each block with the free
    /// blocks that follow it, unlinking empty headers. Returns the new head.
    fn glue_blocks(&mut self, first: u32) -> u32 {
        let mut head = first;
        // The link to rewrite when a header is dropped: `None` for `head`,
        // `Some(node)` for that node's `next` field.
        let mut prev: Option<u32> = None;
        let mut n = first;
        while n != 0 {
            let node = n;
            let mut nu = self.u16(node + NODE_NU) as u32;
            n = self.u32(node + NODE_NEXT);
            if nu == 0 {
                match prev {
                    None => head = n,
                    Some(p) => self.set_u32(p + NODE_NEXT, n),
                }
            } else {
                prev = Some(node);
                loop {
                    let node2 = node + nu * UNIT_SIZE;
                    nu += self.u16(node2 + NODE_NU) as u32;
                    if self.u16(node2 + NODE_STAMP) != EMPTY_NODE || nu >= 0x10000 {
                        break;
                    }
                    self.set_u16(node + NODE_NU, nu as u16);
                    self.set_u16(node2 + NODE_NU, 0);
                }
            }
        }
        head
    }

    /// The fill half of `GlueFreeBlocks`: puts every glued block back on the
    /// free lists, in 128-unit pieces and a remainder.
    fn fill_list(&mut self, head: u32) {
        let mut n = head;
        while n != 0 {
            let mut node = n;
            let mut nu = self.u16(node + NODE_NU) as u32;
            n = self.u32(node + NODE_NEXT);
            if nu == 0 {
                continue;
            }
            while nu > 128 {
                self.insert_node(node, NUM_INDEXES as u32 - 1);
                nu -= 128;
                node += 128 * UNIT_SIZE;
            }
            let mut index = u2i(nu);
            if i2u(index) != nu {
                index -= 1;
                let k = i2u(index);
                self.insert_node(node + k * UNIT_SIZE, nu - k - 1);
            }
            self.insert_node(node, index);
        }
    }

    /// `AllocUnitsRare`: glue once per 255 calls, then a larger block split
    /// down, then units stolen from the top of the text region. `None` when
    /// the arena is full (the model restarts).
    #[inline(never)]
    pub(crate) fn alloc_units_rare(&mut self, index: u32) -> Option<u32> {
        if self.glue_count == 0 {
            self.glue_free_blocks();
            if self.has_free(index) {
                return Some(self.remove_node(index));
            }
        }
        let mut i = index;
        loop {
            i += 1;
            if i == NUM_INDEXES as u32 {
                let num_bytes = i2u(index) * UNIT_SIZE;
                let us = self.units_start;
                self.glue_count -= 1;
                return if us - self.text > num_bytes {
                    self.units_start = us - num_bytes;
                    Some(self.units_start)
                } else {
                    None
                };
            }
            if self.has_free(i) {
                break;
            }
        }
        let block = self.remove_node(i);
        self.split_block(block, i, index);
        Some(block)
    }

    /// `AllocUnits`.
    #[inline(always)]
    pub(crate) fn alloc_units(&mut self, index: u32) -> Option<u32> {
        if self.has_free(index) {
            return Some(self.remove_node(index));
        }
        let num_bytes = i2u(index) * UNIT_SIZE;
        let lo = self.lo_unit;
        if self.hi_unit - lo >= num_bytes {
            self.lo_unit = lo + num_bytes;
            return Some(lo);
        }
        self.alloc_units_rare(index)
    }

    /// One unit for a context, from `hi_unit` first (`CreateSuccessors`).
    #[inline(always)]
    pub(crate) fn alloc_context(&mut self) -> Option<u32> {
        if self.hi_unit != self.lo_unit {
            self.hi_unit -= UNIT_SIZE;
            Some(self.hi_unit)
        } else if self.has_free(0) {
            Some(self.remove_node(0))
        } else {
            self.alloc_units_rare(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u32 = 1 << 16;

    #[test]
    fn tables_match_7zip() {
        assert_eq!(
            INDEX2UNITS,
            [
                1, 2, 3, 4, 6, 8, 10, 12, 15, 18, 21, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64,
                68, 72, 76, 80, 84, 88, 92, 96, 100, 104, 108, 112, 116, 120, 124, 128
            ]
        );
        for nu in 1..=128u32 {
            let i = u2i(nu);
            assert!(i2u(i) >= nu);
            assert!(i == 0 || i2u(i - 1) < nu);
        }
    }

    #[test]
    fn layout_matches_ppmd7_restart_model() {
        for size in [2048u32, 2049, 2050, 2051, 1 << 20, (1 << 20) + 7] {
            let a = Arena::new(size);
            let align = (4 - size % 4) % 4;
            assert_eq!(a.text, align);
            assert_eq!((a.text + size) % 4, 0);
            assert_eq!(a.hi_unit, align + size);
            assert_eq!(a.units_start, align + size - size / 8 / 12 * 7 * 12);
            assert_eq!(a.lo_unit, a.units_start);
        }
    }

    #[test]
    fn freed_blocks_are_reused_last_in_first_out() {
        let mut a = Arena::new(SIZE);
        let x = a.alloc_units(1).unwrap();
        let y = a.alloc_units(1).unwrap();
        assert_eq!(y, x + 2 * UNIT_SIZE);
        a.insert_node(x, 1);
        a.insert_node(y, 1);
        assert_eq!(a.alloc_units(1), Some(y));
        assert_eq!(a.alloc_units(1), Some(x));
    }

    #[test]
    fn split_frees_the_tail_in_list_sizes() {
        let mut a = Arena::new(SIZE);
        let block = a.alloc_units(NUM_INDEXES as u32 - 1).unwrap(); // 128 units
        a.split_block(block, NUM_INDEXES as u32 - 1, 0);
        // 127 units left over: a 124-unit block and a 3-unit block.
        assert!(a.has_free(u2i(124)));
        assert!(a.has_free(u2i(3)));
    }

    #[test]
    fn glue_merges_adjacent_free_blocks() {
        let mut a = Arena::new(SIZE);
        // Fill the gap between lo_unit and hi_unit so allocations must come
        // from the free lists.
        let first = a.alloc_units(0).unwrap();
        let second = a.alloc_units(0).unwrap();
        let gap = (a.hi_unit - a.lo_unit) / UNIT_SIZE;
        for _ in 0..gap {
            // A live context's first u16 (NumStats) is nonzero.
            let c = a.alloc_context().unwrap();
            a.set_u16(c, 1);
        }
        assert_eq!(a.lo_unit, a.hi_unit);
        a.insert_node(first, 0);
        a.insert_node(second, 0);
        a.glue_count = 0;
        // A two-unit request finds no two-unit block until the glue pass
        // merges the neighbours.
        assert_eq!(a.alloc_units(1), Some(first));
    }

    #[test]
    fn rare_allocation_steals_from_the_text_region_then_fails() {
        let mut a = Arena::new(SIZE);
        a.lo_unit = a.hi_unit;
        a.glue_count = 1;
        let us = a.units_start;
        assert_eq!(a.alloc_units_rare(0), Some(us - UNIT_SIZE));
        assert_eq!(a.units_start, us - UNIT_SIZE);
        a.text = a.units_start - UNIT_SIZE;
        a.glue_count = 1;
        assert_eq!(a.alloc_units_rare(0), None);
    }

    #[test]
    fn reset_keeps_the_arena_and_its_bytes() {
        let mut a = Arena::new(SIZE);
        let addr = a.arena_addr();
        a.set_u8(a.text + 100, 0xA5);
        a.alloc_units(3).unwrap();
        a.reset();
        assert_eq!(a.arena_addr(), addr);
        assert_eq!(a.u8(a.text + 100), 0xA5);
        assert_eq!(a.lo_unit, a.units_start);
    }
}
