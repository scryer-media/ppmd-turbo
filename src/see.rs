//! Secondary escape estimation.
//!
//! Variant H's SEE contexts are adaptive estimators of the escape frequency
//! in a masked (non-binary) context, indexed by the context's shape. The
//! design is part of Dmitry Shkarin's PPMd variant H; this is a translation of
//! ppmd-rust 1.5.0's `See` (CC0-1.0 / MIT-0), itself a translation of
//! `CPpmd_See` in Igor Pavlov's `C/Ppmd7.c` (7-Zip, public domain).

/// `PPMD_PERIOD_BITS`.
const PERIOD_BITS: u8 = 7;

/// The table's 25 x 16 contexts, flattened, plus the dummy context the
/// 256-symbol context uses.
pub(crate) const SEE_CELLS: usize = 25 * 16 + 1;

/// Index of `DummySee`.
pub(crate) const DUMMY: usize = 25 * 16;

/// One SEE context (`CPpmd_See`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct See {
    /// Scaled sum of escape frequencies. It wraps at 16 bits as the
    /// reference's does: only its low 16 bits are ever used.
    pub(crate) summ: u16,
    /// Right shift that turns `summ` into the mean.
    pub(crate) shift: u8,
    /// Updates left before the next adaptation step.
    pub(crate) count: u8,
}

impl See {
    /// `Ppmd_See_UPDATE`: after a symbol found through this context.
    #[inline(always)]
    pub(crate) fn update(&mut self) {
        if self.shift < PERIOD_BITS {
            self.count = self.count.wrapping_sub(1);
            if self.count == 0 {
                self.summ = self.summ.wrapping_shl(1);
                self.count = (3u32 << self.shift) as u8;
                self.shift += 1;
            }
        }
    }

    /// The escape frequency `MakeEscFreq` takes from this context:
    /// `r = summ >> shift; summ -= r; r + (r == 0)`.
    #[inline(always)]
    pub(crate) fn take_mean(&mut self) -> u32 {
        let r = self.summ >> self.shift;
        self.summ -= r;
        u32::from(r) + u32::from(r == 0)
    }
}

/// The SEE table with its dummy context.
pub(crate) struct SeeTable {
    pub(crate) cells: [See; SEE_CELLS],
}

impl SeeTable {
    /// A table as `RestartModel` leaves it.
    pub(crate) fn new() -> Self {
        let mut table = Self {
            cells: [See::default(); SEE_CELLS],
        };
        table.reset();
        table
    }

    /// `RestartModel`'s SEE part: row `i` starts at `(5 * i + 10) << 3`
    /// with shift 3 and count 4; the dummy at summ 0, shift 7, count 64.
    pub(crate) fn reset(&mut self) {
        for i in 0..25 {
            let summ = ((5 * i + 10) << (PERIOD_BITS - 4)) as u16;
            for k in 0..16 {
                self.cells[i * 16 + k] = See {
                    summ,
                    shift: PERIOD_BITS - 4,
                    count: 4,
                };
            }
        }
        self.cells[DUMMY] = See {
            summ: 0,
            shift: PERIOD_BITS,
            count: 64,
        };
    }

    /// The context at `index` (`row * 16 + column`, or [`DUMMY`]).
    #[inline(always)]
    pub(crate) fn get(&mut self, index: usize) -> &mut See {
        &mut self.cells[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_matches_restart_model() {
        let table = SeeTable::new();
        assert_eq!(table.cells[0].summ, 80);
        assert_eq!(table.cells[24 * 16 + 15].summ, 1040);
        assert_eq!((table.cells[0].shift, table.cells[0].count), (3, 4));
        assert_eq!(
            table.cells[DUMMY],
            See {
                summ: 0,
                shift: 7,
                count: 64
            }
        );
    }

    #[test]
    fn take_mean_subtracts_and_never_returns_zero() {
        let mut see = See {
            summ: 160,
            shift: 3,
            count: 4,
        };
        assert_eq!(see.take_mean(), 20);
        assert_eq!(see.summ, 140);
        see.summ = 0;
        assert_eq!(see.take_mean(), 1);
        assert_eq!(see.summ, 0);
    }

    #[test]
    fn update_doubles_after_count_and_stops_at_period_bits() {
        let mut see = See {
            summ: 160,
            shift: 3,
            count: 4,
        };
        for _ in 0..4 {
            see.update();
        }
        assert_eq!((see.summ, see.shift, see.count), (320, 4, 24));
        let mut top = See {
            summ: 0x9000,
            shift: 7,
            count: 1,
        };
        top.update();
        assert_eq!((top.summ, top.shift, top.count), (0x9000, 7, 1));
        // The doubling keeps only the low 16 bits.
        let mut wrap = See {
            summ: 0x9000,
            shift: 6,
            count: 1,
        };
        wrap.update();
        assert_eq!((wrap.summ, wrap.shift, wrap.count), (0x2000, 7, 192));
    }
}
