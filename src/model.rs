//! The variant H context model.
//!
//! Dmitry Shkarin's PPMd variant H, as 7-Zip implements it in `Ppmd7.c` and
//! `Ppmd7Dec.c` (Igor Pavlov): contexts and their symbol states, binary
//! contexts with their adaptive `BinSumm` probabilities, secondary escape
//! estimation for masked contexts, model update (`UpdateModel`,
//! `CreateSuccessors`, `Rescale`) and restart when the arena fills.
//!
//! The model decodes through any [`RangeDecoder`] and encodes through any
//! [`RangeEncoder`](crate::rc::RangeEncoder) (`encode.rs`, after 7-Zip's
//! `Ppmd7Enc.c`); it never normalizes the coder itself (see the traits'
//! contracts).
//!
//! Every arena pointer the model follows comes from the stream's own history
//! and so is untrusted: each one is checked against the arena and the text
//! boundary before it is dereferenced, and a pointer that fails the check
//! ends decoding with [`Error::CorruptStream`]. The checks produce
//! validated-span tokens, so a record is checked once and then read
//! field by field without repeating the check.

use crate::alloc::{NodeRef, SubAllocator, UNIT_SIZE, ValidatedArenaOffset, ValidatedArenaSpan};
use crate::error::{Error, Result};
use crate::rc::RangeDecoder;
#[cfg(test)]
use crate::rc::RarRangeDecoder;
use crate::see::SeeTable;
use crate::{PPMD7_MAX_MEM_SIZE, PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE, PPMD7_MIN_ORDER};

mod encode;

// --- Constants ---

const MAX_ORDER: usize = PPMD7_MAX_ORDER as usize;
const MAX_FREQ: u8 = 124;

const BIN_SCALE: u32 = 1 << 14; // 16384
const INTERVAL: u16 = 1 << 7; // 128

const INIT_BIN_ESC: [u16; 8] = [
    0x3CDD, 0x1F3F, 0x59BF, 0x48F3, 0x64A1, 0x5ABC, 0x6632, 0x6051,
];

const EXP_ESCAPE: [u8; 16] = [25, 14, 9, 7, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2, 2];

// --- Context layout (12 bytes per context node) ---
// Contexts are allocated as single units from the arena.

/// Byte offset of suffix context ref (u32, stored as byte offset in arena).
const CTX_SUFFIX: usize = 0;
/// Byte offset of NumStats (u16). NumStats = number of symbols (1 = binary).
const CTX_NUM_STATS: usize = 4;
// Union at offset 6 (6 bytes):
//   NumStats == 1: OneState inline — symbol(1) + freq(1) + successor(4)
//   NumStats > 1: SummFreq(2) + Stats pointer(4)
const CTX_SUMM_FREQ: usize = 6;
const CTX_STATS: usize = 8;
// OneState aliases (same offsets, different interpretation):
const CTX_ONE_SYM: usize = 6;
const CTX_ONE_FREQ: usize = 7;
const CTX_ONE_SUCC: usize = 8;

// --- State layout (6 bytes, packed 2 per 12-byte unit) ---
const STATE_SIZE: usize = 6;
const STATE_SYM: usize = 0;
const STATE_FREQ: usize = 1;
const STATE_SUCC: usize = 2;

#[inline(always)]
const fn pack_unmasked_state(index: usize, head: u16) -> u32 {
    debug_assert!(index <= u8::MAX as usize);
    index as u32 | ((head as u32) << 8)
}

#[inline(always)]
const fn unmasked_state_index(packed: u32) -> usize {
    (packed as u8) as usize
}

#[inline(always)]
const fn unmasked_state_symbol(packed: u32) -> u8 {
    (packed >> 8) as u8
}

#[inline(always)]
const fn unmasked_state_frequency(packed: u32) -> u8 {
    (packed >> 16) as u8
}

/// A PPMd variant H context model with its arena.
///
/// One model decodes one stream; RAR's framing keeps it alive across blocks
/// and members, which is why it is separate from the range decoder.
pub struct Model {
    alloc: SubAllocator,
    see: SeeTable,
    max_order: usize,

    // Context tracking (all stored as byte offsets in arena, 0 = NULL).
    min_context: u32,
    max_context: u32,

    // Found state (byte offset of the matched state, 0 = not found).
    found_state: u32,

    order_fall: i32,

    // Binary summation table [freq-1][combined_index].
    bin_summ: [[u16; 64]; 128],

    // Lookup tables.
    ns2_indx: [u8; 256],
    ns2_bs_indx: [u8; 256],
    hb2_flag: [u8; 256],

    // Mask and counters.
    char_mask: [u8; 256],
    esc_count: u8,
    num_masked: u32,

    // State.
    prev_success: u8,
    /// Symbol of the previous decode's found state (`FoundState->Symbol` at
    /// the top of the next `DecodeChar`). Kept as a byte instead of
    /// re-validating `found_state` on every binary/escape decode: rescale and
    /// update relocate the found state but never change its symbol, so the
    /// returned symbol of decode N IS `FoundState->Symbol` seen by decode
    /// N+1. Restart re-seeds it from the restart-installed found state.
    prev_sym: u8,
    hi_bits_flag: u8,
    init_esc: u8,
    run_length: i32,
    init_rl: i32,
    // Reused packed escape-decode state index/head values. Keeping this on the model avoids
    // clearing a padded 2 KiB `(u32, u8)` array on every masked-context walk.
    unmasked_scratch: [u32; 256],
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    use_ssse3_state_batches: bool,
    model_fault: bool,
    /// Model restarts so far, including the ones a full arena forces.
    #[cfg(test)]
    restarts: u32,
}

// --- Helpers for converting between NodeRef and byte offsets ---

#[inline]
fn ref_to_off(node: NodeRef) -> u32 {
    node.offset() as u32
}

#[inline]
fn off_to_ref(off: u32) -> NodeRef {
    NodeRef(off / UNIT_SIZE as u32)
}

impl Model {
    /// Creates a model of the given order over an arena of `mem_size` bytes.
    ///
    /// `order` must be in [`PPMD7_MIN_ORDER`]`..=`[`PPMD7_MAX_ORDER`] and
    /// `mem_size` in [`PPMD7_MIN_MEM_SIZE`]`..=`[`PPMD7_MAX_MEM_SIZE`];
    /// anything else is [`Error::InvalidParameters`]. The arena is allocated
    /// here, once, and never grows: its length is `mem_size` rounded down to
    /// whole 12-byte units, plus three units, as unrar lays it out.
    pub fn new(order: u32, mem_size: u32) -> Result<Self> {
        let (order, mem_size) = Self::check_parameters(order, mem_size)?;
        let mut model = Self {
            alloc: SubAllocator::new(mem_size),
            see: SeeTable::new(),
            max_order: order,
            min_context: 0,
            max_context: 0,
            found_state: 0,
            order_fall: 0,
            bin_summ: [[0u16; 64]; 128],
            ns2_indx: [0u8; 256],
            ns2_bs_indx: [0u8; 256],
            hb2_flag: [0u8; 256],
            char_mask: [0u8; 256],
            esc_count: 1,
            num_masked: 0,
            prev_success: 0,
            prev_sym: 0,
            hi_bits_flag: 0,
            init_esc: 0,
            run_length: 0,
            init_rl: -(order.min(12) as i32) - 1,
            unmasked_scratch: [0; 256],
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            use_ssse3_state_batches: std::arch::is_x86_feature_detected!("ssse3"),
            model_fault: false,
            #[cfg(test)]
            restarts: 0,
        };
        model.start_checked(order, mem_size);
        Ok(model)
    }

    pub(crate) fn check_parameters(order: u32, mem_size: u32) -> Result<(usize, usize)> {
        if !(PPMD7_MIN_ORDER..=PPMD7_MAX_ORDER).contains(&order)
            || !(PPMD7_MIN_MEM_SIZE..=PPMD7_MAX_MEM_SIZE).contains(&mem_size)
        {
            return Err(Error::InvalidParameters);
        }
        Ok((order as usize, mem_size as usize))
    }

    /// The model order.
    pub fn order(&self) -> u32 {
        self.max_order as u32
    }

    /// The arena size in bytes the model was created or last started with.
    pub fn mem_size(&self) -> u32 {
        self.alloc.allocated_size() as u32
    }

    /// Address of the model arena; see [`SubAllocator::arena_addr`].
    #[cfg(test)]
    pub(crate) fn arena_addr(&self) -> usize {
        self.alloc.arena_addr()
    }

    /// Restarts the model with a new order and arena size, as a fresh
    /// [`Model::new`] would be, but keeping the arena when its size is
    /// unchanged so its pages are not faulted in again.
    pub fn start(&mut self, order: u32, mem_size: u32) -> Result<()> {
        let (order, mem_size) = Self::check_parameters(order, mem_size)?;
        self.start_checked(order, mem_size);
        Ok(())
    }

    fn start_checked(&mut self, max_order: usize, alloc_size: usize) {
        if self.alloc.allocated_size() != alloc_size {
            self.alloc = SubAllocator::new(alloc_size);
        }
        self.max_order = max_order;
        self.esc_count = 1;
        self.model_fault = false;
        self.restart_model();
        self.build_lookup_tables();
    }

    /// Restarts the model from scratch with its current order and arena
    /// size: the state a stream begins in.
    pub fn restart(&mut self) {
        self.start_checked(self.max_order, self.alloc.allocated_size());
    }

    fn build_lookup_tables(&mut self) {
        // NS2BSIndx
        self.ns2_bs_indx[0] = 0;
        self.ns2_bs_indx[1] = 2;
        for i in 2..11 {
            self.ns2_bs_indx[i] = 4;
        }
        for i in 11..256 {
            self.ns2_bs_indx[i] = 6;
        }

        // NS2Indx: 0,1,2 then groups of increasing size
        self.ns2_indx[0] = 0;
        self.ns2_indx[1] = 1;
        self.ns2_indx[2] = 2;
        let mut m = 3u8;
        let mut step = 1usize;
        let mut k = step;
        for i in 3..256 {
            self.ns2_indx[i] = m;
            k -= 1;
            if k == 0 {
                step += 1;
                k = step;
                m += 1;
            }
        }

        // HB2Flag
        for i in 0..0x40 {
            self.hb2_flag[i] = 0;
        }
        for i in 0x40..256 {
            self.hb2_flag[i] = 0x08;
        }
    }

