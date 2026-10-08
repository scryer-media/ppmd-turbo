//! The variant H context model.
//!
//! Dmitry Shkarin's PPMd variant H: contexts and their symbol states, binary
//! contexts with their adaptive `BinSumm` probabilities, secondary escape
//! estimation for masked contexts, model update (`UpdateModel`,
//! `CreateSuccessors`, `Rescale`) and restart when the arena fills.
//!
//! This is a direct translation of ppmd-rust 1.5.0's `internal/ppmd7`
//! (CC0-1.0 / MIT-0), which is in turn a translation of Igor Pavlov's
//! `C/Ppmd7.c`, `C/Ppmd7Dec.c` and `C/Ppmd7Enc.c` in 7-Zip (public domain).
//! Every update rule, table and branch is the reference's; only the
//! representation differs: records are addressed by 32-bit offsets into the
//! [`Arena`], and the range coder is reached through [`RangeDecoder`] and
//! [`RangeEncoder`](crate::rc::RangeEncoder) (`encode.rs`), whose calls
//! normalize eagerly where 7-Zip normalizes at the top of the escape loop.
//!
//! **Consistency, and why the model trusts its own records.** Input reaches
//! the model only as the coder's choice among the symbols the model offers,
//! so whatever the input, the model evolves only through its own updates.
//! Those updates keep it consistent: every context's symbols are distinct,
//! a subset of its suffix's, and the order-0 context holds all 256. The
//! proof is in `docs/algorithms.md` ("Model consistency"). An aborted symbol
//! (a count past the total, a coder fault, or the end marker) would leave
//! `MinContext` below `MaxContext` with `OrderFall` raised; continuing from
//! there could add a symbol twice. Every abort path therefore puts both back
//! as they were when the symbol began, so the model stays consistent between
//! calls whatever the caller does next. Arena access is unchecked in release
//! builds on the strength of that invariant and checked by `debug_assert!`
//! in debug builds, Miri and the fuzz targets.

use crate::alloc::{Arena, UNIT_SIZE, u2i};
use crate::error::{Error, Result};
use crate::rc::{RangeDecoder, corrupt};
use crate::see::{DUMMY, SeeTable};
use crate::{PPMD7_MAX_MEM_SIZE, PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE, PPMD7_MIN_ORDER};

mod encode;

// --- Constants (`Ppmd.h`, `Ppmd7.c`) ---

const MAX_ORDER: usize = PPMD7_MAX_ORDER as usize;
const MAX_FREQ: u32 = 124;
const INT_BITS: u32 = 7;
const PERIOD_BITS: u32 = 7;
const BIN_SCALE: u32 = 1 << (INT_BITS + PERIOD_BITS);

const INIT_BIN_ESC: [u16; 8] = [
    0x3CDD, 0x1F3F, 0x59BF, 0x48F3, 0x64A1, 0x5ABC, 0x6632, 0x6051,
];

static EXP_ESCAPE: [u8; 16] = [25, 14, 9, 7, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2, 2];

/// `NS2BSIndx`.
static NS2BS_INDEX: [u8; 256] = {
    let mut t = [0u8; 256];
    t[1] = 2;
    let mut i = 2;
    while i < 256 {
        t[i] = if i < 11 { 4 } else { 6 };
        i += 1;
    }
    t
};

/// `NS2Indx`.
static NS2INDEX: [u8; 256] = {
    let mut t = [0u8; 256];
    t[0] = 0;
    t[1] = 1;
    t[2] = 2;
    let mut m = 3u32;
    let mut k = 1u32;
    let mut i = 3;
    while i < 256 {
        t[i] = m as u8;
        k -= 1;
        if k == 0 {
            m += 1;
            k = m - 2;
        }
        i += 1;
    }
    t
};

/// `PPMD_GET_MEAN`.
#[inline(always)]
const fn get_mean(prob: u32) -> u32 {
    (prob + (1 << (PERIOD_BITS - 2))) >> PERIOD_BITS
}

/// `PPMD_UPDATE_PROB_1`.
#[inline(always)]
const fn update_prob_1(prob: u32) -> u32 {
    prob - get_mean(prob)
}

/// `PPMD7_HiBitsFlag_3`: 8 for symbols 0x40 and up.
#[inline(always)]
const fn hi_bits_flag3(sym: u32) -> u32 {
    ((sym + 0xC0) >> (8 - 3)) & (1 << 3)
}

/// `PPMD7_HiBitsFlag_4`: 16 for symbols 0x40 and up.
#[inline(always)]
const fn hi_bits_flag4(sym: u32) -> u32 {
    ((sym + 0xC0) >> (8 - 4)) & (1 << 4)
}

// --- Record layout ---
//
// Context (one 12-byte unit): NumStats u16 at 0; SummFreq u16 at 2; Stats
// u32 at 4; Suffix u32 at 8. A binary context (NumStats == 1) keeps its one
// state in place of SummFreq and Stats: symbol at 2, freq at 3, successor at 4.
// State (6 bytes, two per unit): symbol at 0, freq at 1, successor u32 at 2.

const CTX_NUM_STATS: u32 = 0;
const CTX_SUMM_FREQ: u32 = 2;
const CTX_STATS: u32 = 4;
const CTX_SUFFIX: u32 = 8;
/// The binary context's state (`Ppmd7Context_OneState`).
const CTX_ONE_STATE: u32 = 2;

