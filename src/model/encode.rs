//! Encoding through the variant H model.
//!
//! The mirror of the decode path in `model.rs`, derived from Igor Pavlov's
//! `Ppmd7z_EncodeSymbol` (`C/Ppmd7Enc.c`, public domain) and Dmitry
//! Shkarin's PPMd variant H encoder (public domain). Nothing here derives
//! from unrar, which has no encoder.
//!
//! The encoder drives the model through exactly the update code the decoder
//! uses (`update1_0`, `update1`, `update2`, the binary-context updates,
//! SEE, `rescale` and `next_context`/`UpdateModel`), so a stream encoded
//! here leaves the model in the state the decoder reaches reading it back.
//! Only the search differs: the decoder looks for the cumulative count the
//! coder hands it, the encoder for the symbol it was given. With the 7z
//! coder the output is byte-identical to 7-Zip's.

use super::{
    Model, STATE_SIZE, ValidatedArenaSpan, pack_unmasked_state, unmasked_state_frequency,
    unmasked_state_index, unmasked_state_symbol,
};
use crate::error::Result;
use crate::rc::RangeEncoder;

/// The symbol value the end marker is coded as (`PPMD7_SYM_END`): it
/// matches no state, so it escapes out of every context including order 0.
const END_MARKER: i32 = crate::SYM_END;

impl Model {
    /// Encodes one symbol: `Some(byte)` for a byte, `None` for the end
    /// marker (an escape out of the order-0 context).
    ///
    /// The model must be fresh from [`Model::new`], [`Model::start`] or
    /// [`Model::restart`], or have encoded only bytes since: after the end
    /// marker it has to be restarted before it codes again. The coder
    /// normalizes on every call, as [`RangeEncoder`] requires.
    ///
    /// Errors: [`Error::CorruptStream`](crate::Error::CorruptStream) if the
    /// coder met a range scaled to zero or the model is inconsistent (for
    /// example a byte after the end marker). Neither happens to a model
    /// driven only through this method.
    #[inline(always)]
    pub fn encode_symbol<E: RangeEncoder>(&mut self, rc: &mut E, sym: Option<u8>) -> Result<()> {
        let target = sym.map_or(END_MARKER, i32::from);
        let coded = self.encode_char(rc, target);
        if rc.faulted() {
            self.model_fault = false;
            return Self::corrupt_model("frequency total exceeds the encoder's range");
        }
        if core::mem::take(&mut self.model_fault) || coded != target {
            return Self::corrupt_model("encoder model state is inconsistent");
        }
        Ok(())
    }

    /// `Ppmd7z_EncodeSymbols`: encodes every byte of `data` in order.
    pub(crate) fn encode_bytes<E: RangeEncoder>(&mut self, rc: &mut E, data: &[u8]) -> Result<()> {
        for &byte in data {
            self.encode_symbol(rc, Some(byte))?;
        }
        Ok(())
    }

    /// `Ppmd7z_EncodeSymbol`: returns the coded symbol (`target`), -1 once
    /// the end marker escaped out of order 0, or -1 with `model_fault` set.
    fn encode_char<E: RangeEncoder>(&mut self, rc: &mut E, target: i32) -> i32 {
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
            if !self.encode_symbol1(
                rc,
                context_span,
                states_span,
                context_head,
                target,
                &mut found_span,
            ) {
                return -1;
            }
        } else if !self.encode_bin_symbol(rc, context_span, context_head, target, &mut found_span) {
            return -1;
        }