    /// `RestartModel`: empty the arena and rebuild the order-0 context. Used
    /// when the arena fills; unlike [`Self::restart`] it keeps the escape
    /// counter's caller-chosen value.
    fn restart_model(&mut self) {
        #[cfg(test)]
        {
            self.restarts += 1;
        }
        self.char_mask = [0; 256];
        self.alloc.reset();
        self.init_rl = -(self.max_order.min(12) as i32) - 1;

        let root = self.alloc.alloc_context();
        if root.is_null() {
            return;
        }
        let root_off = ref_to_off(root);
        self.min_context = root_off;
        self.max_context = root_off;

        self.alloc.write_u32(root, CTX_SUFFIX, 0);
        self.order_fall = self.max_order as i32;
        self.alloc.write_u16(root, CTX_NUM_STATS, 256);
        self.alloc.write_u16(root, CTX_SUMM_FREQ, 257);

        let states = self.alloc.alloc_units(128);
        if states.is_null() {
            return;
        }
        let states_off = ref_to_off(states);
        self.alloc.write_u32(root, CTX_STATS, states_off);
        self.found_state = states_off;

        self.run_length = self.init_rl;
        self.prev_success = 0;
        self.prev_sym = 0;
        for i in 0..256u32 {
            let off = states_off as usize + i as usize * STATE_SIZE;
            self.alloc.write_byte_at(off + STATE_SYM, i as u8);
            self.alloc.write_byte_at(off + STATE_FREQ, 1);
            self.alloc.write_u32_at(off + STATE_SUCC, 0);
        }

        for i in 0..128u16 {
            for (k, &esc) in INIT_BIN_ESC.iter().enumerate() {
                let val = BIN_SCALE as u16 - esc / (i + 2);
                for m in (0..64).step_by(8) {
                    self.bin_summ[i as usize][k + m] = val;
                }
            }
        }
        self.see = SeeTable::new();
    }

    // --- Context field accessors ---

    #[inline(always)]
    fn validated_context(&self, ctx: u32) -> Option<ValidatedArenaSpan> {
        self.alloc.validated_model_span(ctx, UNIT_SIZE)
    }

    #[inline(always)]
    fn validated_states(&self, stats: u32, count: usize) -> Option<ValidatedArenaSpan> {
        if !(2..=256).contains(&count) {
            return None;
        }
        self.alloc.validated_model_span(stats, count * STATE_SIZE)
    }

    #[inline(always)]
    fn validated_state(&self, state: u32) -> Option<ValidatedArenaSpan> {
        self.alloc.validated_tail_span(state, STATE_SIZE)
    }

    /// Packed context bytes 0..8: suffix, NumStats, and the first two union
    /// bytes (SummFreq or OneState symbol/frequency).
    #[inline(always)]
    fn span_context_head(&self, span: ValidatedArenaSpan) -> u64 {
        self.alloc.span_read_u64(span, 0)
    }

    #[inline(always)]
    fn span_ctx_suffix(&self, span: ValidatedArenaSpan) -> u32 {
        self.alloc.span_read_u32(span, CTX_SUFFIX)
    }

    #[inline(always)]
    fn span_ctx_num_stats(&self, span: ValidatedArenaSpan) -> u16 {
        self.alloc.span_read_u16(span, CTX_NUM_STATS)
    }

    #[inline(always)]
    fn span_ctx_summ_freq(&self, span: ValidatedArenaSpan) -> u16 {
        self.alloc.span_read_u16(span, CTX_SUMM_FREQ)
    }

    #[inline(always)]
    fn span_ctx_stats(&self, span: ValidatedArenaSpan) -> u32 {
        self.alloc.span_read_u32(span, CTX_STATS)
    }

    #[inline(always)]
    fn span_one_sym(&self, span: ValidatedArenaSpan) -> u8 {
        self.alloc.span_read_u8(span, CTX_ONE_SYM)
    }

    #[inline(always)]
    fn span_one_freq(&self, span: ValidatedArenaSpan) -> u8 {
        self.alloc.span_read_u8(span, CTX_ONE_FREQ)
    }

    #[inline(always)]
    fn span_one_succ(&self, span: ValidatedArenaSpan) -> u32 {
        self.alloc.span_read_u32(span, CTX_ONE_SUCC)
    }

    #[inline(always)]
    fn span_set_one_freq(&mut self, span: ValidatedArenaSpan, value: u8) {
        self.alloc.span_write_u8(span, CTX_ONE_FREQ, value);
    }

    #[inline(always)]
    fn span_state_sym(&self, span: ValidatedArenaSpan, index: usize) -> u8 {
        self.alloc
            .span_read_u8(span, index * STATE_SIZE + STATE_SYM)
    }

    #[inline(always)]
    fn span_state_freq(&self, span: ValidatedArenaSpan, index: usize) -> u8 {
        self.alloc
            .span_read_u8(span, index * STATE_SIZE + STATE_FREQ)
    }

    #[inline(always)]
    fn span_state_succ(&self, span: ValidatedArenaSpan, index: usize) -> u32 {
        self.alloc
            .span_read_u32(span, index * STATE_SIZE + STATE_SUCC)
    }

    #[inline(always)]
    fn span_set_state_freq(&mut self, span: ValidatedArenaSpan, index: usize, value: u8) {
        self.alloc
            .span_write_u8(span, index * STATE_SIZE + STATE_FREQ, value);
    }

    #[inline(always)]
    fn span_set_state_sym(&mut self, span: ValidatedArenaSpan, index: usize, value: u8) {
        self.alloc
            .span_write_u8(span, index * STATE_SIZE + STATE_SYM, value);
    }

    #[inline(always)]
    fn span_set_state_succ(&mut self, span: ValidatedArenaSpan, index: usize, value: u32) {
        self.alloc
            .span_write_u32(span, index * STATE_SIZE + STATE_SUCC, value);
    }

    #[inline(always)]
    fn span_write_state(
        &mut self,
        span: ValidatedArenaSpan,
        index: usize,
        symbol: u8,
        frequency: u8,
        successor: u32,
    ) {
        let relative = index * STATE_SIZE;
        let head = u16::from(symbol) | (u16::from(frequency) << 8);
        self.alloc.span_write_u16(span, relative, head);
        self.alloc
            .span_write_u32(span, relative + STATE_SUCC, successor);
    }

    #[inline(always)]
    fn span_copy_state(&mut self, span: ValidatedArenaSpan, dst: usize, src: usize) {
        let head = self.alloc.span_read_u16(span, src * STATE_SIZE);
        let successor = self
            .alloc
            .span_read_u32(span, src * STATE_SIZE + STATE_SUCC);
        self.alloc.span_write_u16(span, dst * STATE_SIZE, head);
        self.alloc
            .span_write_u32(span, dst * STATE_SIZE + STATE_SUCC, successor);
    }

    #[inline(always)]
    fn span_swap_states(&mut self, span: ValidatedArenaSpan, a: usize, b: usize) {
        let a_head = self.alloc.span_read_u16(span, a * STATE_SIZE);
        let a_successor = self.alloc.span_read_u32(span, a * STATE_SIZE + STATE_SUCC);
        let b_head = self.alloc.span_read_u16(span, b * STATE_SIZE);
        let b_successor = self.alloc.span_read_u32(span, b * STATE_SIZE + STATE_SUCC);
        self.alloc.span_write_u16(span, a * STATE_SIZE, b_head);
        self.alloc
            .span_write_u32(span, a * STATE_SIZE + STATE_SUCC, b_successor);
        self.alloc.span_write_u16(span, b * STATE_SIZE, a_head);
        self.alloc
            .span_write_u32(span, b * STATE_SIZE + STATE_SUCC, a_successor);
    }

    #[inline(always)]
    fn span_set_ctx_num_stats(&mut self, span: ValidatedArenaSpan, value: u16) {
        self.alloc.span_write_u16(span, CTX_NUM_STATS, value);
    }

    #[inline(always)]
    fn span_set_ctx_summ_freq(&mut self, span: ValidatedArenaSpan, value: u16) {
        self.alloc.span_write_u16(span, CTX_SUMM_FREQ, value);
    }

    #[inline(always)]
    fn span_set_ctx_stats(&mut self, span: ValidatedArenaSpan, value: u32) {
        self.alloc.span_write_u32(span, CTX_STATS, value);
    }

    #[inline(always)]
    fn span_set_ctx_suffix(&mut self, span: ValidatedArenaSpan, value: u32) {
        self.alloc.span_write_u32(span, CTX_SUFFIX, value);
    }

    #[inline(always)]
    fn span_set_one_sym(&mut self, span: ValidatedArenaSpan, value: u8) {
        self.alloc.span_write_u8(span, CTX_ONE_SYM, value);
    }

    #[inline(always)]
    fn span_set_one_succ(&mut self, span: ValidatedArenaSpan, value: u32) {
        self.alloc.span_write_u32(span, CTX_ONE_SUCC, value);
    }

    #[cfg(test)]
    #[inline]
    fn ctx_num_stats(&self, ctx: u32) -> u16 {
        self.alloc.read_u16_at(ctx as usize + CTX_NUM_STATS)
    }

    #[cfg(test)]
    #[inline]
    fn ctx_summ_freq(&self, ctx: u32) -> u16 {
        self.alloc.read_u16_at(ctx as usize + CTX_SUMM_FREQ)
    }

    #[cfg(test)]
    #[inline]
    fn ctx_stats(&self, ctx: u32) -> u32 {
        self.alloc.read_u32_at(ctx as usize + CTX_STATS)
    }

    #[cfg(test)]
    #[inline]
    fn set_ctx_stats(&mut self, ctx: u32, val: u32) {
        self.alloc.write_u32_at(ctx as usize + CTX_STATS, val);
    }

    // State accessors at arbitrary byte offset.
    #[cfg(test)]
    #[inline]
    fn st_sym(&self, off: u32) -> u8 {
        self.alloc.read_byte_at(off as usize + STATE_SYM)
    }
    #[cfg(test)]
    #[inline]
    fn st_freq(&self, off: u32) -> u8 {
        self.alloc.read_byte_at(off as usize + STATE_FREQ)
    }
    /// Check if a successor value is a text pointer.
    fn is_text_succ(&self, succ: u32) -> bool {
        succ != 0 && (succ as usize) <= self.alloc.text_position()
    }

    #[cold]
    #[inline(never)]
    fn fail_model(&mut self) -> i32 {
        self.model_fault = true;
        -1
    }

    #[cold]
    #[inline(never)]
    fn corrupt_model<T>(detail: &'static str) -> Result<T> {
        Err(Error::CorruptStream { detail })
    }

    // =======================================================================
    // Decode entry point
    // =======================================================================