const STATE_SIZE: u32 = 6;
const ST_SYMBOL: u32 = 0;
const ST_FREQ: u32 = 1;
const ST_SUCCESSOR: u32 = 2;

/// A PPMd variant H context model with its arena.
///
/// One model codes one stream; RAR's framing keeps it alive across blocks
/// and members, which is why it is separate from the range coder.
pub struct Model {
    a: Arena,
    min_context: u32,
    max_context: u32,
    found_state: u32,
    order_fall: u32,
    init_esc: u32,
    prev_success: u32,
    max_order: u32,
    hi_bits_flag: u32,
    run_length: i32,
    init_rl: i32,
    see: SeeTable,
    bin_summ: [[u16; 64]; 128],
    /// Model restarts so far, including the ones a full arena forces.
    #[cfg(test)]
    restarts: u32,
}

impl Model {
    /// Creates a model of the given order over an arena of `mem_size` bytes.
    ///
    /// `order` must be in [`PPMD7_MIN_ORDER`]`..=`[`PPMD7_MAX_ORDER`] and
    /// `mem_size` in [`PPMD7_MIN_MEM_SIZE`]`..=`[`PPMD7_MAX_MEM_SIZE`];
    /// anything else is [`Error::InvalidParameters`]. The arena is allocated
    /// here, once, and never grows: `mem_size` bytes plus up to three bytes
    /// of alignment, as 7-Zip's `Ppmd7_Alloc` lays it out.
    pub fn new(order: u32, mem_size: u32) -> Result<Self> {
        Self::check_parameters(order, mem_size)?;
        let mut model = Self {
            a: Arena::new(mem_size),
            min_context: 0,
            max_context: 0,
            found_state: 0,
            order_fall: 0,
            init_esc: 0,
            prev_success: 0,
            max_order: order,
            hi_bits_flag: 0,
            run_length: 0,
            init_rl: 0,
            see: SeeTable::new(),
            bin_summ: [[0; 64]; 128],
            #[cfg(test)]
            restarts: 0,
        };
        model.restart_model();
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
        self.max_order
    }

    /// The arena size in bytes the model was created or last started with.
    pub fn mem_size(&self) -> u32 {
        self.a.size()
    }

    /// Address of the model arena, for tests that check a same-size restart
    /// keeps it.
    #[cfg(test)]
    pub(crate) fn arena_addr(&self) -> usize {
        self.a.arena_addr()
    }

    /// Restarts the model with a new order and arena size, as a fresh
    /// [`Model::new`] would be, but keeping the arena when its size is
    /// unchanged so its pages are not faulted in again.
    pub fn start(&mut self, order: u32, mem_size: u32) -> Result<()> {
        Self::check_parameters(order, mem_size)?;
        if self.a.size() != mem_size {
            self.a = Arena::new(mem_size);
        }
        self.max_order = order;
        self.restart_model();
        Ok(())
    }

    /// Restarts the model from scratch with its current order and arena
    /// size: the state a stream begins in (`Ppmd7_Init`).
    pub fn restart(&mut self) {
        self.restart_model();
    }

    // ---- record access -----------------------------------------------------

    #[inline(always)]
    fn num_stats(&self, c: u32) -> u32 {
        self.a.u16(c + CTX_NUM_STATS) as u32
    }

    #[inline(always)]
    fn set_num_stats(&mut self, c: u32, v: u32) {
        self.a.set_u16(c + CTX_NUM_STATS, v as u16);
    }

    #[inline(always)]
    fn summ_freq(&self, c: u32) -> u32 {
        self.a.u16(c + CTX_SUMM_FREQ) as u32
    }

    #[inline(always)]
    fn set_summ_freq(&mut self, c: u32, v: u32) {
        self.a.set_u16(c + CTX_SUMM_FREQ, v as u16);
    }

    #[inline(always)]
    fn stats(&self, c: u32) -> u32 {
        self.a.u32(c + CTX_STATS)
    }

    #[inline(always)]
    fn set_stats(&mut self, c: u32, v: u32) {
        self.a.set_u32(c + CTX_STATS, v);
    }

    #[inline(always)]
    fn suffix(&self, c: u32) -> u32 {
        self.a.u32(c + CTX_SUFFIX)
    }

    #[inline(always)]
    fn set_suffix(&mut self, c: u32, v: u32) {
        self.a.set_u32(c + CTX_SUFFIX, v);
    }

    #[inline(always)]
    fn sym(&self, s: u32) -> u32 {
        self.a.u8(s + ST_SYMBOL) as u32
    }

    #[inline(always)]
    fn freq(&self, s: u32) -> u32 {
        self.a.u8(s + ST_FREQ) as u32
    }

    #[inline(always)]
    fn set_freq(&mut self, s: u32, v: u32) {
        self.a.set_u8(s + ST_FREQ, v as u8);
    }

    #[inline(always)]
    fn successor(&self, s: u32) -> u32 {
        self.a.u32(s + ST_SUCCESSOR)
    }

    #[inline(always)]
    fn set_successor(&mut self, s: u32, v: u32) {
        self.a.set_u32(s + ST_SUCCESSOR, v);
    }

