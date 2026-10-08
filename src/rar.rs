//! The RAR framing.
//!
//! RAR 2.9 through 4.x (the RAR3 format) can code a block with PPMd variant
//! H instead of LZ; RAR5 has no PPMd. A PPMd block header carries a reset
//! flag, the model order and the arena size in MiB; the model and the
//! carry-less range coder's registers live on across blocks and, in solid
//! archives, across members.
//!
//! [`RarPpmd`] owns that long-lived state. The RAR unpacker that feeds it
//! parses the block header and handles everything the escape character
//! introduces (switch to LZ, end of file, VM filter code, match and run
//! copies): [`decode`](RarPpmd::decode) runs literals up to the escape, and
//! [`next_symbol`](RarPpmd::next_symbol) reads the command bytes after it.
//! Consumed counts are exact, so a switch to LZ hands the bit reader back
//! the coder's exact byte.
//!
//! Decoded output is identical to RARLAB unrar's.

use alloc_crate::boxed::Box;

use crate::arena::try_box;
use crate::engine::run::{self, Padding, Stop};
use crate::error::{Error, ErrorKind, Position, Progress, Result};
use crate::model::Model;
use crate::params::{MAX_INPUT_PER_SYMBOL, Params, carryless_margin};
use crate::rc::{CarrylessRangeDecoder, RangeCoderState, SliceInput};

/// RAR's `CleanUp` parameters after a corrupt symbol: order 2 over a
/// one-MiB arena.
const CLEANUP: Params = match Params::new(2, 1 << 20) {
    Ok(p) => p,
    Err(_) => panic!("cleanup parameters are valid"),
};

/// Zeros past the input a RAR stream may need by default before it counts
/// as truncated. The encoder flushes four bytes at the end of a block, so a
/// well-formed stream needs only a handful.
const DEFAULT_PADDING_ALLOWANCE: u32 = 64;

/// Coder initialization bytes at the start of a PPMd block.
const INIT: usize = 4;

/// Why [`RarPpmd::decode`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RarStatus {
    /// More input is needed: present the unconsumed bytes again, followed
    /// by more, or say the input is the last.
    NeedInput,
    /// The output slice is full.
    OutputFull,
    /// The escape character was decoded. It is consumed and not written;
    /// read the command after it with [`RarPpmd::next_symbol`].
    Escape,
    /// The model's end marker, which RAR reads as a corrupt symbol: unrar
    /// answers it with [`RarPpmd::cleanup`] and ends the member. Every later
    /// call returns this again until the model is restarted.
    ModelEnd,
}

/// What [`RarPpmd::next_symbol`] read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Symbol {
    /// A symbol.
    Byte(u8),
    /// More input is needed, as [`RarStatus::NeedInput`].
    NeedInput,
    /// The model's end marker, as [`RarStatus::ModelEnd`].
    ModelEnd,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Ready,
    ModelEnd,
    Failed(Error),
}

/// PPMd decoder state for a RAR stream, persisted across blocks and solid
/// members.
///
/// A non-final call needs `4 * (order + 2)` bytes of input to decode a
/// symbol ([`max_input_per_symbol`](Self::max_input_per_symbol)). After an
/// error the decoder is poisoned: every later call repeats the error until
/// [`start_block`](Self::start_block) with `Some` parameters,
/// [`cleanup`](Self::cleanup) or [`forget`](Self::forget). Error positions
/// count input bytes and decoded symbols (written literals, escapes and
/// [`next_symbol`](Self::next_symbol) symbols) since the last
/// `start_block`.
///
/// ```
/// use ppmd_turbo::{Params, RarPpmd, RarStatus};
///
/// let mut ppmd = RarPpmd::new();
/// ppmd.start_block(Some(Params::rar(6, 1)?))?;
/// let mut out = [0u8; 16];
/// let step = ppmd.decode(&[0u8; 64], true, &mut out, 2)?;
/// assert!(step.consumed <= 64);
/// assert_ne!(step.status, RarStatus::NeedInput);
/// # Ok::<(), ppmd_turbo::Error>(())
/// ```
pub struct RarPpmd {
    model: Option<Box<Model>>,
    regs: RangeCoderState,
    awaiting_init: bool,
    state: State,
    padding: u32,
    allowance: u32,
    limit: u64,
    total_in: u64,
    total_out: u64,
}

impl Default for RarPpmd {
    fn default() -> Self {
        Self::new()
    }
}