    /// `DecodeSymbol`: 0-255 on success, -1 at the end marker or on a
    /// fault (`model_fault` tells the two apart).
    fn decode_char<R: RangeDecoder>(&mut self, rc: &mut R) -> i32 {
        let Some(context_span) = self.validated_context(self.min_context) else {
            return self.fail_model();
        };
        let mut active_context_span = context_span;
        let context_head = self.span_context_head(context_span);
        let mut active_context_head = context_head;
        let mut found_span = None;

        let ns = (context_head >> 32) as u16;
        if ns == 0 || ns > 256 {
            return self.fail_model();
        }

        if ns != 1 {
            let stats = self.span_ctx_stats(context_span);
            let Some(states_span) = self.validated_states(stats, ns as usize) else {
                return self.fail_model();
            };
            // Multi-symbol context.
            if !self.decode_symbol1(rc, context_span, states_span, context_head, &mut found_span) {
                return -1;
            }
        } else {
            // Binary context.
            if !self.decode_bin_symbol(rc, context_span, context_head, &mut found_span) {
                return -1;
            }
        }

        // Escape loop: walk suffix chain until a symbol is found.
        let mut validated_suffix: Option<(ValidatedArenaSpan, u64)> = None;
        while found_span.is_none() {
            let (decode_context_span, decode_context_head) = loop {
                self.order_fall += 1;
                let prev_ctx = self.min_context;
                debug_assert_eq!(active_context_span.offset(), prev_ctx as usize);
                let suffix = active_context_head as u32;
                self.min_context = suffix;
                if self.min_context == 0 {
                    return -1;
                }
                let (suffix_span, suffix_head) = if let Some((span, head)) = validated_suffix.take()
                {
                    if span.offset() != self.min_context as usize {
                        return self.fail_model();
                    }
                    (span, head)
                } else {
                    // Validate context pointer.
                    let Some(span) = self.validated_context(self.min_context) else {
                        return self.fail_model();
                    };
                    (span, self.span_context_head(span))
                };
                active_context_span = suffix_span;
                active_context_head = suffix_head;
                let ns2 = (suffix_head >> 32) as u16 as u32;
                if ns2 != self.num_masked {
                    break (suffix_span, suffix_head);
                }
            };
            if !self.decode_symbol2(
                rc,
                decode_context_span,
                decode_context_head,
                &mut found_span,
                &mut validated_suffix,
            ) {
                return -1;
            }
        }

        let Some(found_span) = found_span else {
            return self.fail_model();
        };
        self.next_context(found_span, active_context_span, active_context_head)
    }

    /// `NextContext` / `UpdateModel` after a symbol was coded in the
    /// context `active_context_span`: returns the symbol, or -1 on a model
    /// fault. Shared by the decoder and the encoder.
    #[inline(always)]
    fn next_context(
        &mut self,
        found_span: ValidatedArenaSpan,
        active_context_span: ValidatedArenaSpan,
        active_context_head: u64,
    ) -> i32 {
        let symbol = self.span_state_sym(found_span, 0);

        if self.order_fall == 0 {
            let succ = self.span_state_succ(found_span, 0);
            if succ != 0 && !self.is_text_succ(succ) {
                // Deterministic context jump.
                // The successor is range-checked before dereference at the
                // next decode entry, avoiding the same check twice.
                self.min_context = succ;
                self.max_context = succ;
            } else {
                if !self.update_model(found_span, active_context_span, active_context_head) {
                    return -1;
                }
                if self.esc_count == 0 {
                    self.clear_mask();
                }
            }
        } else {
            if !self.update_model(found_span, active_context_span, active_context_head) {
                return -1;
            }
            if self.esc_count == 0 {
                self.clear_mask();
            }
        }

        self.prev_sym = symbol;
        symbol as i32
    }

    /// Decodes one symbol.
    ///
    /// Returns `Ok(Some(byte))` for a decoded byte and `Ok(None)` for the end
    /// marker (an escape out of the order-0 context; RAR also reads it as the
    /// model giving up on a stream it cannot decode). A model whose arena
    /// pointers or frequencies are inconsistent, or a coder that faulted,
    /// is [`Error::CorruptStream`]; the model must be restarted before it is
    /// used again.
    #[inline(always)]
    pub fn decode_symbol<R: RangeDecoder>(&mut self, rc: &mut R) -> Result<Option<u8>> {
        let ch = self.decode_char(rc);
        if rc.faulted() {
            // A frequency total outran the coder's range: the stream is
            // corrupt whatever symbol the arithmetic then produced.
            self.model_fault = false;
            return Self::corrupt_model("frequency total exceeds the coder's range");
        }
        if ch < 0 {
            if core::mem::take(&mut self.model_fault) {
                return Self::corrupt_model("model pointer or frequency out of bounds");
            }
            Ok(None)
        } else {
            Ok(Some(ch as u8))
        }
    }

    // =======================================================================
    // decode_bin_symbol (NumStats == 1)
    // =======================================================================

    fn decode_bin_symbol<R: RangeDecoder>(
        &mut self,
        rc: &mut R,
        context_span: ValidatedArenaSpan,
        context_head: u64,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        let ctx = self.min_context;
        debug_assert_eq!(context_span.offset(), ctx as usize);
        let symbol = (context_head >> 48) as u8;
        let freq = (context_head >> 56) as u8;
        let Some((idx0, idx1)) = self.bin_summ_index(context_head) else {
            return false;
        };
        let bs = self.bin_summ[idx0][idx1];

        if rc.decode_bit(u32::from(bs)) == 0 {
            self.update_bin_hit(ctx, context_span, freq, (idx0, idx1), found_span);
            true
        } else {
            self.update_bin_escape(symbol, (idx0, idx1), found_span)
        }
    }

    /// `Ppmd7_GetBinSumm`: the `BinSumm` cell a binary context codes against,
    /// setting `HiBitsFlag` from the previous symbol on the way. `None` for
    /// an inconsistent model (`model_fault` set) or an out-of-range
    /// probability. Shared by the decoder and the encoder.
    #[inline(always)]
    fn bin_summ_index(&mut self, context_head: u64) -> Option<(usize, usize)> {
        let symbol = (context_head >> 48) as u8;
        let freq = (context_head >> 56) as u8;

        if self.found_state == 0 {
            self.model_fault = true;
            return None;
        }
        self.hi_bits_flag = self.hb2_flag[self.prev_sym as usize];
        let suffix = context_head as u32;
        if freq == 0 || freq > 128 || suffix == 0 {
            self.model_fault = true;
            return None;
        }
        let Some(suffix_span) = self.validated_context(suffix) else {
            self.model_fault = true;
            return None;
        };
        let suffix_ns = (self.span_context_head(suffix_span) >> 32) as u16;
        if suffix_ns == 0 || suffix_ns > 256 {
            self.model_fault = true;
            return None;
        }
        let idx1 = self.prev_success as usize
            + self.ns2_bs_indx[suffix_ns as usize - 1] as usize
            + self.hi_bits_flag as usize
            + 2 * self.hb2_flag[symbol as usize] as usize
            + ((self.run_length >> 26) as usize & 0x20);
        let idx0 = freq as usize - 1;
        debug_assert!(idx1 < 64);
        if self.bin_summ[idx0][idx1] as u32 > BIN_SCALE {
            return None;
        }
        Some((idx0, idx1))
    }

    /// A binary context's symbol was coded (`UpdateBin` with the `BinSumm`
    /// raise). Shared by the decoder and the encoder.
    #[inline(always)]
    fn update_bin_hit(
        &mut self,
        ctx: u32,
        context_span: ValidatedArenaSpan,
        freq: u8,
        (idx0, idx1): (usize, usize),
        found_span: &mut Option<ValidatedArenaSpan>,
    ) {
        let bs = self.bin_summ[idx0][idx1];
        self.found_state = ctx + CTX_ONE_SYM as u32;
        *found_span = Some(context_span.subspan(CTX_ONE_SYM, STATE_SIZE));
        let new_freq = if freq < 128 { freq + 1 } else { freq };
        self.span_set_one_freq(context_span, new_freq);

        // Update BinSumm: increase probability.
        let mean = ((bs as u32 + 32) >> 7) as u16;
        self.bin_summ[idx0][idx1] = bs.wrapping_add(INTERVAL).wrapping_sub(mean);

        self.prev_success = 1;
        // `RunLength` is only ever compared or shifted (see the `>> 26`
        // index in `decode_bin_symbol`), so the reference's C `int` overflow
        // is benign there but trips Rust's overflow checks. A corrupt stream
        // can hold a binary context for billions of symbols, so saturate
        // instead of wrapping: a wrap would flip the sign bit and silently
        // change the `>> 26` bucket.
        self.run_length = self.run_length.saturating_add(1);
    }

    /// A binary context escaped: lower `BinSumm`, take `InitEsc` and mask
    /// the context's symbol. Shared by the decoder and the encoder.
    #[inline(always)]
    fn update_bin_escape(
        &mut self,
        symbol: u8,
        (idx0, idx1): (usize, usize),
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        let bs = self.bin_summ[idx0][idx1];
        let mean = ((bs as u32 + 32) >> 7) as u16;
        let new_bs = bs.wrapping_sub(mean);
        let Some(&init_esc) = EXP_ESCAPE.get((new_bs >> 10) as usize) else {
            return false;
        };
        self.bin_summ[idx0][idx1] = new_bs;

        self.init_esc = init_esc;
        self.num_masked = 1;
        self.char_mask[symbol as usize] = self.esc_count;
        self.prev_success = 0;
        self.found_state = 0;
        *found_span = None;
        true
    }

    // =======================================================================
    // decode_symbol1 (NumStats > 1)
    // =======================================================================

