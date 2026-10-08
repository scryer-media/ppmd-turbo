//! The RAR framing.
//!
//! RAR 2.9 through 4.x (the RAR3 format) can code a block with PPMd variant
//! H instead of LZ; RAR5 has no PPMd. A PPMd block header carries a reset
//! flag, the model order and the arena size in MiB, and the model lives on
//! across blocks and, in solid archives, across members. Its symbols are
//! carried by Dmitry Subbotin's carry-less range coder ([`RarRangeDecoder`]).
//!
//! [`RarDecoder`] owns that long-lived model. It decodes raw model symbols;
//! the escape character and the commands it introduces (switch to LZ, end of
//! file, VM filter code, match and run copies) belong to the RAR unpacker
//! that feeds it, which also parses the block header.
//!
//! Decoded output is identical to RARLAB unrar's.

use crate::error::{Error, Result};
use crate::model::Model;
use crate::rc::{RangeDecoder, RarRangeDecoder};

/// The largest arena a RAR PPMd block header can declare, in MiB: the header
/// stores `size - 1` in one byte.
pub const RAR_MAX_MEM_MB: u32 = 256;

/// Zero bytes a range decoder may be fed past the end of its input before
/// [`RarDecoder::decode_block`] calls the stream truncated.
///
/// The encoder flushes four bytes at the end of a block, so a well-formed
/// stream never needs more than a handful of padding bytes; a stream that
/// needs more ran out of data and would otherwise decode plausible symbols
/// for as long as the caller asked.
pub const MAX_ZERO_BYTES_PAST_EOF: u32 = 64;

/// The arena RAR's `CleanUp` restarts a model with after a corrupt symbol:
/// one MiB, at order 2.
const CLEANUP_MEM_SIZE: u32 = 1 << 20;
const CLEANUP_ORDER: u32 = 2;

/// PPMd decoder state for a RAR stream, persisted across blocks and solid
/// members.
#[derive(Default)]
pub struct RarDecoder {
    model: Option<Model>,
}

impl RarDecoder {
    /// A decoder with no model; the first block must carry the reset flag.
    pub fn new() -> Self {
        Self { model: None }
    }

    /// Starts the model for a block header with the reset flag set.
    ///
    /// `order` is the model order the header decodes to and `mem_mb` the
    /// arena size in MiB (the header's byte plus one). An existing arena of
    /// the same size is reused rather than reallocated, as unrar's
    /// `StartSubAllocator` does. Returns [`Error::InvalidParameters`] for an
    /// order outside `2..=64` or a size outside `1..=256` MiB, leaving any
    /// existing model untouched.
    pub fn init_model(&mut self, order: u32, mem_mb: u32) -> Result<()> {
        if !(1..=RAR_MAX_MEM_MB).contains(&mem_mb) {
            return Err(Error::InvalidParameters);
        }
        let mem_size = mem_mb << 20;
        match self.model.as_mut() {
            Some(model) => model.start(order, mem_size),
            None => {
                self.model = Some(Model::new(order, mem_size)?);
                Ok(())
            }
        }
    }