    /// The first state of `ns` from `stats` with symbol `sym`. The model's
    /// invariants guarantee one exists wherever the reference searches
    /// without a bound; the bound only keeps the walk inside the array.
    #[inline(always)]
    fn find_state(&self, stats: u32, ns: u32, sym: u32) -> Option<u32> {
        let end = stats + ns * STATE_SIZE;
        let mut s = stats;
        while s < end {
            if self.sym(s) == sym {
                return Some(s);
            }
            s += STATE_SIZE;
        }
        debug_assert!(
            false,
            "symbol {sym} missing from a context that must hold it"
        );
        None
    }

    // ---- RestartModel ------------------------------------------------------

    /// `RestartModel`: empty the arena and rebuild the order-0 context.
    #[inline(never)]
    fn restart_model(&mut self) {
        #[cfg(test)]
        {
            self.restarts += 1;
        }
        self.a.reset();

        self.order_fall = self.max_order;
        self.init_rl = -(self.max_order.min(12) as i32) - 1;
        self.run_length = self.init_rl;
        self.prev_success = 0;

        self.a.hi_unit -= UNIT_SIZE;
        let mc = self.a.hi_unit;
        let s = self.a.lo_unit;
        self.a.lo_unit += (256 / 2) * UNIT_SIZE;
        self.min_context = mc;
        self.max_context = mc;
        self.found_state = s;

        self.set_num_stats(mc, 256);
        self.set_summ_freq(mc, 256 + 1);
        self.set_stats(mc, s);
        self.set_suffix(mc, 0);

        for i in 0..256u32 {
            let st = s + i * STATE_SIZE;
            self.a.set_u8(st + ST_SYMBOL, i as u8);
            self.a.set_u8(st + ST_FREQ, 1);
            self.set_successor(st, 0);
        }

        for (i, row) in self.bin_summ.iter_mut().enumerate() {
            for (k, &esc) in INIT_BIN_ESC.iter().enumerate() {
                let val = (BIN_SCALE - esc as u32 / (i as u32 + 2)) as u16;
                for m in (0..64).step_by(8) {
                    row[k + m] = val;
                }
            }
        }

        self.see.reset();
    }

    // ---- CreateSuccessors / UpdateModel ------------------------------------

    /// `CreateSuccessors`: turns the raw successor of `FoundState` (a
    /// position in the text) into real contexts, linking them from
    /// `FoundState` and from the identical raw successors in the suffix
    /// contexts of `MinContext`. `None` when the arena is full.
    #[inline(never)]
    fn create_successors(&mut self) -> Option<u32> {
        let mut c = self.min_context;
        let fs = self.found_state;
        let up_branch = self.successor(fs);
        let fs_sym = self.sym(fs);
        let mut ps = [0u32; MAX_ORDER];
        let mut num_ps = 0usize;

        if self.order_fall != 0 {
            ps[0] = fs;
            num_ps = 1;
        }

        loop {
            let suffix = self.suffix(c);
            if suffix == 0 {
                break;
            }
            c = suffix;
            let ns = self.num_stats(c);
            let s = if ns != 1 {
                self.find_state(self.stats(c), ns, fs_sym)?
            } else {
                c + CTX_ONE_STATE
            };
            let successor = self.successor(s);
            if successor != up_branch {
                // `c` is the real context here.
                c = successor;
                if num_ps == 0 {
                    // A real MAX-order context: nothing to create.
                    return Some(c);
                }
                break;
            }
            *ps.get_mut(num_ps)? = s;
            num_ps += 1;
        }

        // Every new context has a single symbol whose raw successor is the
        // next text position after `FoundState`'s.
        let new_sym = self.a.u8(up_branch) as u32;
        let up_branch = up_branch + 1;

        let ns = self.num_stats(c);
        let new_freq = if ns == 1 {
            self.freq(c + CTX_ONE_STATE)
        } else {
            let s = self.find_state(self.stats(c), ns, new_sym)?;
            let cf = self.freq(s) - 1;
            let s0 = self.summ_freq(c) - ns - cf;
            1 + if 2 * cf <= s0 {
                (5 * cf > s0) as u32
            } else {
                // `s0 >= 1` in a consistent model (it counts the escape).
                (2 * cf + s0 - 1) / (2 * s0).max(1) + 1
            }
        };

        // New single-symbol contexts, from low order to high.
        while num_ps != 0 {
            let c1 = self.a.alloc_context()?;
            self.set_num_stats(c1, 1);
            self.a.set_u8(c1 + CTX_ONE_STATE + ST_SYMBOL, new_sym as u8);
            self.a.set_u8(c1 + CTX_ONE_STATE + ST_FREQ, new_freq as u8);
            self.set_successor(c1 + CTX_ONE_STATE, up_branch);
            self.set_suffix(c1, c);
            num_ps -= 1;
            self.set_successor(ps[num_ps], c1);
            c = c1;
        }
        Some(c)
    }