    fn decode_symbol1<R: RangeDecoder>(
        &mut self,
        rc: &mut R,
        context_span: ValidatedArenaSpan,
        states_span: ValidatedArenaSpan,
        context_head: u64,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        let ctx = self.min_context;
        debug_assert_eq!(context_span.offset(), ctx as usize);
        let ns = (context_head >> 32) as u16 as usize;
        let sum_freq = (context_head >> 48) as u16 as u32;
        let stats = self.span_ctx_stats(context_span);
        debug_assert_eq!(states_span.offset(), stats as usize);
        debug_assert_eq!(states_span.len(), ns * STATE_SIZE);

        if sum_freq == 0 {
            self.model_fault = true;
            return false;
        }
        let count = rc.get_threshold(sum_freq);
        if count >= sum_freq {
            return false;
        }

        // Check first symbol.
        let p0_freq = self.span_state_freq(states_span, 0) as u32;
        if count < p0_freq {
            // First symbol matched.
            let model_valid = self.update1_0(
                ctx,
                context_span,
                states_span,
                p0_freq,
                sum_freq,
                found_span,
            );
            rc.decode(0, p0_freq);
            return model_valid;
        }

        if self.found_state == 0 {
            return false;
        }

        self.prev_success = 0;
        let mut hi_cnt = p0_freq;
        let mut remaining = ns - 1;
        let mut state_index = 1usize;

        loop {
            let p_freq = self.span_state_freq(states_span, state_index) as u32;
            hi_cnt += p_freq;
            if hi_cnt > count {
                // Found a symbol.
                let low = hi_cnt - p_freq;
                rc.decode(low, p_freq);
                return self.update1(
                    ctx,
                    context_span,
                    states_span,
                    state_index,
                    p_freq as u8,
                    found_span,
                );
            }
            remaining -= 1;
            if remaining == 0 {
                self.hi_bits_flag = self.hb2_flag[self.prev_sym as usize];
                self.num_masked = ns as u32;
                self.found_state = 0;
                *found_span = None;

                for index in (0..ns).rev() {
                    let sym = self.span_state_sym(states_span, index);
                    self.char_mask[sym as usize] = self.esc_count;
                }

                let escape_freq = sum_freq - hi_cnt;
                rc.decode(hi_cnt, escape_freq);
                return true;
            }
            state_index += 1;
        }
    }

    /// `Update1_0`: the first state of a multi-symbol context was coded.
    /// Shared by the decoder and the encoder.
    #[inline(always)]
    fn update1_0(
        &mut self,
        ctx: u32,
        context_span: ValidatedArenaSpan,
        states_span: ValidatedArenaSpan,
        p0_freq: u32,
        sum_freq: u32,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        self.prev_success = if 2 * p0_freq > sum_freq { 1 } else { 0 };
        // Same saturation rationale as `decode_bin_symbol`; see there.
        self.run_length = self.run_length.saturating_add(self.prev_success as i32);
        self.found_state = states_span.offset() as u32;
        *found_span = Some(states_span.subspan(0, STATE_SIZE));

        // unrar's model.cpp:420-423 stores the wrapped byte into `Freq` but keeps
        // comparing the un-truncated `int HiCnt` against MAX_FREQ, so a
        // corrupt state with `Freq >= 252` still rescales.
        let raised_freq = p0_freq + 4;
        let new_freq = raised_freq as u8;
        let needs_rescale = raised_freq > MAX_FREQ as u32;
        self.span_set_state_freq(states_span, 0, new_freq);
        self.span_set_ctx_summ_freq(context_span, (sum_freq + 4) as u16);

        let model_valid = !needs_rescale || self.rescale(ctx);
        if needs_rescale {
            *found_span = self.validated_state(self.found_state);
        }
        model_valid
    }

    /// update1: increase freq, maintain sorted order, rescale if needed.
    #[inline(always)]
    fn update1(
        &mut self,
        ctx: u32,
        context_span: ValidatedArenaSpan,
        states_span: ValidatedArenaSpan,
        state_index: usize,
        state_freq: u8,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        debug_assert!(state_index < self.span_ctx_num_stats(context_span) as usize);
        let p = self.span_ctx_stats(context_span) + state_index as u32 * STATE_SIZE as u32;
        self.found_state = p;
        let new_freq = state_freq.wrapping_add(4);
        self.span_set_state_freq(states_span, state_index, new_freq);

        let sf = self.span_ctx_summ_freq(context_span);
        self.span_set_ctx_summ_freq(context_span, sf.wrapping_add(4));

        let mut found_index = state_index;
        if state_index > 0 {
            let prev = p - STATE_SIZE as u32;
            if new_freq > self.span_state_freq(states_span, state_index - 1) {
                self.span_swap_states(states_span, state_index, state_index - 1);
                self.found_state = prev;
                found_index -= 1;
                if self.span_state_freq(states_span, state_index - 1) > MAX_FREQ {
                    let model_valid = self.rescale(ctx);
                    *found_span = self.validated_state(self.found_state);
                    return model_valid && found_span.is_some();
                }
            }
        }
        *found_span = Some(states_span.subspan(found_index * STATE_SIZE, STATE_SIZE));
        true
    }

    // =======================================================================
    // decode_symbol2 (masked context decode during escape)
    // =======================================================================

    fn decode_symbol2<R: RangeDecoder>(
        &mut self,
        rc: &mut R,
        context_span: ValidatedArenaSpan,
        context_head: u64,
        found_span: &mut Option<ValidatedArenaSpan>,
        validated_suffix: &mut Option<(ValidatedArenaSpan, u64)>,
    ) -> bool {
        *validated_suffix = None;
        let ctx = self.min_context;
        debug_assert_eq!(context_span.offset(), ctx as usize);
        let ns = (context_head >> 32) as u16 as u32;
        let stats = self.span_ctx_stats(context_span);
        let Some(states_span) = self.validated_states(stats, ns as usize) else {
            self.model_fault = true;
            return false;
        };
        let suffix = context_head as u32;
        let suffix_data = if ns != 256 {
            if suffix == 0 {
                self.model_fault = true;
                return false;
            }
            let Some(span) = self.validated_context(suffix) else {
                self.model_fault = true;
                return false;
            };
            Some((span, self.span_context_head(span)))
        } else {
            None
        };
        let Some(diff) = ns.checked_sub(self.num_masked) else {
            return false;
        };
        if diff == 0 {
            return false;
        }

        // makeEscFreq2
        let suffix_ns = suffix_data.map_or(0, |(_, head)| (head >> 32) as u16 as u32);
        // `Suffix->NumStats-NumStats` in unrar's model.cpp:474 is signed int arithmetic:
        // a suffix with fewer stats than this context is not a fault, it just
        // makes `Diff < Suffix->NumStats-NumStats` false. Only an out-of-range
        // stat count is rejected.
        if ns != 256 && suffix_ns > 256 {
            self.model_fault = true;
            return false;
        }
        let (esc_freq, see_index) = self.make_esc_freq2(context_head, suffix_ns, diff);
        let n = diff as usize;

        // Two passes, like 7-Zip's Ppmd7: the first only sums the unmasked
        // frequencies, and the states are walked again only to select the
        // decoded one or to mask them on escape. The walk has no
        // data-dependent branch (whether a symbol is masked is close to a
        // coin flip on escape-heavy input).
        let mut hi_cnt = 0u32;
        let esc_count = self.esc_count;
        let alloc = &self.alloc;
        let char_mask = &self.char_mask;
        let mut found = 0usize;
        for state_index in 0..ns as usize {
            let head = alloc.span_read_u16(states_span, state_index * STATE_SIZE);
            let unmasked = char_mask[head as u8 as usize] != esc_count;
            hi_cnt += u32::from(head >> 8) & 0u32.wrapping_sub(u32::from(unmasked));
            found += usize::from(unmasked);
        }
        // A consistent model has exactly `ns - num_masked` unmasked states.
        // A corrupt one keeps the reference's shape: too few is a failed
        // decode, and with too many only the first `n` count, so that case
        // gathers them into the scratch array.
        let consistent = found == n;
        if !consistent {
            if found < n {
                return false;
            }
            let scratch = &mut self.unmasked_scratch;
            let mut kept = 0usize;
            for state_index in 0..ns as usize {
                let head = alloc.span_read_u16(states_span, state_index * STATE_SIZE);
                if char_mask[head as u8 as usize] != esc_count {
                    scratch[kept] = pack_unmasked_state(state_index, head);
                    kept += 1;
                    if kept == n {
                        break;
                    }
                }
            }
            hi_cnt = scratch[..n]
                .iter()
                .map(|&packed| u32::from(unmasked_state_frequency(packed)))
                .sum();
        }
        let scale = esc_freq + hi_cnt;
        let count = rc.get_threshold(scale);
        if count >= scale {
            return false;
        }

        if count < hi_cnt {
            // Symbol found among unmasked. `count < hi_cnt` guarantees a
            // selection: the unmasked frequencies sum to `hi_cnt`.
            let mut selected = None;
            if consistent {
                let mut rest = count;
                for state_index in 0..ns as usize {
                    let head = alloc.span_read_u16(states_span, state_index * STATE_SIZE);
                    let unmasked = char_mask[head as u8 as usize] != esc_count;
                    let freq = u32::from(head >> 8) & 0u32.wrapping_sub(u32::from(unmasked));
                    if rest < freq {
                        selected = Some((state_index, (head >> 8) as u8, count - rest));
                        break;
                    }
                    rest -= freq;
                }
            } else {
                let mut cum = 0u32;
                for &packed in &self.unmasked_scratch[..n] {
                    let state_index = unmasked_state_index(packed);
                    let state_freq = unmasked_state_frequency(packed);
                    let freq = state_freq as u32;
                    cum += freq;
                    if cum > count {
                        selected = Some((state_index, state_freq, cum - freq));
                        break;
                    }
                }
            }
            if let Some((state_index, state_freq, low)) = selected {
                rc.decode(low, u32::from(state_freq));
                // SEE update (success).
                self.see_update_success(see_index);
                return self.update2(
                    ctx,
                    context_span,
                    states_span,
                    state_index,
                    state_freq,
                    found_span,
                );
            }
        }

        // Escape again.
        rc.decode(hi_cnt, esc_freq);

        // SEE update (escape): add scale to summ.
        self.see_update_escape(see_index, scale);

        // Mask remaining unmasked symbols. In a consistent model every state
        // is either already masked or one of them, so stamping all of them
        // is the same and needs no mask check.
        let char_mask = &mut self.char_mask;
        if consistent {
            for state_index in 0..ns as usize {
                let sym = self
                    .alloc
                    .span_read_u8(states_span, state_index * STATE_SIZE + STATE_SYM);
                char_mask[sym as usize] = esc_count;
            }
        } else {
            for &packed in &self.unmasked_scratch[..n] {
                let sym = unmasked_state_symbol(packed);
                char_mask[sym as usize] = esc_count;
            }
        }
        self.num_masked = ns;
        *validated_suffix = suffix_data;

        true // escape: FoundState stays NULL and decode_char continues down the suffix chain
    }