impl RarPpmd {
    /// A decoder with no model; the first block must carry the reset flag.
    /// It has no arena limit and a padding allowance of 64 zeros.
    pub const fn new() -> Self {
        Self {
            model: None,
            regs: RangeCoderState {
                low: 0,
                code: 0,
                range: u32::MAX,
            },
            awaiting_init: true,
            state: State::Ready,
            padding: 0,
            allowance: DEFAULT_PADDING_ALLOWANCE,
            limit: u64::MAX,
            total_in: 0,
            total_out: 0,
        }
    }

    /// The most heap a model may take ([`Params::memory_footprint`]); a
    /// block header asking for more fails [`start_block`](Self::start_block)
    /// with [`ErrorKind::MemoryLimit`]. A block header can ask for 256 MiB.
    pub fn set_arena_limit(&mut self, bytes: u64) {
        self.limit = bytes;
    }

    /// Zeros past the end of the last input the coder may read before the
    /// stream counts as truncated (default 64). A byte decoded with more
    /// padding than this is not output.
    pub fn set_padding_allowance(&mut self, zeros: u32) {
        self.allowance = zeros;
    }

    /// The caller parsed a PPMd block header. `Some` is the reset flag: the
    /// model restarts with these parameters, keeping its arena when it fits
    /// (as unrar's `StartSubAllocator` does). `None` continues the current
    /// model. Either way the coder reads its four initialization bytes from
    /// the next input, and the padding and position counts restart.
    ///
    /// Errors: [`ErrorKind::MemoryLimit`] above the arena limit,
    /// [`ErrorKind::AllocationFailed`], and a corrupt stream for `None`
    /// without a model. `None` on a poisoned decoder repeats its error.
    pub fn start_block(&mut self, reset: Option<Params>) -> Result<()> {
        self.awaiting_init = true;
        self.padding = 0;
        self.total_in = 0;
        self.total_out = 0;
        match reset {
            Some(params) => {
                self.state = State::Ready;
                if let Err(e) = self.restart(params) {
                    self.state = State::Failed(e);
                    return Err(e);
                }
            }
            None => {
                if let State::Failed(e) = self.state {
                    return Err(e);
                }
                if self.model.is_none() {
                    let e = Error::corrupt("PPMd block without model initialization");
                    self.state = State::Failed(e);
                    return Err(e);
                }
                self.state = State::Ready;
            }
        }
        Ok(())
    }

    fn restart(&mut self, params: Params) -> Result<()> {
        let required = params.memory_footprint();
        if required > self.limit {
            return Err(Error::new(ErrorKind::MemoryLimit {
                required,
                limit: self.limit,
            }));
        }
        match self.model.as_mut() {
            Some(model) => model.start(params.order(), params.mem_size()),
            None => {
                let model = Model::new(params.order(), params.mem_size())?;
                self.model = Some(try_box(model)?);
                Ok(())
            }
        }
    }