    /// `UpdateModel`.
    #[inline(never)]
    fn update_model(&mut self) {
        let fs = self.found_state;
        let fs_sym = self.sym(fs);
        let mc = self.min_context;

        if self.freq(fs) < MAX_FREQ / 4 && self.suffix(mc) != 0 {
            // Update the frequency in the suffix context.
            let c = self.suffix(mc);
            if self.num_stats(c) == 1 {
                let s = c + CTX_ONE_STATE;
                if self.freq(s) < 32 {
                    self.set_freq(s, self.freq(s) + 1);
                }
            } else {
                let mut s = self.stats(c);
                if self.sym(s) != fs_sym {
                    let Some(found) = self.find_state(s, self.num_stats(c), fs_sym) else {
                        self.restart_model();
                        return;
                    };
                    s = found;
                    if self.freq(s) >= self.freq(s - STATE_SIZE) {
                        self.a.swap6(s, s - STATE_SIZE);
                        s -= STATE_SIZE;
                    }
                }
                if self.freq(s) < MAX_FREQ - 9 {
                    self.set_freq(s, self.freq(s) + 2);
                    self.set_summ_freq(c, self.summ_freq(c) + 2);
                }
            }
        }

        if self.order_fall == 0 {
            // MAX-order context: `FoundState`'s successor is raw.
            let Some(c) = self.create_successors() else {
                self.restart_model();
                return;
            };
            self.min_context = c;
            self.max_context = c;
            self.set_successor(self.found_state, c);
            return;
        }

        // NON-MAX-order context.
        let text = self.a.text;
        self.a.set_u8(text, fs_sym as u8);
        let text = text + 1;
        self.a.text = text;
        if text >= self.a.units_start {
            self.restart_model();
            return;
        }
        let mut max_successor = text;
        let mut min_successor = self.successor(fs);

        if min_successor == 0 {
            // Only the order-0 context holds null successors: make it raw,
            // and the next context is the order-0 context again.
            self.set_successor(fs, max_successor);
            min_successor = self.min_context;
        } else {
            if min_successor <= max_successor {
                // A raw successor: create the real contexts.
                let Some(c) = self.create_successors() else {
                    self.restart_model();
                    return;
                };
                min_successor = c;
            }
            // `min_successor` is now the real (order + 1) context.
            self.order_fall -= 1;
            if self.order_fall == 0 {
                max_successor = min_successor;
                self.a.text -= (self.max_context != self.min_context) as u32;
            }
        }

        let mc = self.min_context;
        let mut c = self.max_context;
        self.min_context = min_successor;
        self.max_context = min_successor;

        if c == mc {
            return;
        }

        // s0: the pure escape frequency.
        let ns = self.num_stats(mc);
        let fs_freq = self.freq(fs);
        let s0 = self.summ_freq(mc) - ns - (fs_freq - 1);

        while c != mc {
            let ns1 = self.num_stats(c);
            let mut sum;
            if ns1 != 1 {
                if ns1 & 1 == 0 {
                    // Grow the state array by one unit.
                    let old_nu = ns1 >> 1;
                    let i = u2i(old_nu);
                    if i != u2i(old_nu + 1) {
                        let Some(ptr) = self.a.alloc_units(i + 1) else {
                            self.restart_model();
                            return;
                        };
                        let old_ptr = self.stats(c);
                        self.a.copy(old_ptr, ptr, old_nu * UNIT_SIZE);
                        self.a.insert_node(old_ptr, i);
                        self.set_stats(c, ptr);
                    }
                }
                sum = self.summ_freq(c);
                // The escape frequency grows by at most 3 here.
                sum += ((2 * ns1 < ns) as u32)
                    + 2 * (((4 * ns1 <= ns) as u32) & ((sum <= 8 * ns1) as u32));
            } else {
                // The binary context becomes a two-symbol context.
                let Some(s) = self.a.alloc_units(0) else {
                    self.restart_model();
                    return;
                };
                let one = c + CTX_ONE_STATE;
                let mut freq = self.freq(one);
                let one_sym = self.a.u8(one + ST_SYMBOL);
                let one_successor = self.successor(one);
                self.a.set_u8(s + ST_SYMBOL, one_sym);
                self.set_successor(s, one_successor);
                self.set_stats(c, s);
                if freq < MAX_FREQ / 4 - 1 {
                    freq <<= 1;
                } else {
                    freq = MAX_FREQ - 4;
                }
                self.set_freq(s, freq);
                sum = freq + self.init_esc + ((ns > 3) as u32);
            }

            let s = self.stats(c) + ns1 * STATE_SIZE;
            let mut cf = 2 * (sum + 6) * fs_freq;
            let sf = s0 + sum;
            self.a.set_u8(s + ST_SYMBOL, fs_sym as u8);
            self.set_num_stats(c, ns1 + 1);
            self.set_successor(s, max_successor);
            if cf < 6 * sf {
                cf = 1 + ((cf > sf) as u32) + ((cf >= 4 * sf) as u32);
                sum += 3;
            } else {
                cf = 4
                    + ((cf >= 9 * sf) as u32)
                    + ((cf >= 12 * sf) as u32)
                    + ((cf >= 15 * sf) as u32);
                sum += cf;
            }
            self.set_summ_freq(c, sum);
            self.set_freq(s, cf);

            c = self.suffix(c);
        }
    }

    // ---- Rescale -----------------------------------------------------------