    #[inline(always)]
    fn make_esc_freq2(
        &mut self,
        context_head: u64,
        suffix_ns: u32,
        diff: u32,
    ) -> (u32, Option<(usize, usize)>) {
        let ns = (context_head >> 32) as u16 as u32;
        if ns != 256 {
            debug_assert!((1..=256).contains(&diff));
            let sf = (context_head >> 48) as u16 as u32;
            let idx0 = self.ns2_indx[diff as usize - 1] as usize;
            // Signed, like model.cpp:474 — `suffix_ns < ns` yields a negative
            // right-hand side and the comparison is simply false.
            let suffix_excess = i64::from(suffix_ns) - i64::from(ns);
            let idx1 = (if i64::from(diff) < suffix_excess {
                1
            } else {
                0
            }) + (if sf < 11 * ns { 2 } else { 0 })
                + (if self.num_masked > diff { 4 } else { 0 })
                + self.hi_bits_flag as usize;
            let see_ctx = self.see.get(idx0, idx1);
            (see_ctx.get_mean(), Some((idx0, idx1)))
        } else {
            (1, None)
        }
    }

    #[inline(always)]
    fn see_update_success(&mut self, see_index: Option<(usize, usize)>) {
        if let Some((idx0, idx1)) = see_index {
            self.see.get(idx0, idx1).update();
        }
    }

    #[inline(always)]
    fn see_update_escape(&mut self, see_index: Option<(usize, usize)>, scale: u32) {
        if let Some((idx0, idx1)) = see_index {
            let see = self.see.get(idx0, idx1);
            see.summ = see.summ.wrapping_add(scale as u16);
        } else {
            let dummy = self.see.get_dummy();
            dummy.summ = dummy.summ.wrapping_add(scale as u16);
        }
    }

    /// update2: set FoundState, increase freq, maybe rescale.
    #[inline(always)]
    fn update2(
        &mut self,
        ctx: u32,
        context_span: ValidatedArenaSpan,
        states_span: ValidatedArenaSpan,
        state_index: usize,
        freq: u8,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        debug_assert!(state_index < self.span_ctx_num_stats(context_span) as usize);
        let p = self.span_ctx_stats(context_span) + state_index as u32 * STATE_SIZE as u32;
        self.found_state = p;
        let new_freq = freq.wrapping_add(4);
        self.span_set_state_freq(states_span, state_index, new_freq);

        let sf = self.span_ctx_summ_freq(context_span);
        self.span_set_ctx_summ_freq(context_span, sf.wrapping_add(4));
        if new_freq > MAX_FREQ {
            if !self.rescale(ctx) {
                return false;
            }
            *found_span = self.validated_state(self.found_state);
            if found_span.is_none() {
                self.model_fault = true;
                return false;
            }
        } else {
            *found_span = Some(states_span.subspan(state_index * STATE_SIZE, STATE_SIZE));
        }
        self.esc_count = self.esc_count.wrapping_add(1);
        self.run_length = self.init_rl;
        true
    }

    // =======================================================================
    // rescale
    // =======================================================================

    fn rescale(&mut self, ctx: u32) -> bool {
        let Some(context_span) = self.validated_context(ctx) else {
            self.model_fault = true;
            return false;
        };
        let old_ns = self.span_ctx_num_stats(context_span) as usize;
        let stats = self.span_ctx_stats(context_span);
        let Some(states_span) = self.validated_states(stats, old_ns) else {
            self.model_fault = true;
            return false;
        };
        let adder: u8 = if self.order_fall != 0 { 1 } else { 0 };

        // Move FoundState to front.
        let Some(found_delta) = self.found_state.checked_sub(stats) else {
            self.model_fault = true;
            return false;
        };
        if !(found_delta as usize).is_multiple_of(STATE_SIZE) {
            self.model_fault = true;
            return false;
        }
        let mut found_index = found_delta as usize / STATE_SIZE;
        if found_index >= old_ns {
            self.model_fault = true;
            return false;
        }
        while found_index != 0 {
            self.span_swap_states(states_span, found_index, found_index - 1);
            found_index -= 1;
        }

        // Boost first state.
        let f0 = self.span_state_freq(states_span, 0);
        let new_f0 = f0.wrapping_add(4);
        self.span_set_state_freq(states_span, 0, new_f0);
        let sf0 = self.span_ctx_summ_freq(context_span);
        self.span_set_ctx_summ_freq(context_span, sf0.wrapping_add(4));

        // Halve frequencies, accumulate escape frequency.
        let mut esc_freq = self.span_ctx_summ_freq(context_span) as i32
            - self.span_state_freq(states_span, 0) as i32;
        let first_freq = ((self.span_state_freq(states_span, 0) as u16 + adder as u16) >> 1) as u8;
        self.span_set_state_freq(states_span, 0, first_freq);
        self.span_set_ctx_summ_freq(context_span, first_freq as u16);

        for state_index in 1..old_ns {
            esc_freq -= self.span_state_freq(states_span, state_index) as i32;
            let halved =
                ((self.span_state_freq(states_span, state_index) as u16 + adder as u16) >> 1) as u8;
            self.span_set_state_freq(states_span, state_index, halved);
            let summ = self.span_ctx_summ_freq(context_span);
            self.span_set_ctx_summ_freq(context_span, summ.wrapping_add(halved as u16));

            // Maintain sorted order.
            if halved > self.span_state_freq(states_span, state_index - 1) {
                // Bubble up.
                let tmp_sym = self.span_state_sym(states_span, state_index);
                let tmp_freq = halved;
                let tmp_succ = self.span_state_succ(states_span, state_index);
                let mut dst = state_index;
                loop {
                    self.span_copy_state(states_span, dst, dst - 1);
                    dst -= 1;
                    if dst == 0 || tmp_freq <= self.span_state_freq(states_span, dst - 1) {
                        break;
                    }
                }
                self.span_set_state_sym(states_span, dst, tmp_sym);
                self.span_set_state_freq(states_span, dst, tmp_freq);
                self.span_set_state_succ(states_span, dst, tmp_succ);
            }
        }

        // Remove zero-frequency states.
        let mut last_index = old_ns - 1;
        if self.span_state_freq(states_span, last_index) == 0 {
            let mut zero_count = 0usize;
            while self.span_state_freq(states_span, last_index) == 0 && last_index > 0 {
                zero_count += 1;
                last_index -= 1;
            }
            if self.span_state_freq(states_span, last_index) == 0 {
                zero_count += 1;
            }
            esc_freq += zero_count as i32;
            let new_ns = (old_ns - zero_count) as u16;
            self.span_set_ctx_num_stats(context_span, new_ns);
            if new_ns == 0 || esc_freq < 0 {
                self.model_fault = true;
                return false;
            }

            if new_ns == 1 {
                // Collapse to single-state (OneState) context.
                let tmp_sym = self.span_state_sym(states_span, 0);
                let tmp_freq = self.span_state_freq(states_span, 0);
                let tmp_succ = self.span_state_succ(states_span, 0);

                // Halve freq until escape is small.
                let mut tf = tmp_freq;
                let mut ef = esc_freq;
                loop {
                    tf -= tf >> 1;
                    ef >>= 1;
                    if ef <= 1 {
                        break;
                    }
                }

                // Free the stats array.
                self.alloc.free_units(off_to_ref(stats), (old_ns + 1) >> 1);

                // Write OneState inline.
                self.span_set_one_sym(context_span, tmp_sym);
                self.span_set_one_freq(context_span, tf);
                self.span_set_one_succ(context_span, tmp_succ);
                self.found_state = ctx + CTX_ONE_SYM as u32;
                return true;
            }
        }

        if esc_freq < 0 {
            self.model_fault = true;
            return false;
        }
        let esc_freq = esc_freq as u16;
        let summ = self.span_ctx_summ_freq(context_span);
        self.span_set_ctx_summ_freq(
            context_span,
            summ.wrapping_add(esc_freq.wrapping_sub(esc_freq >> 1)),
        );

        // Shrink stats array if needed.
        let n0 = (old_ns + 1) >> 1;
        let new_ns = self.span_ctx_num_stats(context_span) as usize;
        if new_ns == 0 {
            self.model_fault = true;
            return false;
        }
        let n1 = (new_ns + 1) >> 1;
        let mut new_stats = stats;
        if n0 != n1 {
            new_stats = ref_to_off(self.alloc.shrink_units(off_to_ref(stats), n0, n1));
            self.span_set_ctx_stats(context_span, new_stats);
        }
        self.found_state = new_stats;
        if self.validated_state(new_stats).is_none() {
            self.model_fault = true;
            return false;
        }
        true
    }

    // =======================================================================
    // UpdateModel
    // =======================================================================

    #[inline(never)]
    fn update_model(
        &mut self,
        found_span: ValidatedArenaSpan,
        min_context_span: ValidatedArenaSpan,
        min_context_head: u64,
    ) -> bool {
        debug_assert_eq!(min_context_span.offset(), self.min_context as usize);
        let fs_sym = self.span_state_sym(found_span, 0);
        let fs_freq = self.span_state_freq(found_span, 0);
        let fs_succ = self.span_state_succ(found_span, 0);

        // Update suffix context frequencies.
        let suffix = min_context_head as u32;
        if fs_freq < MAX_FREQ / 4 && suffix != 0 {
            let Some(suffix_span) = self.validated_context(suffix) else {
                self.model_fault = true;
                return false;
            };
            let sns = self.span_ctx_num_stats(suffix_span);
            if sns != 1 {
                // Find fs_sym in suffix stats.
                let s_stats = self.span_ctx_stats(suffix_span);
                let Some(suffix_states) = self.validated_states(s_stats, sns as usize) else {
                    self.model_fault = true;
                    return false;
                };
                let mut state_index = 0usize;
                if self.span_state_sym(suffix_states, state_index) != fs_sym {
                    let Some(found_index) =
                        self.span_find_state_from(suffix_states, 1, sns as usize, fs_sym)
                    else {
                        self.model_fault = true;
                        return false;
                    };
                    state_index = found_index;
                    // Swap with predecessor if freq is higher.
                    if self.span_state_freq(suffix_states, state_index)
                        >= self.span_state_freq(suffix_states, state_index - 1)
                    {
                        self.span_swap_states(suffix_states, state_index, state_index - 1);
                        state_index -= 1;
                    }
                }
                if self.span_state_freq(suffix_states, state_index) < MAX_FREQ - 9 {
                    let f = self.span_state_freq(suffix_states, state_index) + 2;
                    self.span_set_state_freq(suffix_states, state_index, f);
                    let sf = self.span_ctx_summ_freq(suffix_span);
                    self.span_set_ctx_summ_freq(suffix_span, sf.wrapping_add(2));
                }
                let p = s_stats + state_index as u32 * STATE_SIZE as u32;
                self.do_update_model_core(found_span, min_context_span, fs_sym, fs_freq, fs_succ, p)
            } else {
                // Suffix is binary context.
                let f = self.span_one_freq(suffix_span);
                if f < 32 {
                    self.span_set_one_freq(suffix_span, f + 1);
                }
                self.do_update_model_core(
                    found_span,
                    min_context_span,
                    fs_sym,
                    fs_freq,
                    fs_succ,
                    suffix + CTX_ONE_SYM as u32,
                )
            }
        } else {
            self.do_update_model_core(found_span, min_context_span, fs_sym, fs_freq, fs_succ, 0)
        }
    }