        // Escape loop: walk the suffix chain past contexts whose symbols are
        // all masked, exactly as the decoder does.
        let mut validated_suffix: Option<(ValidatedArenaSpan, u64)> = None;
        while found_span.is_none() {
            let (code_context_span, code_context_head) = loop {
                self.order_fall += 1;
                let suffix = active_context_head as u32;
                self.min_context = suffix;
                if self.min_context == 0 {
                    // Escaped out of order 0: the end marker.
                    return -1;
                }
                let (suffix_span, suffix_head) = if let Some((span, head)) = validated_suffix.take()
                {
                    if span.offset() != self.min_context as usize {
                        return self.fail_model();
                    }
                    (span, head)
                } else {
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
            if !self.encode_symbol2(
                rc,
                code_context_span,
                code_context_head,
                target,
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

    /// The binary-context branch of `Ppmd7z_EncodeSymbol`.
    fn encode_bin_symbol<E: RangeEncoder>(
        &mut self,
        rc: &mut E,
        context_span: ValidatedArenaSpan,
        context_head: u64,
        target: i32,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        let ctx = self.min_context;
        debug_assert_eq!(context_span.offset(), ctx as usize);
        let symbol = (context_head >> 48) as u8;
        let freq = (context_head >> 56) as u8;
        let Some(index) = self.bin_summ_index(context_head) else {
            return false;
        };
        let bs = u32::from(self.bin_summ[index.0][index.1]);

        if i32::from(symbol) == target {
            rc.encode_bit(bs, 0);
            self.update_bin_hit(ctx, context_span, freq, index, found_span);
            true
        } else {
            rc.encode_bit(bs, 1);
            self.update_bin_escape(symbol, index, found_span)
        }
    }

    /// The multi-symbol branch of `Ppmd7z_EncodeSymbol`: the first state,
    /// then the rest in order, then the escape.
    fn encode_symbol1<E: RangeEncoder>(
        &mut self,
        rc: &mut E,
        context_span: ValidatedArenaSpan,
        states_span: ValidatedArenaSpan,
        context_head: u64,
        target: i32,
        found_span: &mut Option<ValidatedArenaSpan>,
    ) -> bool {
        let ctx = self.min_context;
        debug_assert_eq!(context_span.offset(), ctx as usize);
        let ns = (context_head >> 32) as u16 as usize;
        let sum_freq = (context_head >> 48) as u16 as u32;
        debug_assert_eq!(states_span.len(), ns * STATE_SIZE);

        if sum_freq == 0 {
            self.model_fault = true;
            return false;
        }

        let p0_freq = self.span_state_freq(states_span, 0) as u32;
        if i32::from(self.span_state_sym(states_span, 0)) == target {
            rc.encode(0, p0_freq, sum_freq);
            return self.update1_0(
                ctx,
                context_span,
                states_span,
                p0_freq,
                sum_freq,
                found_span,
            );
        }

        if self.found_state == 0 {
            return false;
        }

        self.prev_success = 0;
        let mut hi_cnt = p0_freq;
        for state_index in 1..ns {
            let p_freq = self.span_state_freq(states_span, state_index) as u32;
            if i32::from(self.span_state_sym(states_span, state_index)) == target {
                rc.encode(hi_cnt, p_freq, sum_freq);
                return self.update1(
                    ctx,
                    context_span,
                    states_span,
                    state_index,
                    p_freq as u8,
                    found_span,
                );
            }
            hi_cnt += p_freq;
        }

        // Escape: mask every symbol of this context.
        self.hi_bits_flag = self.hb2_flag[self.prev_sym as usize];
        self.num_masked = ns as u32;
        self.found_state = 0;
        *found_span = None;
        for index in (0..ns).rev() {
            let sym = self.span_state_sym(states_span, index);
            self.char_mask[sym as usize] = self.esc_count;
        }
        let Some(escape_freq) = sum_freq.checked_sub(hi_cnt) else {
            self.model_fault = true;
            return false;
        };
        rc.encode(hi_cnt, escape_freq, sum_freq);
        true
    }

    /// The masked-context loop body of `Ppmd7z_EncodeSymbol`: SEE's escape
    /// estimate, the unmasked states, then the symbol or another escape.
    fn encode_symbol2<E: RangeEncoder>(
        &mut self,
        rc: &mut E,
        context_span: ValidatedArenaSpan,
        context_head: u64,
        target: i32,
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
            self.model_fault = true;
            return false;
        };
        if diff == 0 {
            self.model_fault = true;
            return false;
        }

        let suffix_ns = suffix_data.map_or(0, |(_, head)| (head >> 32) as u16 as u32);
        if ns != 256 && suffix_ns > 256 {
            self.model_fault = true;
            return false;
        }
        let (esc_freq, see_index) = self.make_esc_freq2(context_head, suffix_ns, diff);
        let n = diff as usize;

        // One pass over every state, as in the decoder: the unmasked
        // frequency sum and count without a branch per state, and the
        // target's index and the unmasked sum before it. A consistent model
        // has exactly `ns - num_masked` unmasked states; any other count
        // falls back to collecting the first `n`, as the decoder does.
        let esc_count = self.esc_count;
        let alloc = &self.alloc;
        let char_mask = &self.char_mask;
        let mut hi_cnt = 0u32;
        let mut found = 0usize;
        let mut low = 0u32;
        let mut target_index = usize::MAX;
        for state_index in 0..ns as usize {
            let head = alloc.span_read_u16(states_span, state_index * STATE_SIZE);
            let unmasked = char_mask[head as u8 as usize] != esc_count;
            let freq = u32::from(head >> 8) & 0u32.wrapping_sub(u32::from(unmasked));
            // The target matches at most once in a consistent model, so this
            // branch is taken once per walk; only the first unmasked match
            // counts, as in the collecting loop below.
            if unmasked && i32::from(head as u8) == target && target_index == usize::MAX {
                target_index = state_index;
                low = hi_cnt;
            }
            hi_cnt += freq;
            found += usize::from(unmasked);
        }
        let consistent = found == n;
        let mut selected = None;
        if consistent {
            if target_index != usize::MAX {
                let head = alloc.span_read_u16(states_span, target_index * STATE_SIZE);
                selected = Some((pack_unmasked_state(target_index, head), low));
            }
        } else {
            hi_cnt = 0u32;
            let scratch = &mut self.unmasked_scratch[..n];
            let mut state_index = 0usize;
            for slot in scratch.iter_mut() {
                let head = loop {
                    if state_index >= ns as usize {
                        self.model_fault = true;
                        return false;
                    }
                    let head = alloc.span_read_u16(states_span, state_index * STATE_SIZE);
                    if char_mask[head as u8 as usize] != esc_count {
                        break head;
                    }
                    state_index += 1;
                };
                let packed = pack_unmasked_state(state_index, head);
                let freq = u32::from(unmasked_state_frequency(packed));
                if selected.is_none() && i32::from(unmasked_state_symbol(packed)) == target {
                    selected = Some((packed, hi_cnt));
                }
                hi_cnt += freq;
                *slot = packed;
                state_index += 1;
            }
        }
        let scale = esc_freq + hi_cnt;

        if let Some((packed, low)) = selected {
            let state_freq = unmasked_state_frequency(packed);
            rc.encode(low, u32::from(state_freq), scale);
            self.see_update_success(see_index);
            return self.update2(
                ctx,
                context_span,
                states_span,
                unmasked_state_index(packed),
                state_freq,
                found_span,
            );
        }

        // Escape again.
        rc.encode(hi_cnt, esc_freq, scale);
        self.see_update_escape(see_index, scale);

        // In a consistent model every state is masked already or one of the
        // unmasked ones, so stamping all of them is the same mask.
        let char_mask = &mut self.char_mask;
        if consistent {
            for state_index in 0..ns as usize {
                let sym = self
                    .alloc
                    .span_read_u8(states_span, state_index * STATE_SIZE);
                char_mask[sym as usize] = esc_count;
            }
        } else {
            for &packed in &self.unmasked_scratch[..n] {
                char_mask[unmasked_state_symbol(packed) as usize] = esc_count;
            }
        }
        self.num_masked = ns;
        *validated_suffix = suffix_data;
        true
    }
}