    /// `Rescale`: halves the frequencies of `MinContext`, keeping the states
    /// sorted, and drops zero-frequency states (possible only in a MAX-order
    /// context, where `OrderFall == 0`).
    #[inline(never)]
    fn rescale(&mut self) {
        let mc = self.min_context;
        let stats = self.stats(mc);
        let mut s = self.found_state;

        // Move the found state to the front.
        if s != stats {
            let tmp = self.a.read6(s);
            while s != stats {
                let prev = self.a.read6(s - STATE_SIZE);
                self.a.write6(s, prev);
                s -= STATE_SIZE;
            }
            self.a.write6(s, tmp);
        }

        let mut sum_freq = self.freq(s);
        let mut esc_freq = self.summ_freq(mc) - sum_freq;
        let adder = (self.order_fall != 0) as u32;

        sum_freq = (sum_freq + 4 + adder) >> 1;
        let num_stats = self.num_stats(mc);
        self.set_freq(s, sum_freq);

        for _ in 0..num_stats - 1 {
            s += STATE_SIZE;
            let mut freq = self.freq(s);
            esc_freq -= freq;
            freq = (freq + adder) >> 1;
            sum_freq += freq;
            self.set_freq(s, freq);
            if freq > self.freq(s - STATE_SIZE) {
                let tmp = self.a.read6(s);
                let mut s1 = s;
                loop {
                    let prev = self.a.read6(s1 - STATE_SIZE);
                    self.a.write6(s1, prev);
                    s1 -= STATE_SIZE;
                    if !(s1 != stats && freq > self.freq(s1 - STATE_SIZE)) {
                        break;
                    }
                }
                self.a.write6(s1, tmp);
            }
        }

        if self.freq(s) == 0 {
            // Remove the zero-frequency states at the tail.
            let mut i = 0;
            while self.freq(s) == 0 {
                i += 1;
                s -= STATE_SIZE;
            }
            esc_freq += i;
            let num_stats_new = num_stats - i;
            self.set_num_stats(mc, num_stats_new);
            let n0 = (num_stats + 1) >> 1;

            if num_stats_new == 1 {
                // A single-symbol context.
                let mut freq = self.freq(stats);
                loop {
                    esc_freq >>= 1;
                    freq = (freq + 1) >> 1;
                    if esc_freq <= 1 {
                        break;
                    }
                }
                let one = mc + CTX_ONE_STATE;
                let state = self.a.read6(stats);
                self.a.write6(one, state);
                self.set_freq(one, freq);
                self.found_state = one;
                self.a.insert_node(stats, u2i(n0));
                return;
            }

            let n1 = (num_stats_new + 1) >> 1;
            if n0 != n1 {
                let i0 = u2i(n0);
                let i1 = u2i(n1);
                if i0 != i1 {
                    if self.a.has_free(i1) {
                        let ptr = self.a.remove_node(i1);
                        self.set_stats(mc, ptr);
                        self.a.copy(stats, ptr, n1 * UNIT_SIZE);
                        self.a.insert_node(stats, i0);
                    } else {
                        self.a.split_block(stats, i0, i1);
                    }
                }
            }
        }

        // Halve the escape frequency.
        self.set_summ_freq(mc, sum_freq + esc_freq - (esc_freq >> 1));
        self.found_state = self.stats(mc);
    }

    // ---- SEE and BinSumm ---------------------------------------------------

    /// `Ppmd7_MakeEscFreq`: the SEE context for the masked context
    /// `MinContext` and the escape frequency it estimates.
    #[inline(always)]
    fn make_esc_freq(&mut self, num_masked: u32) -> (usize, u32) {
        let mc = self.min_context;
        let num_stats = self.num_stats(mc);
        if num_stats != 256 {
            let non_masked = num_stats - num_masked;
            let suffix_ns = self.num_stats(self.suffix(mc));
            // Unsigned, as in `Ppmd7.c`; a consistent model never wraps it
            // (a suffix holds every symbol of its child).
            let idx = NS2INDEX[non_masked as usize - 1] as usize * 16
                + (non_masked < suffix_ns.wrapping_sub(num_stats)) as usize
                + 2 * (self.summ_freq(mc) < 11 * num_stats) as usize
                + 4 * (num_masked > non_masked) as usize
                + self.hi_bits_flag as usize;
            let esc = self.see.get(idx).take_mean();
            (idx, esc)
        } else {
            (DUMMY, 1)
        }
    }

    /// `Ppmd7_GetBinSumm`: the `BinSumm` cell of the binary context
    /// `MinContext`, setting `HiBitsFlag` from the previous symbol.
    #[inline(always)]
    fn bin_summ_index(&mut self) -> (usize, usize) {
        let mc = self.min_context;
        let one = mc + CTX_ONE_STATE;
        let hb3 = hi_bits_flag3(self.sym(self.found_state));
        let hb4 = hi_bits_flag4(self.sym(one));
        self.hi_bits_flag = hb3;
        let suffix_ns = self.num_stats(self.suffix(mc));
        let col = self.prev_success
            + ((self.run_length as u32 >> 26) & 0x20)
            + NS2BS_INDEX[suffix_ns as usize - 1] as u32
            + hb4
            + hb3;
        (self.freq(one) as usize - 1, col as usize)
    }

    // ---- symbol updates ----------------------------------------------------

    /// `NextContext`.
    #[inline(always)]
    fn next_context(&mut self) {
        let c = self.successor(self.found_state);
        if self.order_fall == 0 && c >= self.a.units_start {
            self.min_context = c;
            self.max_context = c;
        } else {
            self.update_model();
        }
    }

    /// `Ppmd7_Update1`: a symbol other than the first found in `MinContext`.
    #[inline(always)]
    fn update1(&mut self) {
        let mut s = self.found_state;
        let freq = self.freq(s) + 4;
        let mc = self.min_context;
        self.set_summ_freq(mc, self.summ_freq(mc) + 4);
        self.set_freq(s, freq);
        if freq > self.freq(s - STATE_SIZE) {
            self.a.swap6(s, s - STATE_SIZE);
            s -= STATE_SIZE;
            self.found_state = s;
            if freq > MAX_FREQ {
                self.rescale();
            }
        }
        self.next_context();
    }