    /// Core of UpdateModel after suffix freq update.
    /// `p1` is the state offset in the suffix context (0 if none).
    #[inline(never)]
    fn do_update_model_core(
        &mut self,
        found_span: ValidatedArenaSpan,
        min_context_span: ValidatedArenaSpan,
        fs_sym: u8,
        fs_freq: u8,
        fs_succ: u32,
        p1: u32,
    ) -> bool {
        let mut next_min_context = fs_succ;

        if self.order_fall == 0 {
            // No escape: create successors.
            let new_ctx = self.create_successors(found_span, min_context_span, true, p1);
            if new_ctx == 0 {
                if self.model_fault {
                    return false;
                }
                self.restart_model();
                self.esc_count = 0;
                return true;
            }
            self.min_context = new_ctx;
            self.max_context = new_ctx;
            // Update found state's successor.
            self.span_set_state_succ(found_span, 0, new_ctx);
            return true;
        }

        // OrderFall > 0: store symbol in text region and propagate.
        self.alloc.write_text_byte(fs_sym);
        let successor = self.alloc.text_position() as u32;
        if self.alloc.text_exhausted() {
            self.restart_model();
            self.esc_count = 0;
            return true;
        }

        let final_succ;
        if fs_succ != 0 {
            // Existing successor — may need to create real contexts from text chain.
            if self.is_text_succ(fs_succ) {
                let new_succ = self.create_successors(found_span, min_context_span, false, p1);
                if new_succ == 0 {
                    if self.model_fault {
                        return false;
                    }
                    self.restart_model();
                    self.esc_count = 0;
                    return true;
                }
                self.span_set_state_succ(found_span, 0, new_succ);
                next_min_context = new_succ;
            }
            self.order_fall -= 1;
            if self.order_fall == 0 {
                final_succ = self.span_state_succ(found_span, 0);
                if self.max_context != self.min_context {
                    self.alloc.text_dec();
                }
            } else {
                final_succ = successor;
            }
        } else {
            // No successor yet: set text pointer as successor.
            self.span_set_state_succ(found_span, 0, successor);
            final_succ = successor;
            // fs.Successor becomes the current MinContext even though the live
            // FoundState successor now points into the text buffer.
            next_min_context = self.min_context;
        }

        let min_ctx = self.min_context;
        debug_assert_eq!(min_context_span.offset(), min_ctx as usize);
        let ns = self.span_ctx_num_stats(min_context_span) as u32;
        let s0 = (self.span_ctx_summ_freq(min_context_span) as u32)
            .wrapping_sub(ns)
            .wrapping_sub(fs_freq as u32)
            .wrapping_add(1);

        let mut pc = self.max_context;
        while pc != min_ctx {
            let Some(context_span) = self.validated_context(pc) else {
                self.model_fault = true;
                return false;
            };
            let ns1 = self.span_ctx_num_stats(context_span) as u32;
            if ns1 == 0 || ns1 > 256 {
                self.model_fault = true;
                return false;
            }

            let states_span = if ns1 != 1 {
                // Multi-symbol context: expand stats array if needed.
                let old_stats = self.span_ctx_stats(context_span);
                let stats = if (ns1 & 1) == 0 {
                    let new_stats = self
                        .alloc
                        .expand_units(off_to_ref(old_stats), (ns1 >> 1) as usize);
                    if new_stats.is_null() {
                        self.restart_model();
                        self.esc_count = 0;
                        return true;
                    }
                    let stats = ref_to_off(new_stats);
                    self.span_set_ctx_stats(context_span, stats);
                    stats
                } else {
                    old_stats
                };
                let Some(states_span) = self.validated_states(stats, ns1 as usize + 1) else {
                    self.model_fault = true;
                    return false;
                };
                // Adjust SummFreq.
                let sf = self.span_ctx_summ_freq(context_span) as u32;
                let adj = (if 2 * ns1 < ns { 1u32 } else { 0 })
                    + 2 * (if 4 * ns1 <= ns && sf <= 8 * ns1 { 1 } else { 0 });
                self.span_set_ctx_summ_freq(context_span, (sf + adj) as u16);
                states_span
            } else {
                // Single-state: promote to multi-state.
                let os_sym = self.span_one_sym(context_span);
                let os_freq = self.span_one_freq(context_span);
                let os_succ = self.span_one_succ(context_span);
                let new_stats_ref = self.alloc.alloc_units(1);
                if new_stats_ref.is_null() {
                    self.restart_model();
                    self.esc_count = 0;
                    return true;
                }
                let new_stats = ref_to_off(new_stats_ref);
                // Copy OneState to the new stats array.
                let Some(new_states_span) = self.validated_states(new_stats, 2) else {
                    self.model_fault = true;
                    return false;
                };
                self.span_write_state(new_states_span, 0, os_sym, os_freq, os_succ);
                self.span_set_ctx_stats(context_span, new_stats);
                let adj_freq = if os_freq < MAX_FREQ / 4 - 1 {
                    os_freq * 2
                } else {
                    MAX_FREQ - 4
                };
                self.span_set_state_freq(new_states_span, 0, adj_freq);
                self.span_set_ctx_summ_freq(
                    context_span,
                    adj_freq as u16 + self.init_esc as u16 + if ns > 3 { 1 } else { 0 },
                );
                new_states_span
            };

            // Compute new state's frequency.
            let sf_pc = self.span_ctx_summ_freq(context_span) as u32;
            let cf = 2 * fs_freq as u32 * (sf_pc + 6);
            let sf = s0 + sf_pc;
            let new_freq;
            if cf < 6 * sf {
                new_freq = 1 + (if cf > sf { 1 } else { 0 }) + (if cf >= 4 * sf { 1 } else { 0 });
                let new_sf = sf_pc + 3;
                self.span_set_ctx_summ_freq(context_span, new_sf as u16);
            } else {
                new_freq = 4
                    + (if cf >= 9 * sf { 1 } else { 0 })
                    + (if cf >= 12 * sf { 1 } else { 0 })
                    + (if cf >= 15 * sf { 1 } else { 0 });
                let new_sf = sf_pc + new_freq;
                self.span_set_ctx_summ_freq(context_span, new_sf as u16);
            }

            self.span_write_state(
                states_span,
                ns1 as usize,
                fs_sym,
                new_freq as u8,
                final_succ,
            );
            self.span_set_ctx_num_stats(context_span, (ns1 + 1) as u16);

            pc = self.span_ctx_suffix(context_span);
        }

        self.max_context = next_min_context;
        self.min_context = next_min_context;
        true
    }

    // =======================================================================
    // CreateSuccessors
    // =======================================================================

    #[inline(never)]
    fn create_successors(
        &mut self,
        found_span: ValidatedArenaSpan,
        min_context_span: ValidatedArenaSpan,
        skip: bool,
        p1: u32,
    ) -> u32 {
        debug_assert_eq!(min_context_span.offset(), self.min_context as usize);
        let up_branch = self.span_state_succ(found_span, 0);
        let found_sym = self.span_state_sym(found_span, 0);
        let min_suffix = self.span_ctx_suffix(min_context_span);

        let mut pc = self.min_context;
        let mut ps = [found_span.compact(); MAX_ORDER];
        let mut ps_len = 0usize;

        if !skip {
            ps[ps_len] = found_span.compact();
            ps_len += 1;
            if min_suffix == 0 {
                // NO_LOOP
                return self.finish_create_successors(&ps[..ps_len], pc, up_branch);
            }
        }

        if p1 != 0 {
            // p1 provided: use it and start from suffix.
            pc = min_suffix;
            let Some(p1_span) = self.validated_state(p1) else {
                self.model_fault = true;
                return 0;
            };
            let p1_succ = self.span_state_succ(p1_span, 0);
            // Check if p1's successor matches up_branch.
            if p1_succ != up_branch {
                pc = p1_succ;
                return self.finish_create_successors(&ps[..ps_len], pc, up_branch);
            }
            if ps_len >= MAX_ORDER {
                return 0;
            }
            ps[ps_len] = p1_span.compact();
            ps_len += 1;
            // Fall through to suffix walk.
            let Some(context_span) = self.validated_context(pc) else {
                self.model_fault = true;
                return 0;
            };
            let suffix = self.span_ctx_suffix(context_span);
            if suffix == 0 {
                // No more suffix to walk.
                return self.finish_create_successors(&ps[..ps_len], pc, up_branch);
            }
            pc = suffix;
        } else {
            if min_suffix == 0 {
                return self.finish_create_successors(&ps[..ps_len], pc, up_branch);
            }
            pc = min_suffix;
        }

        // Walk suffix chain.
        loop {
            let Some(context_span) = self.validated_context(pc) else {
                self.model_fault = true;
                return 0;
            };
            let ns = self.span_ctx_num_stats(context_span);
            let (p_span, p_succ);
            if ns != 1 {
                // Multi-symbol: find our symbol.
                let stats = self.span_ctx_stats(context_span);
                let Some(states_span) = self.validated_states(stats, ns as usize) else {
                    self.model_fault = true;
                    return 0;
                };
                let Some(state_index) = self.span_find_state(states_span, ns as usize, found_sym)
                else {
                    self.model_fault = true;
                    return 0;
                };
                p_span = states_span.subspan(state_index * STATE_SIZE, STATE_SIZE);
                p_succ = self.span_state_succ(states_span, state_index);
            } else {
                // Binary context.
                p_span = context_span.subspan(CTX_ONE_SYM, STATE_SIZE);
                p_succ = self.span_one_succ(context_span);
            }

            if p_succ != up_branch {
                pc = p_succ;
                break;
            }
            if ps_len >= MAX_ORDER {
                return 0;
            }
            ps[ps_len] = p_span.compact();
            ps_len += 1;

            let suffix = self.span_ctx_suffix(context_span);
            if suffix == 0 {
                break;
            }
            pc = suffix;
        }

        self.finish_create_successors(&ps[..ps_len], pc, up_branch)
    }