    /// Whether a model has been started.
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }

    /// RAR's `CleanUp` after a corrupt symbol or the model's end marker:
    /// restarts the model at order 2 over a one-MiB arena so a later block
    /// can decode safely, and clears an earlier error. The next block must
    /// still call [`start_block`](Self::start_block).
    ///
    /// Errors: [`ErrorKind::MemoryLimit`] when the limit is below the
    /// cleanup model's footprint, [`ErrorKind::AllocationFailed`]; the
    /// decoder is then poisoned with it.
    pub fn cleanup(&mut self) -> Result<()> {
        self.awaiting_init = true;
        self.state = State::Ready;
        if let Err(e) = self.restart(CLEANUP) {
            self.state = State::Failed(e);
            return Err(e);
        }
        Ok(())
    }

    /// Drops the model and clears any error, so the next block must carry
    /// the reset flag.
    pub fn forget(&mut self) {
        self.model = None;
        self.awaiting_init = true;
        self.state = State::Ready;
        self.padding = 0;
    }

    /// Zeros past the end of the input the coder has read since the last
    /// [`start_block`](Self::start_block).
    pub fn padding_used(&self) -> u32 {
        self.padding
    }

    /// Heap bytes the decoder holds: the model's arena allocation and its
    /// tables, or 0 without a model.
    pub fn memory_footprint(&self) -> u64 {
        self.model.as_ref().map_or(0, |m| {
            m.arena_capacity() as u64 + core::mem::size_of::<Model>() as u64
        })
    }

    /// The input a non-final call needs to decode a symbol with the current
    /// model, `4 * (order + 2)` bytes; [`MAX_INPUT_PER_SYMBOL`] without one.
    pub fn max_input_per_symbol(&self) -> usize {
        self.model
            .as_ref()
            .map_or(MAX_INPUT_PER_SYMBOL, |m| carryless_margin(m.order() as u8))
    }

    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn arena_addr(&self) -> Option<usize> {
        self.model.as_ref().map(|m| m.arena_addr())
    }

    #[cold]
    fn fail(&mut self, e: Error) -> Error {
        let e = e.at(Position {
            input: self.total_in,
            output: self.total_out,
        });
        self.state = State::Failed(e);
        e
    }

    /// Checks the state and reads the init bytes. `Ok(None)` is a status to
    /// return with the bytes consumed so far; `Ok(Some(n))` means decode
    /// from `input[n..]`.
    fn prepare(
        &mut self,
        input: &[u8],
        input_is_last: bool,
    ) -> Result<core::result::Result<usize, Stopped>> {
        match self.state {
            State::Failed(e) => return Err(e),
            State::ModelEnd => return Ok(Err(Stopped::ModelEnd(0))),
            State::Ready => {}
        }
        if self.model.is_none() {
            return Err(self.fail(Error::corrupt("PPMd block without model initialization")));
        }
        if !self.awaiting_init {
            return Ok(Ok(0));
        }
        let Some(init) = input.get(..INIT) else {
            if input_is_last {
                return Err(self.fail(Error::truncated()));
            }
            return Ok(Err(Stopped::NeedInput(0)));
        };
        match CarrylessRangeDecoder::new(init) {
            Ok(rc) => self.regs = rc.state(),
            Err(e) => return Err(self.fail(e)),
        }
        self.awaiting_init = false;
        self.total_in += INIT as u64;
        Ok(Ok(INIT))
    }

    /// Decodes literals from `input` into `out` until the escape character
    /// `esc`, which is consumed and not written ([`RarStatus::Escape`]).
    ///
    /// Unless `input_is_last`, the decoder decodes only while at least
    /// [`max_input_per_symbol`](Self::max_input_per_symbol) bytes remain and
    /// then returns [`RarStatus::NeedInput`]; present the bytes after
    /// `consumed` again on the next call. With `input_is_last`, the coder
    /// reads zeros past the input up to the padding allowance.
    ///
    /// Errors: a corrupt stream without a model or for a corrupt symbol;
    /// truncation when the block's four initialization bytes are missing
    /// from the last input, a byte needs more padding than allowed, or the
    /// end marker is met on padding.
    pub fn decode(
        &mut self,
        input: &[u8],
        input_is_last: bool,
        out: &mut [u8],
        esc: u8,
    ) -> Result<Progress<RarStatus>> {
        let mut consumed = match self.prepare(input, input_is_last)? {
            Ok(n) => n,
            Err(stopped) => return Ok(stopped.progress()),
        };
        if out.is_empty() {
            return Ok(progress(consumed, 0, RarStatus::OutputFull));
        }
        let Some(model) = self.model.as_deref_mut() else {
            unreachable!("prepare checked the model");
        };
        let rest = &input[consumed..];
        let margin = carryless_margin(model.order() as u8);
        let mut rc = CarrylessRangeDecoder::from_state(SliceInput::new(rest), self.regs);
        let (mut produced, mut stop) = match rest.len().checked_sub(margin) {
            Some(fast_end) => run::fast::<_, true>(model, &mut rc, out, fast_end, esc),
            None => (0, Ok(Stop::Margin)),
        };
        if input_is_last && stop == Ok(Stop::Margin) {
            let rule = Padding {
                before: self.padding,
                allowance: self.allowance,
                truncates_errors: false,
            };
            let (more, edge_stop) =
                run::edge::<_, true>(model, &mut rc, &mut out[produced..], esc, rule);
            produced += more;
            stop = edge_stop;
        }
        let padded = rc.zero_bytes_past_eof();
        consumed += rc.position();
        self.regs = rc.state();
        self.padding = self.padding.saturating_add(padded);
        self.total_in += rc.position() as u64;
        self.total_out += produced as u64;
        let status = match stop {
            Err(e) => return Err(self.fail(e)),
            Ok(Stop::Margin) => RarStatus::NeedInput,
            Ok(Stop::OutputFull) => RarStatus::OutputFull,
            Ok(Stop::Escape) => {
                self.total_out += 1;
                RarStatus::Escape
            }
            Ok(Stop::EndMarker) => {
                if self.padding != 0 {
                    return Err(self.fail(Error::truncated()));
                }
                self.state = State::ModelEnd;
                RarStatus::ModelEnd
            }
        };
        Ok(progress(consumed, produced, status))
    }

    /// Decodes one symbol: the command bytes after an escape, VM code. The
    /// cold path; [`decode`](Self::decode) runs literals. Returns the input
    /// bytes consumed and the symbol, with the stop rules and errors of
    /// `decode`.
    pub fn next_symbol(&mut self, input: &[u8], input_is_last: bool) -> Result<(usize, Symbol)> {
        let mut consumed = match self.prepare(input, input_is_last)? {
            Ok(n) => n,
            Err(stopped) => return Ok(stopped.symbol()),
        };
        let Some(model) = self.model.as_deref_mut() else {
            unreachable!("prepare checked the model");
        };
        let rest = &input[consumed..];
        let margin = carryless_margin(model.order() as u8);
        if !input_is_last && rest.len() < margin {
            return Ok((consumed, Symbol::NeedInput));
        }
        let mut rc = CarrylessRangeDecoder::from_state(SliceInput::new(rest), self.regs);
        let rule = Padding {
            before: self.padding,
            allowance: self.allowance,
            truncates_errors: false,
        };
        let mut byte = [0u8; 1];
        let (_, stop) = run::edge::<_, false>(model, &mut rc, &mut byte, 0, rule);
        consumed += rc.position();
        self.regs = rc.state();
        self.padding = self.padding.saturating_add(rc.zero_bytes_past_eof());
        self.total_in += rc.position() as u64;
        match stop {
            Err(e) => Err(self.fail(e)),
            Ok(Stop::EndMarker) => {
                if self.padding != 0 {
                    return Err(self.fail(Error::truncated()));
                }
                self.state = State::ModelEnd;
                Ok((consumed, Symbol::ModelEnd))
            }
            Ok(_) => {
                self.total_out += 1;
                Ok((consumed, Symbol::Byte(byte[0])))
            }
        }
    }
}