    /// `Ppmd7_Update1_0`: the first symbol of `MinContext`.
    #[inline(always)]
    fn update1_0(&mut self) {
        let s = self.found_state;
        let mc = self.min_context;
        let freq = self.freq(s);
        let summ_freq = self.summ_freq(mc);
        self.prev_success = (2 * freq > summ_freq) as u32;
        self.run_length = self.run_length.wrapping_add(self.prev_success as i32);
        self.set_summ_freq(mc, summ_freq + 4);
        let freq = freq + 4;
        self.set_freq(s, freq);
        if freq > MAX_FREQ {
            self.rescale();
        }
        self.next_context();
    }

    /// `Ppmd7_Update2`: a symbol found after an escape.
    #[inline(always)]
    fn update2(&mut self) {
        let s = self.found_state;
        let freq = self.freq(s) + 4;
        self.run_length = self.init_rl;
        let mc = self.min_context;
        self.set_summ_freq(mc, self.summ_freq(mc) + 4);
        self.set_freq(s, freq);
        if freq > MAX_FREQ {
            self.rescale();
        }
        self.update_model();
    }

    /// `Ppmd7_UpdateBin`: the binary context's symbol was coded.
    #[inline(always)]
    fn update_bin(&mut self, s: u32) {
        let freq = self.freq(s);
        self.found_state = s;
        self.prev_success = 1;
        self.run_length = self.run_length.wrapping_add(1);
        self.set_freq(s, freq + (freq < 128) as u32);
        self.next_context();
    }

    /// Masks the symbols of the states `[stats, last]`, two at a time from
    /// the front and `last` on its own, as `Ppmd7.c`'s `MASK_SYMBOLS` does.
    #[inline(always)]
    fn mask_symbols(&self, char_mask: &mut [u8; 256], last: u32, stats: u32) {
        char_mask[self.sym(last) as usize] = 0;
        let mut s2 = stats;
        while s2 < last {
            let sym0 = self.sym(s2);
            let sym1 = self.sym(s2 + STATE_SIZE);
            s2 += 2 * STATE_SIZE;
            char_mask[sym0 as usize] = 0;
            char_mask[sym1 as usize] = 0;
        }
    }

    /// Puts the model back as it was when the symbol began, after the
    /// symbol was abandoned part way down the escape chain (see the module
    /// documentation).
    #[cold]
    fn abandon_symbol(&mut self, order_fall: u32) {
        self.min_context = self.max_context;
        self.order_fall = order_fall;
    }

    // ---- decoding (`Ppmd7Dec.c`) -------------------------------------------

    /// Decodes one symbol.
    ///
    /// Returns `Ok(Some(byte))` for a decoded byte and `Ok(None)` for the end
    /// marker (an escape out of the order-0 context) or a count past the
    /// frequency total, which a valid stream never produces; RAR reads both
    /// as the model giving up on the block. A coder that faulted is
    /// [`Error::CorruptStream`]. The model stays consistent either way and
    /// may be used again.
    #[inline(always)]
    pub fn decode_symbol<R: RangeDecoder>(&mut self, rc: &mut R) -> Result<Option<u8>> {
        let ch = self.decode_char(rc);
        if rc.faulted() {
            // A frequency total outran the coder's range: the stream is
            // corrupt whatever symbol the arithmetic then produced.
            return Err(corrupt("frequency total exceeds the coder's range"));
        }
        Ok(u8::try_from(ch).ok())
    }

