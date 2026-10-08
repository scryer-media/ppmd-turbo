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

/// The x86-64 passes: SSSE3 takes 16 states per batch, AVX2 32 (two
/// 16-state halves, one per 128-bit lane). `pshufb` looks up only 16
/// entries, so the mask goes in as a 256-bit set (bit `s` set when symbol
/// `s` is unmasked) built with `movemask`; each symbol's byte of the set is
/// one of two 16-byte tables and its bit a third lookup.
#[cfg(all(target_arch = "x86_64", not(miri)))]
pub(super) mod x86 {
    use std::arch::x86_64::*;

    /// Without `std` there is no run-time detection: a tier runs only when
    /// the build enables its target features.
    #[cfg(not(feature = "std"))]
    macro_rules! is_x86_feature_detected {
        ($feature:tt) => {
            cfg!(target_feature = $feature)
        };
    }

    /// Contexts narrower than this take the scalar path.
    pub(crate) const MIN_STATES: u32 = 32;

    /// Bytes of 16 states.
    const BATCH_BYTES: usize = 16 * 6;

    /// The widest pass the CPU runs.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Tier {
        Scalar,
        Ssse3,
        Avx2,
    }

    impl Tier {
        /// The best tier this CPU supports.
        pub(crate) fn detect() -> Self {
            if is_x86_feature_detected!("avx2") {
                Tier::Avx2
            } else if is_x86_feature_detected!("ssse3") {
                Tier::Ssse3
            } else {
                Tier::Scalar
            }
        }
    }

    /// The `pshufb` control that moves byte `off` of every state in the
    /// 16-byte load `load` of a batch to its state's lane.
    const fn gather(load: usize, off: usize) -> [u8; 16] {
        let mut m = [0x80u8; 16];
        let mut k = 0;
        while k < 16 {
            let p = 6 * k + off;
            if p >= 16 * load && p < 16 * load + 16 {
                m[k] = (p - 16 * load) as u8;
            }
            k += 1;
        }
        m
    }

    const fn gathers(off: usize) -> [[u8; 16]; 6] {
        [
            gather(0, off),
            gather(1, off),
            gather(2, off),
            gather(3, off),
            gather(4, off),
            gather(5, off),
        ]
    }

    static SYM: [[u8; 16]; 6] = gathers(0);
    static FREQ: [[u8; 16]; 6] = gathers(1);
    static BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

    /// The unmasked-symbol set as two 16-byte tables.
    #[inline(always)]
    unsafe fn set(char_mask: &[u8; 256]) -> (__m128i, __m128i) {
        let mut bits = [0u16; 16];
        // SAFETY: each load reads 16 of the mask's 256 bytes; SSE2 is in
        // the x86-64 baseline.
        unsafe {
            for (k, b) in bits.iter_mut().enumerate() {
                let v = _mm_loadu_si128(char_mask.as_ptr().add(16 * k).cast());
                *b = _mm_movemask_epi8(v) as u16;
            }
            (
                _mm_loadu_si128(bits.as_ptr().cast()),
                _mm_loadu_si128(bits.as_ptr().add(8).cast()),
            )
        }
    }

    /// The scalar walk over `p[from..to]`.
    #[inline(always)]
    unsafe fn tail(p: *const u8, mut from: usize, to: usize, char_mask: &[u8; 256]) -> u32 {
        let mut sum = 0;
        while from < to {
            // SAFETY: the caller's range covers these state bytes.
            unsafe {
                sum += u32::from(*p.add(from + 1) & char_mask[usize::from(*p.add(from))]);
            }
            from += 6;
        }
        sum
    }

    /// SSSE3: the unmasked frequencies of the 16 states at `p`.
    #[target_feature(enable = "ssse3")]
    #[inline]
    unsafe fn batch_ssse3(p: *const u8, t0: __m128i, t1: __m128i) -> __m128i {
        // SAFETY: the caller guarantees 96 readable bytes at `p`; every
        // intrinsic is SSE2 or SSSE3, enabled here.
        unsafe {
            let mut syms = _mm_setzero_si128();
            let mut freqs = _mm_setzero_si128();
            for j in 0..6 {
                let v = _mm_loadu_si128(p.add(16 * j).cast());
                syms = _mm_or_si128(
                    syms,
                    _mm_shuffle_epi8(v, _mm_loadu_si128(SYM[j].as_ptr().cast())),
                );
                freqs = _mm_or_si128(
                    freqs,
                    _mm_shuffle_epi8(v, _mm_loadu_si128(FREQ[j].as_ptr().cast())),
                );
            }
            let idx = _mm_and_si128(_mm_srli_epi16::<3>(syms), _mm_set1_epi8(0x1F));
            let hi = _mm_cmpeq_epi8(_mm_and_si128(idx, _mm_set1_epi8(16)), _mm_set1_epi8(16));
            let byte = _mm_or_si128(
                _mm_and_si128(hi, _mm_shuffle_epi8(t1, idx)),
                _mm_andnot_si128(hi, _mm_shuffle_epi8(t0, idx)),
            );
            let bit = _mm_shuffle_epi8(
                _mm_loadu_si128(BITS.as_ptr().cast()),
                _mm_and_si128(syms, _mm_set1_epi8(7)),
            );
            let unmasked = _mm_cmpeq_epi8(_mm_and_si128(byte, bit), bit);
            _mm_and_si128(freqs, unmasked)
        }
    }

    #[target_feature(enable = "ssse3")]
    #[inline]
    fn hsum_ssse3(v: __m128i) -> u32 {
        let s = _mm_sad_epu8(v, _mm_setzero_si128());
        (_mm_cvtsi128_si32(s) + _mm_extract_epi16::<4>(s)) as u32
    }

    /// SSSE3: the unmasked frequency total of the states in `p[0..len]`.
    ///
    /// # Safety
    ///
    /// SSSE3 must be present and `p` valid for reading `len` bytes, a
    /// whole number of 6-byte states.
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn sum_ssse3(p: *const u8, len: usize, char_mask: &[u8; 256]) -> u32 {
        // SAFETY: every batch lies inside `p[0..len]`; SSSE3 is enabled.
        unsafe {
            let (t0, t1) = set(char_mask);
            let mut acc = _mm_setzero_si128();
            let mut s = 0;
            while s + BATCH_BYTES <= len {
                acc = _mm_add_epi64(
                    acc,
                    _mm_sad_epu8(batch_ssse3(p.add(s), t0, t1), _mm_setzero_si128()),
                );
                s += BATCH_BYTES;
            }
            let sum =
                (_mm_cvtsi128_si64(acc) + _mm_cvtsi128_si64(_mm_unpackhi_epi64(acc, acc))) as u32;
            sum + tail(p, s, len, char_mask)
        }
    }

    /// SSSE3: skips the whole batches of 16 states whose unmasked total
    /// `count` reaches, taking it off `count`; returns the bytes skipped.
    ///
    /// # Safety
    ///
    /// As [`sum_ssse3`].
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn skip_ssse3(
        p: *const u8,
        len: usize,
        char_mask: &[u8; 256],
        count: &mut u32,
    ) -> usize {
        // SAFETY: every batch lies inside `p[0..len]`; SSSE3 is enabled.
        unsafe {
            let (t0, t1) = set(char_mask);
            let mut s = 0;
            while s + BATCH_BYTES <= len {
                let b = hsum_ssse3(batch_ssse3(p.add(s), t0, t1));
                if *count < b {
                    break;
                }
                *count -= b;
                s += BATCH_BYTES;
            }
            s
        }
    }

    /// AVX2: the unmasked frequencies of the 32 states at `p`, states
    /// 0..16 in the low lane and 16..32 in the high.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn batch_avx2(p: *const u8, t0: __m256i, t1: __m256i) -> __m256i {
        // SAFETY: the caller guarantees 192 readable bytes at `p`; every
        // intrinsic is AVX2 or below, enabled here.
        unsafe {
            let mut syms = _mm256_setzero_si256();
            let mut freqs = _mm256_setzero_si256();
            for j in 0..6 {
                let lo = _mm_loadu_si128(p.add(16 * j).cast());
                let hi = _mm_loadu_si128(p.add(BATCH_BYTES + 16 * j).cast());
                let v = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(lo), hi);
                let sc = _mm256_broadcastsi128_si256(_mm_loadu_si128(SYM[j].as_ptr().cast()));
                let fc = _mm256_broadcastsi128_si256(_mm_loadu_si128(FREQ[j].as_ptr().cast()));
                syms = _mm256_or_si256(syms, _mm256_shuffle_epi8(v, sc));
                freqs = _mm256_or_si256(freqs, _mm256_shuffle_epi8(v, fc));
            }
            let idx = _mm256_and_si256(_mm256_srli_epi16::<3>(syms), _mm256_set1_epi8(0x1F));
            let hi = _mm256_cmpeq_epi8(
                _mm256_and_si256(idx, _mm256_set1_epi8(16)),
                _mm256_set1_epi8(16),
            );
            let byte = _mm256_blendv_epi8(
                _mm256_shuffle_epi8(t0, idx),
                _mm256_shuffle_epi8(t1, idx),
                hi,
            );
            let bits = _mm256_broadcastsi128_si256(_mm_loadu_si128(BITS.as_ptr().cast()));
            let bit = _mm256_shuffle_epi8(bits, _mm256_and_si256(syms, _mm256_set1_epi8(7)));
            let unmasked = _mm256_cmpeq_epi8(_mm256_and_si256(byte, bit), bit);
            _mm256_and_si256(freqs, unmasked)
        }
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    fn fold_avx2(acc: __m256i) -> u32 {
        let s = _mm_add_epi64(
            _mm256_castsi256_si128(acc),
            _mm256_extracti128_si256::<1>(acc),
        );
        (_mm_cvtsi128_si64(s) + _mm_cvtsi128_si64(_mm_unpackhi_epi64(s, s))) as u32
    }

    /// AVX2: as [`sum_ssse3`], 32 states per batch.
    ///
    /// # Safety
    ///
    /// AVX2 must be present; otherwise as [`sum_ssse3`].
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn sum_avx2(p: *const u8, len: usize, char_mask: &[u8; 256]) -> u32 {
        // SAFETY: every batch lies inside `p[0..len]`; AVX2 is enabled.
        unsafe {
            let (t0, t1) = set(char_mask);
            let (t0, t1) = (
                _mm256_broadcastsi128_si256(t0),
                _mm256_broadcastsi128_si256(t1),
            );
            let mut acc = _mm256_setzero_si256();
            let mut s = 0;
            while s + 2 * BATCH_BYTES <= len {
                let b = batch_avx2(p.add(s), t0, t1);
                acc = _mm256_add_epi64(acc, _mm256_sad_epu8(b, _mm256_setzero_si256()));
                s += 2 * BATCH_BYTES;
            }
            fold_avx2(acc) + tail(p, s, len, char_mask)
        }
    }

    /// AVX2: as [`skip_ssse3`], 32 states per batch.
    ///
    /// # Safety
    ///
    /// As [`sum_avx2`].
    #[target_feature(enable = "avx2")]
    pub(crate) unsafe fn skip_avx2(
        p: *const u8,
        len: usize,
        char_mask: &[u8; 256],
        count: &mut u32,
    ) -> usize {
        // SAFETY: every batch lies inside `p[0..len]`; AVX2 is enabled.
        unsafe {
            let (t0, t1) = set(char_mask);
            let (t0, t1) = (
                _mm256_broadcastsi128_si256(t0),
                _mm256_broadcastsi128_si256(t1),
            );
            let mut s = 0;
            while s + 2 * BATCH_BYTES <= len {
                let b = batch_avx2(p.add(s), t0, t1);
                let b = fold_avx2(_mm256_sad_epu8(b, _mm256_setzero_si256()));
                if *count < b {
                    break;
                }
                *count -= b;
                s += 2 * BATCH_BYTES;
            }
            s
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scalar(p: &[u8], mask: &[u8; 256]) -> u32 {
            p.chunks_exact(6)
                .map(|s| u32::from(s[1] & mask[usize::from(s[0])]))
                .sum()
        }

        fn scalar_skip(p: &[u8], mask: &[u8; 256], count: &mut u32, batch: usize) -> usize {
            let mut s = 0;
            while s + batch <= p.len() {
                let b = scalar(&p[s..s + batch], mask);
                if *count < b {
                    break;
                }
                *count -= b;
                s += batch;
            }
            s
        }

        /// Every tier this CPU has gives the scalar sums and skips on
        /// random states, masks and thresholds.
        #[test]
        fn tiers_match_the_scalar_walk() {
            let tier = Tier::detect();
            let mut seed = 0x2545_f491u32;
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            for round in 0..4000 {
                let ns = 1 + (next() % 256) as usize;
                let states: Vec<u8> = (0..ns * 6).map(|_| next() as u8).collect();
                let mut mask = [0u8; 256];
                for m in mask.iter_mut() {
                    *m = if next() % 3 == 0 { 0 } else { 0xFF };
                }
                let want = scalar(&states, &mask);
                let total = want + 1;
                let count = next() % total;
                let p = states.as_ptr();
                if tier != Tier::Scalar {
                    // SAFETY: SSSE3 is present; `states` holds `ns` states.
                    let got = unsafe { sum_ssse3(p, states.len(), &mask) };
                    assert_eq!(got, want, "ssse3 sum, round {round}");
                    let (mut c0, mut c1) = (count, count);
                    let w = scalar_skip(&states, &mask, &mut c0, 96);
                    // SAFETY: as above.
                    let g = unsafe { skip_ssse3(p, states.len(), &mask, &mut c1) };
                    assert_eq!((g, c1), (w, c0), "ssse3 skip, round {round}");
                }
                if tier == Tier::Avx2 {
                    // SAFETY: AVX2 is present; `states` holds `ns` states.
                    let got = unsafe { sum_avx2(p, states.len(), &mask) };
                    assert_eq!(got, want, "avx2 sum, round {round}");
                    let (mut c0, mut c1) = (count, count);
                    let w = scalar_skip(&states, &mask, &mut c0, 192);
                    // SAFETY: as above.
                    let g = unsafe { skip_avx2(p, states.len(), &mask, &mut c1) };
                    assert_eq!((g, c1), (w, c0), "avx2 skip, round {round}");
                }
            }
        }
    }
}