    /// Whether a model has been initialized.
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }

    /// The model's arena size in bytes, if there is a model.
    pub fn mem_size(&self) -> Option<u32> {
        self.model.as_ref().map(Model::mem_size)
    }

    /// Forgets the model, so the next block must carry the reset flag.
    pub fn reset(&mut self) {
        self.model = None;
    }

    /// RAR's `CleanUp` after a corrupt symbol: restart the model at order 2
    /// over a one-MiB arena, so a later block can decode safely.
    pub fn cleanup(&mut self) {
        match self.model.as_mut() {
            Some(model) => {
                let restarted = model.start(CLEANUP_ORDER, CLEANUP_MEM_SIZE);
                debug_assert!(restarted.is_ok());
            }
            None => self.model = Model::new(CLEANUP_ORDER, CLEANUP_MEM_SIZE).ok(),
        }
    }

    /// Decodes one model symbol through `rc`.
    ///
    /// `Ok(None)` is the model's end marker, which RAR reads as a corrupt
    /// symbol: unrar answers it with [`cleanup`](Self::cleanup) and falls
    /// back to LZ. Without a model this is [`Error::CorruptStream`].
    #[inline(always)]
    pub fn decode_symbol<D: RangeDecoder>(&mut self, rc: &mut D) -> Result<Option<u8>> {
        match self.model.as_mut() {
            Some(model) => model.decode_symbol(rc),
            None => Err(Error::CorruptStream {
                detail: "PPMd block without model initialization",
            }),
        }
    }

    /// Decodes one byte-aligned PPMd block.
    ///
    /// - `reset`: restart the model with `order` and `mem_mb` first (both
    ///   are ignored otherwise).
    /// - `rc_data`: the range-coded data, starting with the coder's four
    ///   initialization bytes.
    /// - `unpacked_remaining`: how many bytes to decode at most.
    /// - `out`: receives the decoded bytes.
    ///
    /// Decoding stops after `unpacked_remaining` bytes or at the model's end
    /// marker. Returns the number of bytes of `rc_data` the coder consumed.
    /// Empty `rc_data` decodes nothing and consumes nothing. Bytes are raw
    /// model symbols: escape-character commands are the caller's.
    ///
    /// Errors: [`Error::InvalidParameters`] for a bad `order` or `mem_mb`,
    /// [`Error::CorruptStream`] without a model or for a corrupt stream, and
    /// [`Error::Truncated`] when `rc_data` is shorter than the coder's
    /// initialization or runs out more than [`MAX_ZERO_BYTES_PAST_EOF`]
    /// bytes before the output is complete.
    pub fn decode_block(
        &mut self,
        reset: bool,
        order: u32,
        mem_mb: u32,
        rc_data: &[u8],
        unpacked_remaining: u64,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if rc_data.is_empty() {
            return Ok(0);
        }
        if reset {
            self.init_model(order, mem_mb)?;
        }
        let Some(model) = self.model.as_mut() else {
            return Err(Error::CorruptStream {
                detail: "PPMd block without model initialization",
            });
        };

        let mut rc = RarRangeDecoder::new(rc_data)?;
        let mut produced = 0u64;
        while produced < unpacked_remaining {
            let Some(byte) = model.decode_symbol(&mut rc)? else {
                break;
            };
            if rc.zero_bytes_past_eof() > MAX_ZERO_BYTES_PAST_EOF {
                return Err(Error::Truncated);
            }
            out.push(byte);
            produced += 1;
        }
        Ok(rc.position())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_decoder_has_no_model() {
        let decoder = RarDecoder::new();
        assert!(!decoder.has_model());
        assert_eq!(decoder.mem_size(), None);
    }

    #[test]
    fn a_block_without_init_is_corrupt() {
        let mut decoder = RarDecoder::new();
        let mut output = Vec::new();
        let result = decoder.decode_block(false, 0, 0, &[0u8; 5], 10, &mut output);
        assert!(matches!(result, Err(Error::CorruptStream { .. })));
    }

    #[test]
    fn a_block_with_init_decodes_without_error() {
        let mut decoder = RarDecoder::new();
        let mut output = Vec::new();
        let consumed = decoder
            .decode_block(true, 6, 1, &[0u8; 104], 5, &mut output)
            .unwrap();
        assert!(decoder.has_model());
        assert!(output.len() <= 5);
        assert!((4..=104).contains(&consumed));
    }

    #[test]
    fn empty_data_decodes_nothing_and_initializes_nothing() {
        let mut decoder = RarDecoder::new();
        let mut output = Vec::new();
        assert_eq!(
            decoder
                .decode_block(true, 6, 1, &[], 5, &mut output)
                .unwrap(),
            0
        );
        assert!(!decoder.has_model());
    }

    #[test]
    fn cleanup_restarts_at_order_two_over_one_mib() {
        let mut decoder = RarDecoder::new();
        decoder.init_model(16, 4).unwrap();
        decoder.cleanup();
        let model = decoder.model.as_ref().unwrap();
        assert_eq!(model.order(), 2);
        assert_eq!(model.mem_size(), 1 << 20);

        let mut fresh = RarDecoder::new();
        fresh.cleanup();
        assert_eq!(fresh.mem_size(), Some(1 << 20));
    }

    /// A same-size restart keeps the arena; a different size replaces it.
    #[test]
    fn init_model_reuses_a_same_sized_arena() {
        let mut decoder = RarDecoder::new();
        decoder.init_model(16, 1).unwrap();
        let arena = decoder.model.as_ref().unwrap().arena_addr();
        decoder.init_model(6, 1).unwrap();
        assert_eq!(decoder.model.as_ref().unwrap().arena_addr(), arena);
        assert_eq!(decoder.model.as_ref().unwrap().order(), 6);
        decoder.init_model(6, 2).unwrap();
        assert_eq!(decoder.mem_size(), Some(2 << 20));
    }

    #[test]
    fn reset_forgets_the_model() {
        let mut decoder = RarDecoder::new();
        decoder.init_model(6, 1).unwrap();
        decoder.reset();
        assert!(!decoder.has_model());
    }

    /// Hostile input through the public RAR API. Every case must come back
    /// as an `Err` (or a bounded `Ok`); none may panic, hang or grow memory.
    mod hostile {
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

        #[test]
        fn input_shorter_than_the_coder_init_is_truncated() {
            for len in 1..4 {
                let mut decoder = RarDecoder::new();
                let mut out = Vec::new();
                let result = decoder.decode_block(true, 6, 1, &[0xA5; 3][..len], 10, &mut out);
                assert!(matches!(result, Err(Error::Truncated)), "len {len}");
                assert!(out.is_empty());
            }
            assert!(matches!(
                RarRangeDecoder::new(&[1u8, 2, 3][..]),
                Err(Error::Truncated)
            ));
        }

        #[test]
        fn input_that_runs_dry_is_truncated_not_padded_forever() {
            for seed in [1, 2, 3, 0xDEAD_BEEF] {
                let data = noise(64, seed);
                let mut decoder = RarDecoder::new();
                let mut out = Vec::new();
                match decoder.decode_block(true, 8, 1, &data, u64::MAX, &mut out) {
                    Err(Error::Truncated) => {}
                    // The model may hit its end marker or a corrupt symbol first.
                    Ok(consumed) => assert!(consumed <= data.len()),
                    Err(Error::CorruptStream { .. }) => {}
                    Err(other) => panic!("seed {seed}: unexpected {other:?}"),
                }
                // However it ends, output is bounded by the padding allowance: each
                // symbol costs at least a fraction of a byte, and the coder gives up
                // after a fixed number of zero bytes past the end.
                assert!(
                    out.len() < (data.len() + MAX_ZERO_BYTES_PAST_EOF as usize) * 64,
                    "seed {seed}: {} bytes",
                    out.len()
                );
            }
        }

        #[test]
        fn out_of_range_parameters_are_rejected_and_keep_the_old_model() {
            let mut decoder = RarDecoder::new();
            for (order, mem_mb) in [
                (0, 1),
                (1, 1),
                (65, 1),
                (u32::MAX, 1),
                (6, 0),
                (6, RAR_MAX_MEM_MB + 1),
                (6, u32::MAX),
            ] {
                assert!(
                    matches!(
                        decoder.init_model(order, mem_mb),
                        Err(Error::InvalidParameters)
                    ),
                    "order {order} mem {mem_mb}"
                );
                let mut out = Vec::new();
                assert!(matches!(
                    decoder.decode_block(true, order, mem_mb, &[0; 8], 4, &mut out),
                    Err(Error::InvalidParameters)
                ));
            }
            assert!(!decoder.has_model());

            decoder.init_model(6, 2).unwrap();
            assert!(decoder.init_model(65, 2).is_err());
            assert_eq!(decoder.mem_size(), Some(2 << 20));
        }

        /// At the highest order a one-MiB arena fills on arbitrary input; the model
        /// restarts inside it and keeps decoding. Arbitrary input also strays outside
        /// the coder's interval every few thousand symbols; each time, decoding
        /// carries on with a fresh coder over the same, uncleaned model.
        #[test]
        fn arena_exhaustion_and_reuse_after_errors_never_panic() {
            let data = noise(1 << 20, 0x9E37_79B9);
            let mut decoder = RarDecoder::new();
            decoder.init_model(64, 1).unwrap();
            let mut offset = 0;
            let mut decoded = 0usize;
            let mut errors = 0usize;
            while data.len() - offset >= 4 {
                let mut rc = RarRangeDecoder::new(&data[offset..]).unwrap();
                loop {
                    match decoder.decode_symbol(&mut rc) {
                        Ok(Some(_)) => decoded += 1,
                        Ok(None) => decoder.init_model(64, 1).unwrap(),
                        Err(Error::CorruptStream { .. }) => {
                            errors += 1;
                            break;
                        }
                        Err(other) => panic!("unexpected {other:?}"),
                    }
                    if rc.zero_bytes_past_eof() != 0 {
                        break;
                    }
                }
                offset += rc.position().max(1);
            }
            assert!(decoded > 250_000, "decoded {decoded}");
            assert!(errors > 0);
            assert_eq!(decoder.mem_size(), Some(1 << 20));
        }

        /// Every block resets the model with a different order or arena size.
        #[test]
        fn restart_storms_never_panic() {
            let data = noise(1 << 12, 0x0BAD_5EED);
            let mut decoder = RarDecoder::new();
            let mut out = Vec::new();
            for round in 0..500u32 {
                let order = 2 + round % 63;
                let mem_mb = 1 + round % 3;
                let start = (round as usize * 7) % 2048;
                let reset = round % 5 != 4;
                let _ = decoder.decode_block(reset, order, mem_mb, &data[start..], 32, &mut out);
                if round % 11 == 0 {
                    decoder.cleanup();
                }
                if round % 13 == 0 {
                    decoder.reset();
                }
            }
            decoder.init_model(6, 1).unwrap();
            out.clear();
            decoder
                .decode_block(false, 0, 0, &data, 64, &mut out)
                .unwrap();
            assert!(!out.is_empty());
        }

        #[test]
        fn symbols_without_a_model_are_a_corrupt_stream() {
            let mut decoder = RarDecoder::new();
            let mut rc = RarRangeDecoder::new(&[0u8; 8][..]).unwrap();
            assert!(matches!(
                decoder.decode_symbol(&mut rc),
                Err(Error::CorruptStream { .. })
            ));
        }

        /// Arbitrary blocks never panic.
        #[test]
        fn arbitrary_blocks_never_panic() {
            let mut decoder = RarDecoder::new();
            let mut out = Vec::new();
            for seed in 1..200u32 {
                let data = noise(256, seed.wrapping_mul(0x9E37_79B9));
                out.clear();
                let _ =
                    decoder.decode_block(seed % 3 != 0, seed % 70, seed % 4, &data, 4096, &mut out);
            }
        }
    }
}