    /// `Ppmd7z_DecodeSymbol`: the symbol, or -1 (end marker) or -2 (a count
    /// past the total).
    #[inline(always)]
    fn decode_char<R: RangeDecoder>(&mut self, rc: &mut R) -> i32 {
        let entry_order_fall = self.order_fall;
        let mut char_mask: [u8; 256];
        let mc = self.min_context;
        let ns = self.num_stats(mc);

        if ns != 1 {
            let mut s = self.stats(mc);
            let summ_freq = self.summ_freq(mc);
            let mut count = rc.get_threshold(summ_freq);
            let hi_cnt = count;

            let freq = self.freq(s);
            if count < freq {
                rc.decode(0, freq);
                self.found_state = s;
                let sym = self.sym(s);
                self.update1_0();
                return sym as i32;
            }
            count -= freq;

            self.prev_success = 0;
            for _ in 1..ns {
                s += STATE_SIZE;
                let freq = self.freq(s);
                if count < freq {
                    rc.decode(hi_cnt - count, freq);
                    self.found_state = s;
                    let sym = self.sym(s);
                    self.update1();
                    return sym as i32;
                }
                count -= freq;
            }

            if hi_cnt >= summ_freq {
                self.abandon_symbol(entry_order_fall);
                return crate::SYM_ERROR;
            }

            let hi_cnt = hi_cnt - count;
            rc.decode(hi_cnt, summ_freq - hi_cnt);

            self.hi_bits_flag = hi_bits_flag3(self.sym(self.found_state));
            char_mask = [u8::MAX; 256];
            self.mask_symbols(&mut char_mask, s, self.stats(mc));
        } else {
            let s = mc + CTX_ONE_STATE;
            let (row, col) = self.bin_summ_index();
            let pr = self.bin_summ[row][col] as u32;
            if rc.decode_bit(pr) == 0 {
                self.bin_summ[row][col] = (update_prob_1(pr) + (1 << INT_BITS)) as u16;
                let sym = self.sym(s);
                self.update_bin(s);
                return sym as i32;
            }
            let pr = update_prob_1(pr);
            self.bin_summ[row][col] = pr as u16;
            self.init_esc = EXP_ESCAPE[(pr >> 10) as usize] as u32;
            char_mask = [u8::MAX; 256];
            char_mask[self.sym(s) as usize] = 0;
            self.prev_success = 0;
        }

        loop {
            let mut mc = self.min_context;
            let num_masked = self.num_stats(mc);
            loop {
                self.order_fall += 1;
                let suffix = self.suffix(mc);
                if suffix == 0 {
                    self.abandon_symbol(entry_order_fall);
                    return crate::SYM_END;
                }
                mc = suffix;
                if self.num_stats(mc) != num_masked {
                    break;
                }
            }

            let stats = self.stats(mc);
            let ns = self.num_stats(mc);
            let mut s = stats;
            let odd = ns & 1;
            let mut hi_cnt =
                self.freq(s) & char_mask[self.sym(s) as usize] as u32 & 0u32.wrapping_sub(odd);
            s += odd * STATE_SIZE;
            for _ in 0..ns / 2 {
                let sym0 = self.sym(s);
                let sym1 = self.sym(s + STATE_SIZE);
                hi_cnt += self.freq(s) & char_mask[sym0 as usize] as u32;
                hi_cnt += self.freq(s + STATE_SIZE) & char_mask[sym1 as usize] as u32;
                s += 2 * STATE_SIZE;
            }
            self.min_context = mc;

            let (see, esc_freq) = self.make_esc_freq(num_masked);
            let freq_sum = esc_freq + hi_cnt;
            let mut count = rc.get_threshold(freq_sum);

            if count < hi_cnt {
                let mut s = stats;
                let hi = count;
                loop {
                    let f = self.freq(s) & char_mask[self.sym(s) as usize] as u32;
                    if count < f {
                        break;
                    }
                    count -= f;
                    s += STATE_SIZE;
                }
                let freq = self.freq(s);
                rc.decode(hi - count, freq);
                self.see.get(see).update();
                self.found_state = s;
                let sym = self.sym(s);
                self.update2();
                return sym as i32;
            }

            if count >= freq_sum {
                self.abandon_symbol(entry_order_fall);
                return crate::SYM_ERROR;
            }

            rc.decode(hi_cnt, freq_sum - hi_cnt);
            // `see.summ` grows by every unmasked frequency; it may wrap.
            let cell = self.see.get(see);
            cell.summ = cell.summ.wrapping_add(freq_sum as u16);

            let end = stats + ns * STATE_SIZE;
            let mut s = stats;
            while s < end {
                char_mask[self.sym(s) as usize] = 0;
                s += STATE_SIZE;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rc::RarRangeDecoder;

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

    impl Model {
        /// Walks the suffix chain from `MaxContext` and checks the
        /// invariants the module documentation relies on: distinct symbols,
        /// each context's symbols a subset of its suffix's, frequencies
        /// nonzero, the order-0 context full, and `MinContext == MaxContext`
        /// between symbols.
        fn check_consistency(&self) {
            assert_eq!(self.min_context, self.max_context);
            let mut c = self.max_context;
            let mut child: Option<[bool; 256]> = None;
            let mut depth = 0;
            loop {
                let ns = self.num_stats(c);
                assert!((1..=256).contains(&ns), "num_stats {ns}");
                let mut present = [false; 256];
                if ns == 1 {
                    let one = c + CTX_ONE_STATE;
                    assert!(self.freq(one) >= 1 && self.freq(one) <= 128);
                    present[self.sym(one) as usize] = true;
                } else {
                    let stats = self.stats(c);
                    let mut sum = 0;
                    for i in 0..ns {
                        let s = stats + i * STATE_SIZE;
                        assert!(!present[self.sym(s) as usize], "duplicate symbol");
                        present[self.sym(s) as usize] = true;
                        assert!(self.freq(s) >= 1);
                        sum += self.freq(s);
                    }
                    assert!(sum < self.summ_freq(c), "no room for the escape");
                }
                if let Some(child) = child {
                    for sym in 0..256 {
                        assert!(!child[sym] || present[sym], "child symbol not in suffix");
                    }
                }
                child = Some(present);
                let suffix = self.suffix(c);
                if suffix == 0 {
                    assert_eq!(ns, 256, "the order-0 context holds every symbol");
                    break;
                }
                c = suffix;
                depth += 1;
                assert!(depth <= self.max_order);
            }
        }
    }

    #[test]
    fn a_new_model_has_the_full_order_0_context() {
        let model = Model::new(6, 1 << 20).unwrap();
        let mc = model.min_context;
        assert_eq!(model.num_stats(mc), 256);
        assert_eq!(model.summ_freq(mc), 257);
        let stats = model.stats(mc);
        assert_eq!((model.sym(stats), model.freq(stats)), (0, 1));
        let last = stats + 255 * STATE_SIZE;
        assert_eq!((model.sym(last), model.freq(last)), (255, 1));
        assert_eq!(model.found_state, stats);
        assert_eq!(model.order_fall, 6);
        model.check_consistency();
    }

    #[test]
    fn lookup_tables_match_ppmd7_construct() {
        assert_eq!(&NS2INDEX[..8], &[0, 1, 2, 3, 4, 4, 5, 5]);
        assert_eq!(NS2INDEX[255], 24);
        assert_eq!(&NS2BS_INDEX[..3], &[0, 2, 4]);
        assert_eq!((NS2BS_INDEX[10], NS2BS_INDEX[11]), (4, 6));
    }

    /// `run_length` is a C `int` the reference lets wrap; the wrap moves the
    /// `BinSumm` column by `0x20`, exactly as 7-Zip and ppmd-rust do.
    #[test]
    fn run_length_wraps_as_the_reference_does() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        model.run_length = i32::MAX;
        model.run_length = model.run_length.wrapping_add(1);
        assert_eq!(model.run_length, i32::MIN);
        assert_eq!((model.run_length as u32 >> 26) & 0x20, 0x20);
    }

    /// Drives `model` through `data`, restarting it at each end marker, and
    /// returns how many symbols it decoded. Checks consistency after every
    /// symbol when `check` is set.
    fn decode_noise(model: &mut Model, data: &[u8], symbols: usize, check: bool) -> usize {
        let mut rc = RarRangeDecoder::new(data).unwrap();
        let mut decoded = 0;
        for _ in 0..symbols {
            match model.decode_symbol(&mut rc) {
                Ok(Some(_)) => decoded += 1,
                Ok(None) => model.restart(),
                Err(Error::CorruptStream { .. }) => break,
                Err(other) => panic!("unexpected error {other:?}"),
            }
            if check {
                model.check_consistency();
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
        let decoded = decode_noise(&mut model, &data, 50_000, false);
        assert!(decoded > 1_000, "decoded {decoded}");
        assert!(model.restarts > 10, "restarted {} times", model.restarts);
        assert_eq!(model.arena_addr(), arena);
        assert_eq!(model.mem_size(), PPMD7_MIN_MEM_SIZE);
    }

    /// Arbitrary input never makes the model inconsistent, including when
    /// decoding carries on after the end marker and after counts past the
    /// total without a restart (the abort paths put the model back).
    #[test]
    fn noise_keeps_the_model_consistent_across_aborted_symbols() {
        for seed in 1..=24u32 {
            let data = noise(1 << 12, seed.wrapping_mul(0x9E37_79B9));
            for (order, mem) in [(2, 1 << 11), (6, 1 << 14), (16, 1 << 12), (64, 1 << 11)] {
                let mut model = Model::new(order, mem).unwrap();
                let mut rc = RarRangeDecoder::new(&data[..]).unwrap();
                for _ in 0..3_000 {
                    let before = model.order_fall;
                    match model.decode_symbol(&mut rc) {
                        Ok(Some(_)) => {}
                        Ok(None) => assert_eq!(model.order_fall, before),
                        Err(_) => break,
                    }
                    model.check_consistency();
                }
            }
        }
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
            decode_noise(&mut model, &data, 64, true);
        }
        model.start(6, 1 << 16).unwrap();
        assert!(decode_noise(&mut model, &data, 256, true) > 0);
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
    /// total would divide by zero; the model reports a corrupt stream and
    /// stays consistent.
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
            model.check_consistency();
        }
    }

    #[test]
    fn start_reuses_same_sized_arena_storage() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let untouched = model.a.text + 100;
        model.a.set_u8(untouched, 0xA5);
        let arena = model.arena_addr();
        model.start(16, 1 << 20).unwrap();
        assert_eq!(model.max_order, 16);
        assert_eq!(model.arena_addr(), arena);
        assert_eq!(model.a.u8(untouched), 0xA5);
    }

    #[test]
    fn decoding_zeros_works() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        let data = vec![0u8; 256];
        let mut rc = RarRangeDecoder::new(&data[..]).unwrap();
        for _ in 0..5 {
            let _ = model.decode_symbol(&mut rc).unwrap();
            model.check_consistency();
        }
    }

