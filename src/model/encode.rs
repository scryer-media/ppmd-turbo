//! Encoding through the variant H model.
//!
//! A translation of ppmd-rust 1.5.0's `internal/ppmd7/encoder.rs`
//! (CC0-1.0 / MIT-0), itself a translation of Igor Pavlov's
//! `Ppmd7z_EncodeSymbol` (`C/Ppmd7Enc.c`, 7-Zip, public domain). The encoder
//! drives the model through exactly the update code the decoder uses, so a
//! stream encoded here leaves the model in the state the decoder reaches
//! reading it back; with the 7z coder the output is byte-identical to
//! 7-Zip's. The reference divides the range in the model and then calls
//! `RC_Encode(start, size)`; [`RangeEncoder::encode`] takes the total and
//! does both.

use super::{
    CTX_ONE_STATE, EXP_ESCAPE, INT_BITS, Model, SS, STATE_SIZE, hi_bits_flag3, update_prob_1,
};
use crate::error::Result;
use crate::rc::{RangeEncoder, corrupt};

impl Model {
    /// Encodes one symbol: `Some(byte)` for a byte, `None` for the end
    /// marker (an escape out of the order-0 context).
    ///
    /// The model must be fresh from [`Model::new`], [`Model::start`] or
    /// [`Model::restart`], or have encoded only bytes since, for the stream
    /// to decode. The coder normalizes on every call, as [`RangeEncoder`]
    /// requires.
    ///
    /// Errors: [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the
    /// coder met a range scaled to zero, which a model driven only through
    /// this method never produces.
    #[inline(always)]
    pub fn encode_symbol<E: RangeEncoder>(&mut self, rc: &mut E, sym: Option<u8>) -> Result<()> {
        self.encode_char(rc, sym.map_or(crate::SYM_END, i32::from));
        if rc.faulted() {
            return Err(corrupt("frequency total exceeds the encoder's range"));
        }
        Ok(())
    }

    /// `Ppmd7z_EncodeSymbols`: encodes every byte of `data` in order.
    #[allow(dead_code)] // The step encoder checks its sink per byte.
    pub(crate) fn encode_bytes<E: RangeEncoder>(&mut self, rc: &mut E, data: &[u8]) -> Result<()> {
        for &byte in data {
            self.encode_symbol(rc, Some(byte))?;
        }
        Ok(())
    }

    /// `Ppmd7z_EncodeSymbol`.
    #[inline(always)]
    fn encode_char<E: RangeEncoder>(&mut self, rc: &mut E, symbol: i32) {
        let entry_order_fall = self.order_fall;
        let mut char_mask: [u8; 256];
        let mc = self.min_context;
        let ns = self.num_stats(mc);

        if ns != 1 {
            let mut s = self.stats(mc);
            let summ_freq = self.summ_freq(mc);
            if self.sym(s) as i32 == symbol {
                rc.encode(0, self.freq(s), summ_freq);
                self.found_state = s;
                self.update1_0();
                return;
            }
            self.prev_success = 0;
            let mut sum = self.freq(s);
            let mut p = s as usize;
            let last = p + (ns as usize - 1) * SS;
            while p != last {
                p += SS;
                if self.sym_at(p) as i32 == symbol {
                    rc.encode(sum, self.freq_at(p), summ_freq);
                    self.found_state = p as u32;
                    self.update1();
                    return;
                }
                sum += self.freq_at(p);
            }
            s = p as u32;
            rc.encode(sum, summ_freq - sum, summ_freq);

            self.hi_bits_flag = hi_bits_flag3(self.sym(self.found_state));
            char_mask = [u8::MAX; 256];
            self.mask_symbols(&mut char_mask, s, self.stats(mc));
        } else {
            let s = mc + CTX_ONE_STATE;
            let (row, col) = self.bin_summ_index();
            let pr = self.bin_summ[row][col] as u32;
            if self.sym(s) as i32 == symbol {
                self.bin_summ[row][col] = (update_prob_1(pr) + (1 << INT_BITS)) as u16;
                rc.encode_bit(pr, 0);
                self.update_bin(s);
                return;
            }
            let pr1 = update_prob_1(pr);
            self.bin_summ[row][col] = pr1 as u16;
            self.init_esc = EXP_ESCAPE[(pr1 >> 10) as usize] as u32;
            rc.encode_bit(pr, 1);
            char_mask = [u8::MAX; 256];
            char_mask[self.sym(s) as usize] = 0;
            self.prev_success = 0;
        }

        loop {
            let mut mc = self.min_context;
            let num_masked = self.num_stats(mc);
            let mut i;
            loop {
                self.order_fall += 1;
                let suffix = self.suffix(mc);
                if suffix == 0 {
                    // The end marker (or a symbol no context holds).
                    self.abandon_symbol(entry_order_fall);
                    return;
                }
                mc = suffix;
                i = self.num_stats(mc);
                if i != num_masked {
                    break;
                }
            }
            self.min_context = mc;

            let (see, esc_freq) = self.make_esc_freq(num_masked);
            let stats = self.stats(mc);
            let mut s = stats as usize;
            let end = s + i as usize * SS;
            let mut sum = 0u32;

            while s < end {
                let cur = self.sym_at(s);
                if cur as i32 == symbol {
                    let low = sum;
                    let freq = self.freq_at(s);
                    self.see.get(see).update();
                    self.found_state = s as u32;
                    sum += esc_freq;

                    // The rest of the unmasked total, the found state
                    // included: `i` states from `s` on.
                    let i = ((end - s) / SS) as u32;
                    let odd = i & 1;
                    sum += freq & 0u32.wrapping_sub(odd);
                    s += odd as usize * SS;
                    while s < end {
                        let sym0 = self.sym_at(s);
                        let sym1 = self.sym_at(s + SS);
                        sum += self.freq_at(s) & char_mask[sym0 as usize] as u32;
                        sum += self.freq_at(s + SS) & char_mask[sym1 as usize] as u32;
                        s += 2 * SS;
                    }
                    rc.encode(low, freq, sum);
                    self.update2();
                    return;
                }
                sum += self.freq_at(s) & char_mask[cur as usize] as u32;
                s += SS;
            }
            let s = s as u32;

            let total = sum + esc_freq;
            let cell = self.see.get(see);
            cell.summ = (cell.summ as u32).wrapping_add(total) as u16;
            rc.encode(sum, esc_freq, total);

            self.mask_symbols(&mut char_mask, s - STATE_SIZE, stats);
        }
    }
}