    #[inline(always)]
    fn finish_create_successors(
        &mut self,
        ps: &[ValidatedArenaOffset],
        mut pc: u32,
        up_branch: u32,
    ) -> u32 {
        if ps.is_empty() {
            return pc;
        }

        // Read the symbol and successor from the text chain (UpBranch).
        if up_branch == 0 || !self.is_text_succ(up_branch) {
            self.model_fault = true;
            return 0;
        }
        let up_sym = self.alloc.read_byte_at(up_branch as usize);
        let up_succ = up_branch + 1;

        // Determine the frequency for the new state.
        let Some(context_span) = self.validated_context(pc) else {
            self.model_fault = true;
            return 0;
        };
        let up_freq;
        let ns_pc = self.span_ctx_num_stats(context_span);
        if ns_pc != 1 {
            let stats = self.span_ctx_stats(context_span);
            let Some(states_span) = self.validated_states(stats, ns_pc as usize) else {
                self.model_fault = true;
                return 0;
            };
            let Some(state_index) = self.span_find_state(states_span, ns_pc as usize, up_sym)
            else {
                self.model_fault = true;
                return 0;
            };
            let Some(cf) = self
                .span_state_freq(states_span, state_index)
                .checked_sub(1)
                .map(u32::from)
            else {
                self.model_fault = true;
                return 0;
            };
            let Some(s0) = (self.span_ctx_summ_freq(context_span) as u32)
                .checked_sub(ns_pc as u32)
                .and_then(|summ| summ.checked_sub(cf))
            else {
                self.model_fault = true;
                return 0;
            };
            up_freq = if 2 * cf <= s0 {
                1 + u8::from(5 * cf > s0)
            } else {
                if s0 == 0 {
                    self.model_fault = true;
                    return 0;
                }
                (1 + ((2 * cf + 3 * s0 - 1) / (2 * s0))) as u8
            };
        } else {
            up_freq = self.span_one_freq(context_span);
        }

        // Create child contexts from ps (in reverse order).
        for &state_offset in ps.iter().rev() {
            let state_span = state_offset.span(STATE_SIZE);
            let child_ref = self.alloc.alloc_context();
            if child_ref.is_null() {
                return 0;
            }
            let child = ref_to_off(child_ref);
            let Some(child_span) = self.validated_context(child) else {
                self.model_fault = true;
                return 0;
            };

            self.span_set_ctx_num_stats(child_span, 1);
            self.span_set_one_sym(child_span, up_sym);
            self.span_set_one_freq(child_span, up_freq);
            self.span_set_one_succ(child_span, up_succ);
            self.span_set_ctx_suffix(child_span, pc);
            self.span_set_state_succ(state_span, 0, child);

            pc = child;
        }

        pc
    }

    /// Find a state with the given symbol in a stats array.
    #[inline(always)]
    fn span_find_state(
        &self,
        states_span: ValidatedArenaSpan,
        ns: usize,
        sym: u8,
    ) -> Option<usize> {
        self.span_find_state_from(states_span, 0, ns, sym)
    }

    #[inline(always)]
    fn span_find_state_from(
        &self,
        states_span: ValidatedArenaSpan,
        start: usize,
        ns: usize,
        sym: u8,
    ) -> Option<usize> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        let mut index = start;
        #[cfg(any(not(target_arch = "aarch64"), miri))]
        let index = start;

        #[cfg(all(target_arch = "x86_64", not(miri)))]
        if self.use_ssse3_state_batches {
            // SAFETY: SSSE3 support was detected once when the model was created.
            return unsafe { self.span_find_state_from_ssse3(states_span, index, ns, sym) };
        }

        #[cfg(all(target_arch = "aarch64", not(miri)))]
        while index + 8 <= ns {
            let heads = self
                .alloc
                .span_read_state_heads8(states_span, index * STATE_SIZE);
            if let Some(lane) = heads.iter().position(|&head| head as u8 == sym) {
                return Some(index + lane);
            }
            index += 8;
        }