    /// `Rescale` collapsing a MAX-order context to one state halves the
    /// surviving frequency once per halving of the escape frequency.
    #[test]
    fn rescale_single_state_collapse() {
        let mut model = Model::new(6, 1 << 20).unwrap();
        // A two-state context in a fresh unit block, with one state that
        // rescales to zero.
        let c = model.a.alloc_context().unwrap();
        let stats = model.a.alloc_units(0).unwrap();
        model.set_num_stats(c, 2);
        model.set_summ_freq(c, 3);
        model.set_stats(c, stats);
        model.set_suffix(c, model.max_context);
        model.a.write6(stats, [10, 2, 0, 0, 0, 0]);
        model.a.write6(stats + STATE_SIZE, [11, 1, 0, 0, 0, 0]);
        model.min_context = c;
        model.found_state = stats;
        model.order_fall = 0;

        model.rescale();
        assert_eq!(model.num_stats(c), 1);
        assert_eq!(model.sym(c + CTX_ONE_STATE), 10);
        // freq (2 + 4) >> 1 = 3, then esc_freq 0 + 1 halves once: (3 + 1) >> 1.
        assert_eq!(model.freq(c + CTX_ONE_STATE), 2);
        assert_eq!(model.found_state, c + CTX_ONE_STATE);
    }
}