/// A status `prepare` returns before decoding.
enum Stopped {
    NeedInput(usize),
    ModelEnd(usize),
}

impl Stopped {
    fn progress(self) -> Progress<RarStatus> {
        match self {
            Self::NeedInput(n) => progress(n, 0, RarStatus::NeedInput),
            Self::ModelEnd(n) => progress(n, 0, RarStatus::ModelEnd),
        }
    }

    fn symbol(self) -> (usize, Symbol) {
        match self {
            Self::NeedInput(n) => (n, Symbol::NeedInput),
            Self::ModelEnd(n) => (n, Symbol::ModelEnd),
        }
    }
}

#[inline]
fn progress(consumed: usize, produced: usize, status: RarStatus) -> Progress<RarStatus> {
    Progress {
        consumed,
        produced,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

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

    fn rar(order: u32, mb: u32) -> Params {
        Params::rar(order, mb).unwrap()
    }

    /// Decodes up to `limit` literals of a whole block (escapes included,
    /// as literals) from one last input.
    fn decode_all(ppmd: &mut RarPpmd, data: &[u8], limit: usize) -> Result<(usize, Vec<u8>)> {
        let mut out = vec![0u8; limit];
        let mut produced = 0;
        let mut consumed = 0;
        while produced < limit {
            let step = ppmd.decode(&data[consumed..], true, &mut out[produced..], 0)?;
            produced += step.produced;
            consumed += step.consumed;
            match step.status {
                RarStatus::Escape => {
                    out[produced] = 0;
                    produced += 1;
                }
                RarStatus::ModelEnd | RarStatus::NeedInput => break,
                RarStatus::OutputFull => {}
            }
        }
        out.truncate(produced);
        Ok((consumed, out))
    }

    #[test]
    fn a_new_decoder_has_no_model() {
        let ppmd = RarPpmd::new();
        assert!(!ppmd.has_model());
        assert_eq!(ppmd.memory_footprint(), 0);
        assert_eq!(ppmd.max_input_per_symbol(), MAX_INPUT_PER_SYMBOL);
    }

    #[test]
    fn a_block_without_a_model_is_corrupt_and_poisons() {
        let mut ppmd = RarPpmd::new();
        let e = ppmd.start_block(None).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::Corrupt(_)));
        assert_eq!(ppmd.decode(&[0; 8], true, &mut [0; 4], 2), Err(e));
        assert_eq!(ppmd.next_symbol(&[0; 8], true), Err(e));
        ppmd.forget();
        let e = ppmd.decode(&[0; 8], true, &mut [0; 4], 2).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::Corrupt(_)));
    }

    #[test]
    fn a_block_with_init_decodes_without_error() {
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        let (consumed, out) = decode_all(&mut ppmd, &[0u8; 104], 5).unwrap();
        assert!(ppmd.has_model());
        assert!(out.len() <= 5);
        assert!((4..=104).contains(&consumed));
    }

    #[test]
    fn short_non_final_input_needs_more_and_takes_nothing() {
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        let step = ppmd.decode(&[0; 3], false, &mut [0; 4], 2).unwrap();
        assert_eq!((step.consumed, step.status), (0, RarStatus::NeedInput));
        let margin = ppmd.max_input_per_symbol();
        assert_eq!(margin, 32);
        let step = ppmd
            .decode(&vec![0; INIT + margin - 1], false, &mut [0; 4], 2)
            .unwrap();
        assert_eq!(
            (step.consumed, step.produced, step.status),
            (INIT, 0, RarStatus::NeedInput)
        );
        let (n, sym) = ppmd.next_symbol(&vec![0; margin - 1], false).unwrap();
        assert_eq!((n, sym), (0, Symbol::NeedInput));
    }

    #[test]
    fn cleanup_restarts_at_order_two_over_one_mib() {
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(16, 4))).unwrap();
        ppmd.cleanup().unwrap();
        let model = ppmd.model.as_ref().unwrap();
        assert_eq!(model.order(), 2);
        assert_eq!(model.mem_size(), 1 << 20);

        let mut fresh = RarPpmd::new();
        fresh.cleanup().unwrap();
        assert_eq!(fresh.model.as_ref().unwrap().mem_size(), 1 << 20);
    }

    /// A same-size restart keeps the arena.
    #[test]
    fn start_block_reuses_a_fitting_arena() {
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(16, 1))).unwrap();
        let arena = ppmd.arena_addr();
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        assert_eq!(ppmd.arena_addr(), arena);
        assert_eq!(ppmd.model.as_ref().unwrap().order(), 6);
        ppmd.start_block(Some(rar(6, 4))).unwrap();
        assert_eq!(ppmd.model.as_ref().unwrap().mem_size(), 4 << 20);
    }

    #[test]
    fn the_arena_limit_refuses_large_blocks() {
        let mut ppmd = RarPpmd::new();
        ppmd.set_arena_limit(2 << 20);
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        let e = ppmd.start_block(Some(rar(6, 2))).unwrap_err();
        let ErrorKind::MemoryLimit { required, limit } = e.kind else {
            panic!("{e:?}");
        };
        assert_eq!((required, limit), (rar(6, 2).memory_footprint(), 2 << 20));
        assert_eq!(ppmd.start_block(None), Err(e));
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        ppmd.set_arena_limit(1 << 19);
        assert!(matches!(
            ppmd.cleanup().unwrap_err().kind,
            ErrorKind::MemoryLimit { .. }
        ));
    }

    #[test]
    fn forget_drops_the_model() {
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        ppmd.forget();
        assert!(!ppmd.has_model());
    }

    #[test]
    fn input_shorter_than_the_coder_init_is_truncated() {
        for len in 0..4 {
            let mut ppmd = RarPpmd::new();
            ppmd.start_block(Some(rar(6, 1))).unwrap();
            let e = ppmd.decode(&[0xA5; 3][..len], true, &mut [0; 10], 2);
            assert_eq!(e.unwrap_err().kind, ErrorKind::Truncated, "len {len}");
        }
    }

    #[test]
    fn noise_runs_dry_within_the_allowance() {
        for seed in [1, 2, 3, 0xDEAD_BEEF] {
            let data = noise(64, seed);
            let mut ppmd = RarPpmd::new();
            ppmd.start_block(Some(rar(8, 1))).unwrap();
            match decode_all(&mut ppmd, &data, 1 << 20) {
                Ok((consumed, out)) => {
                    assert!(consumed <= data.len());
                    assert!(out.len() < (data.len() + 64) * 64);
                }
                Err(e) => assert!(e.is_data_error(), "seed {seed}: {e:?}"),
            }
        }
    }

    /// At the highest order a one-MiB arena fills on arbitrary input; the
    /// model restarts inside it and keeps decoding. Arbitrary input also
    /// decodes to the model's end marker (or, rarely, out of the coder's
    /// interval) every thousand symbols or so; each time, decoding carries
    /// on with a fresh block.
    #[test]
    fn arena_exhaustion_and_reuse_after_errors_never_panic() {
        let data = noise(1 << 20, 0x9E37_79B9);
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(64, 1))).unwrap();
        let mut offset = 0;
        let mut block_start = 0;
        let mut decoded = 0usize;
        let mut errors = 0usize;
        let mut ends = 0usize;
        let mut out = vec![0u8; 4096];
        while data.len() - offset >= 4 + 264 {
            match ppmd.decode(&data[offset..], false, &mut out, 0) {
                Ok(step) => {
                    decoded += step.produced;
                    offset += step.consumed;
                    match step.status {
                        RarStatus::ModelEnd => {
                            ends += 1;
                            ppmd.start_block(Some(rar(64, 1))).unwrap();
                            block_start = offset;
                        }
                        RarStatus::NeedInput => break,
                        _ => {}
                    }
                }
                Err(e) => {
                    assert!(matches!(e.kind, ErrorKind::Corrupt(_)), "{e:?}");
                    errors += 1;
                    offset = block_start + (e.at.input as usize).max(1);
                    block_start = offset;
                    // Carry on over the same, uncleaned model: clear the
                    // poison and re-arm the coder.
                    ppmd.state = State::Ready;
                    ppmd.start_block(None).unwrap();
                }
            }
        }
        assert!(decoded > 250_000, "decoded {decoded}");
        // One coder runs across the whole input, so it rarely leaves its
        // interval; the model's end marker stops it instead.
        assert!(errors + ends > 0, "errors {errors} ends {ends}");
        assert_eq!(ppmd.model.as_ref().unwrap().mem_size(), 1 << 20);
    }

    /// Every block resets the model with a different order or arena size.
    #[test]
    fn restart_storms_never_panic() {
        let data = noise(1 << 12, 0x0BAD_5EED);
        let mut ppmd = RarPpmd::new();
        let mut out = [0u8; 32];
        for round in 0..500u32 {
            let start = (round as usize * 7) % 2048;
            let reset = (round % 5 != 4).then(|| rar(2 + round % 63, 1 + round % 3));
            if ppmd.start_block(reset).is_ok() {
                let _ = ppmd.decode(&data[start..], round % 2 == 0, &mut out, round as u8);
                let _ = ppmd.next_symbol(&data[start..], round % 3 == 0);
            }
            if round % 11 == 0 {
                ppmd.cleanup().unwrap();
            }
            if round % 13 == 0 {
                ppmd.forget();
            }
        }
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        let step = ppmd.decode(&data, true, &mut out, 0).unwrap();
        assert!(step.produced > 0);
    }

    /// Feeding a block one byte at a time decodes exactly what one call
    /// decodes, and consumes exactly as much.
    #[test]
    fn byte_at_a_time_matches_one_shot() {
        let data = noise(4096, 0x1234_5678);
        let one_shot = {
            let mut ppmd = RarPpmd::new();
            ppmd.start_block(Some(rar(6, 1))).unwrap();
            let mut out = vec![0u8; 2048];
            let r = ppmd.decode(&data, true, &mut out, 0x55);
            (r.map(|s| (s.consumed, s.status)), out)
        };
        let mut ppmd = RarPpmd::new();
        ppmd.start_block(Some(rar(6, 1))).unwrap();
        let mut out = vec![0u8; 2048];
        let (mut have, mut consumed, mut produced) = (0usize, 0usize, 0usize);
        let result = loop {
            let last = have == data.len();
            match ppmd.decode(&data[consumed..have], last, &mut out[produced..], 0x55) {
                Ok(step) => {
                    consumed += step.consumed;
                    produced += step.produced;
                    if step.status == RarStatus::NeedInput && !last {
                        have += 1;
                        continue;
                    }
                    break Ok((consumed, step.status));
                }
                Err(e) => break Err(e),
            }
        };
        assert_eq!(result, one_shot.0);
        assert_eq!(out, one_shot.1);
    }
}