        (index..ns).find(|&state_index| self.span_state_sym(states_span, state_index) == sym)
    }

    /// Vector symbol search over whole eight-state batches.
    ///
    /// `span_read_state_syms8_ssse3` puts the batch's symbols in lanes 0..8 in
    /// state order and zeroes lanes 8..16, so comparing against a broadcast of
    /// the wanted symbol and taking the byte mask gives one bit per state, in
    /// scan order, in bits 0..8. Bits 8..16 can only be set when `sym == 0`
    /// (the zeroed upper lanes match), and masking with `0xff` drops them, so
    /// the lowest set bit is always the first matching state — the same state
    /// the scalar scan below would return.
    ///
    /// The batch threshold stays at "eight states remaining": the broadcast and
    /// the three shuffle constants hoist out of the loop, leaving about twelve
    /// instructions per eight states against roughly four per state (and eight
    /// unpredictable branches) in the scalar scan, so a single full batch
    /// already pays for the setup.
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    #[target_feature(enable = "ssse3")]
    #[inline]
    unsafe fn span_find_state_from_ssse3(
        &self,
        states_span: ValidatedArenaSpan,
        mut index: usize,
        ns: usize,
        sym: u8,
    ) -> Option<usize> {
        use std::arch::x86_64::{_mm_cmpeq_epi8, _mm_movemask_epi8, _mm_set1_epi8};

        let wanted = _mm_set1_epi8(sym as i8);
        while index + 8 <= ns {
            // SAFETY: this function requires SSSE3 and the validated span covers
            // the complete state batch.
            let matches = unsafe {
                let syms = self
                    .alloc
                    .span_read_state_syms8_ssse3(states_span, index * STATE_SIZE);
                _mm_movemask_epi8(_mm_cmpeq_epi8(syms, wanted))
            } & 0xff;
            if matches != 0 {
                return Some(index + matches.trailing_zeros() as usize);
            }
            index += 8;
        }

        (index..ns).find(|&state_index| self.span_state_sym(states_span, state_index) == sym)
    }

    fn clear_mask(&mut self) {
        self.esc_count = 1;
        self.char_mask = [0; 256];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_creation() {
        let model = Model::new(6, 1 << 20).unwrap();
        assert_ne!(model.min_context, 0);
        assert_ne!(model.max_context, 0);
    }

    #[test]
    fn test_model_restart() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.restart();
        assert_ne!(model.min_context, 0);
    }

    /// `run_length` is only ever compared or shifted, so the reference's C `int`
    /// overflow never bites there — but `decode_bin_symbol` shifts it into the
    /// `bin_summ` row index as `((run_length >> 26) as usize) & 0x20`, and a
    /// wrap from `i32::MAX` to `i32::MIN` flips that bucket. A corrupt stream can
    /// hold one binary context long enough to reach the boundary.
    #[test]
    fn run_length_saturates_instead_of_flipping_the_bin_summ_bucket() {
        // The index term as `decode_bin_symbol` computes it.
        let bucket = |run_length: i32| ((run_length >> 26) as usize) & 0x20;

        let mut model = Model::new(6, 1 << 20).unwrap();
        model.run_length = i32::MAX;
        let before = bucket(model.run_length);

        for _ in 0..8 {
            model.run_length = model.run_length.saturating_add(1);
            model.run_length = model.run_length.saturating_add(model.prev_success as i32);
        }

        assert_eq!(model.run_length, i32::MAX);
        assert_eq!(bucket(model.run_length), before);
        assert!(
            before < 64,
            "the index term must stay inside bin_summ's row"
        );

        // What saturation is buying: the wrapped value lands in the other
        // bucket, so a release build would silently decode against different
        // probabilities rather than panicking as the debug build did.
        assert_ne!(bucket(i32::MAX.wrapping_add(1)), before);
    }

    #[test]
    fn see_index_tolerates_a_suffix_with_fewer_stats_like_rar_behavior() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.num_masked = 0;
        model.hi_bits_flag = 0;
        // ns is packed in bits 32..48 of the context head, SummFreq in 48..64.
        let context_head = (4u64 << 32) | (1u64 << 48);
        let diff = 2u32;

        // `Suffix->NumStats-NumStats` is signed in unrar's model.cpp:474, so a suffix
        // with fewer stats makes the comparison false instead of underflowing.
        let (_, small_suffix) = model.make_esc_freq2(context_head, 1, diff);
        let (_, large_suffix) = model.make_esc_freq2(context_head, 200, diff);

        let idx0 = model.ns2_indx[diff as usize - 1] as usize;
        // sf(1) < 11*ns(4) contributes 2 in both cases; only the first term moves.
        assert_eq!(small_suffix, Some((idx0, 2)));
        assert_eq!(large_suffix, Some((idx0, 3)));
    }

    /// A deterministic stream of coder bytes (xorshift32), standing in for
    /// arbitrary input.
    fn noise(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect()
    }

    /// Drives `model` through `data`, restarting it at each end marker, and
    /// returns how many symbols it decoded. Never panics; a corrupt-stream
    /// error ends the run.
    fn decode_noise(model: &mut Model, data: &[u8], symbols: usize) -> usize {
        let mut rc = RarRangeDecoder::new(data).unwrap();
        let mut decoded = 0;
        for _ in 0..symbols {
            match model.decode_symbol(&mut rc) {
                Ok(Some(_)) => decoded += 1,
                Ok(None) => model.restart(),
                Err(Error::CorruptStream { .. }) => break,
                Err(other) => panic!("unexpected error {other:?}"),
            }
        }
        decoded
    }

    /// The smallest arena at the highest order fills within a few hundred
    /// symbols of arbitrary input; the model must restart in place, inside
    /// its fixed arena, and keep decoding.
    #[test]
    fn a_full_arena_restarts_the_model_in_place() {
        let mut model = Model::new(PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE).unwrap();
        let arena = model.arena_addr();
        let data = noise(1 << 16, 0x9E37_79B9);
        let decoded = decode_noise(&mut model, &data, 50_000);
        assert!(decoded > 1_000, "decoded {decoded}");
        assert!(model.restarts > 10, "restarted {} times", model.restarts);
        assert_eq!(model.arena_addr(), arena);
        assert_eq!(model.mem_size(), PPMD7_MIN_MEM_SIZE);
    }

    /// Restarting between every symbol, and re-starting with alternating
    /// orders and arena sizes, never panics and leaves a working model.
    #[test]
    fn restart_storms_leave_a_working_model() {
        let data = noise(1 << 12, 0x0BAD_5EED);
        let mut model = Model::new(6, 1 << 16).unwrap();
        for round in 0..2_000u32 {
            model.restart();
            let mut rc = RarRangeDecoder::new(&data[(round as usize % 2048)..]).unwrap();
            let _ = model.decode_symbol(&mut rc);
        }
        for round in 0..200u32 {
            let order = PPMD7_MIN_ORDER + round % (PPMD7_MAX_ORDER - 1);
            let mem = if round % 2 == 0 { 1 << 12 } else { 1 << 16 };
            model.start(order, mem).unwrap();
            assert_eq!(model.order(), order);
            decode_noise(&mut model, &data, 64);
        }
        model.start(6, 1 << 16).unwrap();
        assert!(decode_noise(&mut model, &data, 256) > 0);
    }

    #[test]
    fn out_of_range_parameters_are_rejected() {
        for (order, mem) in [
            (0, 1 << 20),
            (PPMD7_MIN_ORDER - 1, 1 << 20),
            (PPMD7_MAX_ORDER + 1, 1 << 20),
            (u32::MAX, 1 << 20),
            (6, 0),
            (6, PPMD7_MIN_MEM_SIZE - 1),
            (6, PPMD7_MAX_MEM_SIZE + 1),
            (6, u32::MAX),
        ] {
            assert!(
                matches!(Model::new(order, mem), Err(Error::InvalidParameters)),
                "order {order} mem {mem}"
            );
        }
        let mut model = Model::new(6, 1 << 16).unwrap();
        assert!(matches!(
            model.start(65, 1 << 16),
            Err(Error::InvalidParameters)
        ));
        assert_eq!((model.order(), model.mem_size()), (6, 1 << 16));
    }

    /// A resumed coder whose range is below the root context's frequency
    /// total would divide by zero; the model reports a corrupt stream.
    #[test]
    fn a_range_below_the_frequency_total_is_a_corrupt_stream() {
        let data = [0u8; 16];
        let mut model = Model::new(6, 1 << 16).unwrap();
        for range in [0, 1, 100, 256] {
            let state = crate::rc::RangeCoderState {
                low: 0,
                code: 0,
                range,
            };
            let mut rc = RarRangeDecoder::from_state(&data[..], state);
            assert!(
                matches!(
                    model.decode_symbol(&mut rc),
                    Err(Error::CorruptStream { .. })
                ),
                "range {range}"
            );
            model.restart();
        }
    }

    #[test]
    fn start_reuses_same_sized_allocator_storage() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let untouched_text_offset = UNIT_SIZE + 100;
        model.alloc.write_byte_at(untouched_text_offset, 0xA5);

        let arena = model.arena_addr();
        model.start(16, 1 << 20).unwrap();

        assert_eq!(model.max_order, 16);
        assert_eq!(model.arena_addr(), arena);
        assert_eq!(model.alloc.read_byte_at(untouched_text_offset), 0xA5);
    }

    #[test]
    fn test_root_has_256_symbols() {
        let model = Model::new(6, 1 << 20).unwrap();
        let ns = model.ctx_num_stats(model.min_context);
        assert_eq!(ns, 256);
    }

    #[test]
    fn test_root_summary_freq() {
        let model = Model::new(6, 1 << 20).unwrap();
        let sf = model.ctx_summ_freq(model.min_context);
        assert_eq!(sf, 257);
    }

    #[test]
    fn test_root_states() {
        let model = Model::new(6, 1 << 20).unwrap();
        let stats = model.ctx_stats(model.min_context);
        // First state: symbol=0, freq=1.
        assert_eq!(model.st_sym(stats), 0);
        assert_eq!(model.st_freq(stats), 1);
        // Last state: symbol=255, freq=1.
        let last = stats + 255 * STATE_SIZE as u32;
        assert_eq!(model.st_sym(last), 255);
        assert_eq!(model.st_freq(last), 1);
    }

    #[test]
    fn test_decode_from_zeros() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let data = vec![0u8; 256];
        let mut rc = RarRangeDecoder::new(&data[..]).unwrap();
        // Should decode symbols without crashing. None is valid (escape/end).
        for _ in 0..5 {
            let _ = model.decode_symbol(&mut rc).unwrap();
        }
    }

    #[test]
    fn decode_rejects_context_offset_in_text_region() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.min_context = UNIT_SIZE as u32;
        let data = vec![0u8; 256];
        let mut rc = RarRangeDecoder::new(&data[..]).unwrap();

        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn decode_rejects_stats_offset_in_text_region() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.set_ctx_stats(model.min_context, UNIT_SIZE as u32);
        let data = vec![0u8; 256];
        let mut rc = RarRangeDecoder::new(&data[..]).unwrap();

        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn unmasked_scratch_packing_covers_one_and_256_states() {
        let one = pack_unmasked_state(0, u16::from_le_bytes([17, 23]));
        assert_eq!(unmasked_state_index(one), 0);
        assert_eq!(unmasked_state_symbol(one), 17);
        assert_eq!(unmasked_state_frequency(one), 23);

        let mut scratch = [0u32; 256];
        for (index, slot) in scratch.iter_mut().enumerate() {
            *slot = pack_unmasked_state(index, u16::from_le_bytes([index as u8, 1]));
        }
        let last = scratch[255];
        assert_eq!(unmasked_state_index(last), 255);
        assert_eq!(unmasked_state_symbol(last), 255);
        assert_eq!(unmasked_state_frequency(last), 1);
    }

    #[test]
    fn span_find_state_from_matches_the_scalar_scan_for_every_batch_shape() {
        // The vector searches compare eight states at a time, so the shapes
        // that matter are every `ns` across the batch boundary, every start
        // offset within a batch, a match at every index, and duplicate symbols
        // (first match must win). Symbol zero is called out separately: it is
        // the only value that can alias the zeroed upper lanes of the x86-64
        // symbol gather.
        fn assert_find_agrees_with_scan(model: &Model, span: ValidatedArenaSpan, syms: &[u8]) {
            let ns = syms.len();
            for start in 0..=ns {
                for target in [0u8, 1, 7, 200, 255] {
                    let expected = (start..ns).find(|&index| syms[index] == target);
                    assert_eq!(
                        model.span_find_state_from(span, start, ns, target),
                        expected,
                        "ns={ns} start={start} target={target} syms={syms:?}"
                    );
                }
            }
        }

        fn assert_find_agrees_on_every_path(
            model: &mut Model,
            span: ValidatedArenaSpan,
            syms: &[u8],
        ) {
            for (index, &sym) in syms.iter().enumerate() {
                model.span_write_state(span, index, sym, 1, 0);
            }
            assert_find_agrees_with_scan(model, span, syms);

            // Re-run the same oracle with the batch search disabled so the
            // vector path and the scalar path are checked against each other.
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            {
                let detected = std::mem::replace(&mut model.use_ssse3_state_batches, false);
                assert_find_agrees_with_scan(model, span, syms);
                model.use_ssse3_state_batches = detected;
            }
        }

        let mut model = Model::new(6, 1 << 20).unwrap();
        let context_span = model.validated_context(model.min_context).unwrap();
        let stats = model.span_ctx_stats(context_span);
        let mut seed = 0x2545_f491_4f6c_dd1du64;

        // 1..=24 walks both batch boundaries of the eight-wide kernels; the
        // explicit tail adds the 15/16/17, 31/32/33 and 63/64/65 edge lengths,
        // where several full-width passes and a scalar straggler have to agree
        // with the scan.
        let state_counts = (1..=24usize)
            .chain([31, 32, 33, 47, 48, 49, 63, 64, 65])
            .collect::<Vec<_>>();
        for ns in state_counts {
            let span = model
                .alloc
                .validated_tail_span(stats, ns * STATE_SIZE)
                .expect("the root stats block covers every tested state count");

            // A single match walked across every index, for a normal symbol and
            // for symbol zero.
            for target in [0u8, 200] {
                for hit in 0..ns {
                    let syms = (0..ns)
                        .map(|index| {
                            if index == hit {
                                target
                            } else {
                                100 + index as u8
                            }
                        })
                        .collect::<Vec<_>>();
                    assert_find_agrees_on_every_path(&mut model, span, &syms);
                }
            }

            // Randomized symbols over a four-value alphabet, so every array has
            // duplicates and usually several matches per target.
            for _ in 0..8 {
                let syms = (0..ns)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        ((seed >> 33) % 4) as u8
                    })
                    .collect::<Vec<_>>();
                assert_find_agrees_on_every_path(&mut model, span, &syms);
            }
        }
    }

    #[test]
    fn decode_rejects_zero_state_context() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let context_span = model.validated_context(model.min_context).unwrap();
        model.span_set_ctx_num_stats(context_span, 0);
        let mut rc = RarRangeDecoder::new(&[0u8; 256][..]).unwrap();

        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn decode_rejects_truncated_state_span() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.set_ctx_stats(model.min_context, model.alloc.heap_end_bytes() as u32);
        let mut rc = RarRangeDecoder::new(&[0u8; 256][..]).unwrap();

        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn decode_rejects_suffix_in_text_region() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let context_span = model.validated_context(model.min_context).unwrap();
        model.span_set_ctx_num_stats(context_span, 2);
        model.span_set_ctx_summ_freq(context_span, 3);
        model.span_set_ctx_suffix(context_span, UNIT_SIZE as u32);
        let mut rc = RarRangeDecoder::new(&[0u8; 256][..]).unwrap();

        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn decode_rejects_invalid_successor_before_dereference() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let context_span = model.validated_context(model.min_context).unwrap();
        let stats = model.span_ctx_stats(context_span);
        let states_span = model.validated_states(stats, 256).unwrap();
        model.span_set_state_succ(states_span, 0, u32::MAX);
        model.order_fall = 0;
        let mut rc = RarRangeDecoder::new(&[0u8; 256][..]).unwrap();

        assert_eq!(model.decode_symbol(&mut rc).unwrap(), Some(0));
        let result = model.decode_symbol(&mut rc);

        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn rescale_single_state_collapse_always_halves_once() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let context_span = model.validated_context(model.min_context).unwrap();
        let stats = model.span_ctx_stats(context_span);
        let states_span = model.validated_states(stats, 2).unwrap();

        model.span_set_ctx_num_stats(context_span, 2);
        model.span_set_ctx_summ_freq(context_span, 3);
        model.span_write_state(states_span, 0, 10, 2, 0);
        model.span_write_state(states_span, 1, 11, 1, 0);
        model.found_state = stats;
        model.order_fall = 0;

        assert!(model.rescale(model.min_context));
        assert_eq!(model.span_ctx_num_stats(context_span), 1);
        assert_eq!(model.span_one_sym(context_span), 10);
        assert_eq!(model.span_one_freq(context_span), 2);
    }
}
