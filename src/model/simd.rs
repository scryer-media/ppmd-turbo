//! Vector escape passes for wide contexts.
//!
//! In a masked context the escape pass sums `freq & char_mask[sym]` over
//! every state. With 32 or more states, NEON gathers 16 states per step:
//! `vld3q_u16` splits the 6-byte records so lane 0 of each triple is
//! `Symbol | Freq << 8`, and four 64-entry `tbl` lookups read the 256-byte
//! mask for 16 symbols at once. Narrower contexts, where building the
//! lookup costs more than it saves, keep the scalar loops. The sums are
//! integers, so the result is the same on every path.

#[cfg(all(target_arch = "aarch64", target_endian = "little", not(miri)))]
pub(super) mod neon {
    use core::arch::aarch64::*;

    /// Contexts narrower than this take the scalar path: below 32 states,
    /// building the lookup costs order-2 text more than the batches save.
    pub(crate) const MIN_STATES: u32 = 32;

    /// Bytes of 16 states.
    pub(crate) const BATCH_BYTES: usize = 16 * 6;

    /// The 256-byte mask in registers.
    pub(crate) struct Lookup {
        table: [uint8x16x4_t; 4],
    }

    impl Lookup {
        #[inline(always)]
        pub(crate) fn new(char_mask: &[u8; 256]) -> Self {
            let p = char_mask.as_ptr();
            // SAFETY: `char_mask` is 256 bytes, exactly the four 64-byte
            // loads; NEON is in every AArch64 target's baseline.
            unsafe {
                Self {
                    table: [
                        vld1q_u8_x4(p),
                        vld1q_u8_x4(p.add(64)),
                        vld1q_u8_x4(p.add(128)),
                        vld1q_u8_x4(p.add(192)),
                    ],
                }
            }
        }

        /// `char_mask[sym]` per lane. An index past a 64-entry table reads
        /// 0 from `tbl`, so exactly one of the four lookups is live per
        /// lane and their OR is the mask byte.
        #[inline(always)]
        fn mask(&self, syms: uint8x16_t) -> uint8x16_t {
            let [t0, t1, t2, t3] = self.table;
            // SAFETY: register-only NEON intrinsics (baseline feature).
            unsafe {
                vorrq_u8(
                    vorrq_u8(
                        vqtbl4q_u8(t0, syms),
                        vqtbl4q_u8(t1, vsubq_u8(syms, vdupq_n_u8(64))),
                    ),
                    vorrq_u8(
                        vqtbl4q_u8(t2, vsubq_u8(syms, vdupq_n_u8(128))),
                        vqtbl4q_u8(t3, vsubq_u8(syms, vdupq_n_u8(192))),
                    ),
                )
            }
        }

        /// The unmasked frequencies of the 16 states at `p` (masked lanes
        /// zero).
        ///
        /// # Safety
        ///
        /// `p` must be valid for reading [`BATCH_BYTES`] bytes.
        #[inline(always)]
        pub(crate) unsafe fn batch(&self, p: *const u8) -> uint8x16_t {
            // SAFETY: the caller guarantees the 96 bytes the two
            // `vld3q_u16` read; AArch64 permits unaligned vector loads.
            // Little-endian (cfg above): lane 0 of a triple is
            // `Symbol | Freq << 8`.
            unsafe {
                let h0 = vld3q_u16(p.cast::<u16>()).0;
                let h1 = vld3q_u16(p.add(48).cast::<u16>()).0;
                let syms = vmovn_high_u16(vmovn_u16(h0), h1);
                let freqs = vshrn_high_n_u16::<8>(vshrn_n_u16::<8>(h0), h1);
                vandq_u8(freqs, self.mask(syms))
            }
        }

        /// The unmasked frequency total of `batches` runs of 16 states
        /// from `p`.
        ///
        /// # Safety
        ///
        /// `p` must be valid for reading `batches * BATCH_BYTES` bytes.
        #[inline(always)]
        pub(crate) unsafe fn sum(&self, p: *const u8, batches: usize) -> u32 {
            // SAFETY: register-only intrinsics, and each `batch` reads
            // inside the range the caller guarantees. A u16 lane takes two
            // u8 lanes per batch, at most 16 batches of 255 each: no
            // overflow.
            unsafe {
                let mut acc = vdupq_n_u16(0);
                for k in 0..batches {
                    acc = vpadalq_u8(acc, self.batch(p.add(k * BATCH_BYTES)));
                }
                vaddlvq_u16(acc)
            }
        }

        /// The unmasked frequency total of one batch.
        ///
        /// # Safety
        ///
        /// As [`batch`](Self::batch).
        #[inline(always)]
        pub(crate) unsafe fn batch_sum(&self, p: *const u8) -> u32 {
            // SAFETY: as `batch`.
            unsafe { u32::from(vaddlvq_u8(self.batch(p))) }
        }
    }
}
