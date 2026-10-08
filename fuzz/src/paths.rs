//! F5 `checked_vs_unchecked`: the fast decode path against the checked one.
//!
//! The same stream decodes twice, once through the fastest path the step
//! API offers and once through the slowest path the engine offers, and the
//! two must agree on every byte, every verdict and every input position:
//!
//! - **fast**: [`SevenZDecoder`] or [`RarPpmd`] given the whole input in
//!   one call, so every symbol away from the end runs through the engine's
//!   unchecked fast loop over a borrowed slice;
//! - **checked**: an internals [`Model`] driven one
//!   [`Model::decode_symbol`] call at a time through a range coder reading a
//!   [`Trickle`], an input that refills a 1- to 8-byte window one byte at a
//!   time, with the stream's framing rules (initialization bytes, padding,
//!   the end marker, the known size) spelled out symbol by symbol.
//!
//! The model code is the same on both sides; they differ in the input
//! layer, the decode loops, the padding and margin bookkeeping and the
//! coder's monomorphization. A disagreement is a broken invariant in the
//! fast loop or the framing.
//!
//! Input: one mode byte (bit 0: 7z, else RAR; bits 1..=3: the refill window
//! minus one; bit 4: 7-Zip's `FinishStream` for the 7z mode), then a
//! [`RarBlock`] sequence or a [`Decode7z`] case.

use ppmd_turbo::internals::{
    CarrylessRangeDecoder, IntoRangeInput, Model, RangeInput, SevenZipRangeDecoder,
};
use ppmd_turbo::{Params, RarPpmd, RarStatus, SevenZDecoder, SevenZStatus};

use crate::layout::{Decode7z, RarBlock};
use crate::outcome::{ErrKind, classify};
use crate::params::OUTPUT_CAP;

/// The most symbols one 7z case decodes on each side. The decode targets
/// cover long runs; this one runs every input twice, once symbol by symbol.
pub const MAX_SYMBOLS_7Z: usize = 1 << 16;

/// The escape byte the RAR fast side decodes with; it puts escapes back as
/// literals, so both sides produce the raw symbol stream.
const ESC: u8 = 2;

/// [`RarPpmd`]'s default padding allowance.
const RAR_PADDING: u32 = 64;

/// A range input over a slice that copies it through a small window, one
/// byte per refill step, so every byte crosses a refill edge.
#[derive(Debug)]
pub struct Trickle<'a> {
    data: &'a [u8],
    taken: usize,
    window: [u8; 8],
    len: usize,
    at: usize,
    cap: usize,
    zeros: u32,
}

impl<'a> Trickle<'a> {
    /// `data` through a window of `cap` (1..=8) bytes.
    pub fn new(data: &'a [u8], cap: usize) -> Self {
        assert!((1..=8).contains(&cap));
        Self {
            data,
            taken: 0,
            window: [0; 8],
            len: 0,
            at: 0,
            cap,
            zeros: 0,
        }
    }

    fn refill(&mut self) {
        self.len = 0;
        self.at = 0;
        while self.len < self.cap {
            let Some((&b, rest)) = self.data.split_first() else {
                break;
            };
            self.window[self.len] = b;
            self.len += 1;
            self.data = rest;
        }
    }
}

impl RangeInput for Trickle<'_> {
    fn next_byte(&mut self) -> u8 {
        if self.at == self.len {
            self.refill();
        }
        if self.at == self.len {
            self.zeros = self.zeros.saturating_add(1);
            return 0;
        }
        let b = self.window[self.at];
        self.at += 1;
        self.taken += 1;
        b
    }

    fn position(&self) -> usize {
        self.taken
    }

    fn zero_bytes_past_eof(&self) -> u32 {
        self.zeros
    }
}

impl<'a> IntoRangeInput for Trickle<'a> {
    type Input = Self;

    fn into_range_input(self) -> Self {
        self
    }
}

/// One side's result for a block or a stream: the verdict and the input
/// bytes taken.
type Side = (Result<Verdict, ErrKind>, usize);

/// How a decode stopped without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The block's or stream's size (or the harness's cap) was reached.
    Full,
    /// The model's end marker.
    End,
}

/// The RAR fast side: one block through [`RarPpmd`], whole input per call.
fn fast_block(rar: &mut RarPpmd, block: &RarBlock<'_>, out: &mut Vec<u8>) -> Side {
    if block.rc_data.is_empty() {
        return (Ok(Verdict::Full), 0);
    }
    let params = if block.reset {
        match Params::rar(block.order, block.mem_mb) {
            Ok(p) => Some(p),
            Err(e) => return (Err(classify(&e)), 0),
        }
    } else {
        None
    };
    if let Err(e) = rar.start_block(params) {
        return (Err(classify(&e)), 0);
    }
    let limit = usize::try_from(block.unpacked_remaining)
        .unwrap_or(usize::MAX)
        .min(OUTPUT_CAP);
    let mut buf = vec![0u8; limit];
    let mut pos = 0;
    while out.len() < limit {
        let before = out.len();
        match rar.decode(&block.rc_data[pos..], true, &mut buf[..limit - before], ESC) {
            Ok(step) => {
                assert!(
                    step.produced > 0 || step.status != RarStatus::OutputFull,
                    "OutputFull with room left"
                );
                out.extend_from_slice(&buf[..step.produced]);
                pos += step.consumed;
                match step.status {
                    RarStatus::Escape => out.push(ESC),
                    RarStatus::OutputFull => {}
                    RarStatus::ModelEnd => return (Ok(Verdict::End), pos),
                    other => panic!("{other:?} on the last input"),
                }
            }
            Err(e) => {
                // The position counts the symbols since `start_block`.
                let valid = usize::try_from(e.at.output).expect("fits") - before;
                out.extend_from_slice(&buf[..valid]);
                return (Err(classify(&e)), e.at.input as usize);
            }
        }
    }
    (Ok(Verdict::Full), pos)
}

/// The RAR checked side: [`RarPpmd`]'s block contract over a bare model,
/// one symbol at a time.
#[derive(Default)]
struct CheckedRar {
    model: Option<Model>,
    /// The error a block without the reset flag repeats.
    failed: Option<ErrKind>,
}

impl CheckedRar {
    fn block(&mut self, block: &RarBlock<'_>, refill: usize, out: &mut Vec<u8>) -> Side {
        if block.rc_data.is_empty() {
            return (Ok(Verdict::Full), 0);
        }
        if block.reset {
            // Refused parameters never reach the decoder.
            let params = match Params::rar(block.order, block.mem_mb) {
                Ok(p) => p,
                Err(e) => return (Err(classify(&e)), 0),
            };
            let started = match self.model.as_mut() {
                Some(m) => m.start(params.order(), params.mem_size()),
                None => Model::new(params.order(), params.mem_size()).map(|m| {
                    self.model = Some(m);
                }),
            };
            if let Err(e) = started {
                return self.fail(classify(&e), 0);
            }
            self.failed = None;
        } else if let Some(kind) = self.failed {
            return (Err(kind), 0);
        }
        let Some(model) = self.model.as_mut() else {
            return self.fail(ErrKind::Corrupt, 0);
        };
        let limit = block.unpacked_remaining.min(OUTPUT_CAP as u64);
        if limit == 0 {
            return (Ok(Verdict::Full), 0);
        }
        if block.rc_data.len() < 4 {
            return self.fail(ErrKind::Truncated, 0);
        }
        let mut rc = match CarrylessRangeDecoder::new(Trickle::new(block.rc_data, refill)) {
            Ok(rc) => rc,
            Err(e) => return self.fail(classify(&e), 0),
        };
        while (out.len() as u64) < limit {
            let r = model.decode_symbol(&mut rc);
            let padded = rc.zero_bytes_past_eof();
            match r {
                Ok(Some(_)) if padded > RAR_PADDING => {
                    return self.fail(ErrKind::Truncated, rc.position());
                }
                Ok(Some(b)) => out.push(b),
                Ok(None) if padded != 0 => return self.fail(ErrKind::Truncated, rc.position()),
                Ok(None) => return (Ok(Verdict::End), rc.position()),
                Err(e) => return self.fail(classify(&e), rc.position()),
            }
        }
        (Ok(Verdict::Full), rc.position())
    }

    fn fail(&mut self, kind: ErrKind, at: usize) -> Side {
        self.failed = Some(kind);
        (Err(kind), at)
    }
}

/// The RAR mode: a block sequence through two long-lived decoders.
pub fn check_rar(data: &[u8], refill: usize) {
    let mut fast = RarPpmd::new();
    let mut checked = CheckedRar::default();
    let (mut fast_out, mut checked_out) = (Vec::new(), Vec::new());
    for (i, block) in RarBlock::parse_all(data).into_iter().enumerate() {
        if block.fresh {
            fast = RarPpmd::new();
            checked = CheckedRar::default();
        }
        fast_out.clear();
        checked_out.clear();
        let a = fast_block(&mut fast, &block, &mut fast_out);
        let b = checked.block(&block, refill, &mut checked_out);
        assert_eq!(a, b, "block {i}: fast and checked (verdict, input) differ");
        assert!(
            fast_out == checked_out,
            "block {i}: fast {} bytes, checked {} bytes, first difference at {:?}",
            fast_out.len(),
            checked_out.len(),
            fast_out.iter().zip(&checked_out).position(|(x, y)| x != y)
        );
        assert_eq!(fast.has_model(), checked.model.is_some(), "block {i}");
        let footprint = checked.model.as_ref().map_or(0, |m| {
            m.arena_capacity() as u64 + std::mem::size_of::<Model>() as u64
        });
        assert_eq!(fast.memory_footprint(), footprint, "block {i}");
    }
}

/// The 7z fast side: the whole stream in one call.
fn fast_7z(params: Params, case: &Decode7z<'_>, finish: bool, limit: usize) -> (Side, Vec<u8>) {
    let mut dec = match SevenZDecoder::new(params, case.known.map(|n| n as u64)) {
        Ok(d) => d,
        Err(e) => return ((Err(classify(&e)), 0), Vec::new()),
    };
    dec.set_finish_stream(finish);
    let mut out = vec![0u8; limit];
    match dec.decode(case.stream, true, &mut out) {
        Ok(step) => {
            out.truncate(step.produced);
            let verdict = match step.status {
                SevenZStatus::ReachedSize | SevenZStatus::OutputFull => Verdict::Full,
                SevenZStatus::EndMarker => Verdict::End,
                other => panic!("{other:?} on the last input"),
            };
            assert_eq!(dec.total_in(), step.consumed as u64);
            ((Ok(verdict), step.consumed), out)
        }
        Err(e) => {
            out.truncate(e.at.output as usize);
            ((Err(classify(&e)), e.at.input as usize), out)
        }
    }
}

/// The 7z checked side: the stream decoder's framing over a bare model, one
/// symbol at a time.
fn checked_7z(
    params: Params,
    case: &Decode7z<'_>,
    finish: bool,
    limit: usize,
    refill: usize,
) -> (Side, Vec<u8>) {
    let mut model = match Model::new(params.order(), params.mem_size()) {
        Ok(m) => m,
        Err(e) => return ((Err(classify(&e)), 0), Vec::new()),
    };
    if case.stream.len() < 5 {
        return ((Err(ErrKind::Truncated), 0), Vec::new());
    }
    let mut rc = match SevenZipRangeDecoder::new(Trickle::new(case.stream, refill)) {
        Ok(rc) => rc,
        Err(e) => return ((Err(classify(&e)), 0), Vec::new()),
    };
    let mut out = Vec::new();
    let verdict = loop {
        if out.len() == limit {
            let at_size = case.known == Some(out.len());
            if at_size && finish && !rc.is_finished_ok() {
                break Err(ErrKind::Corrupt);
            }
            break Ok(Verdict::Full);
        }
        let r = model.decode_symbol(&mut rc);
        let padded = rc.zero_bytes_past_eof() != 0;
        match r {
            Ok(Some(_)) if padded => break Err(ErrKind::Truncated),
            Ok(Some(b)) => out.push(b),
            Ok(None) if padded => break Err(ErrKind::Truncated),
            Ok(None) if !rc.is_finished_ok() => break Err(ErrKind::Corrupt),
            Ok(None) if finish && case.known.is_some_and(|n| n != out.len()) => {
                break Err(ErrKind::Corrupt);
            }
            Ok(None) => break Ok(Verdict::End),
            Err(_) if padded => break Err(ErrKind::Truncated),
            Err(e) => break Err(classify(&e)),
        }
    };
    ((verdict, rc.position()), out)
}

/// The 7z mode: one stream through the step decoder and a bare model.
pub fn check_7z(data: &[u8], refill: usize, finish: bool) {
    let Some(case) = Decode7z::parse(data) else {
        return;
    };
    let params = match Params::new(case.order, case.mem) {
        Ok(p) => p,
        Err(e) => {
            assert_eq!(classify(&e), ErrKind::InvalidParameters);
            return;
        }
    };
    let limit = case.known.unwrap_or(OUTPUT_CAP).min(MAX_SYMBOLS_7Z);
    let (a, fast_out) = fast_7z(params, &case, finish, limit);
    let (b, checked_out) = checked_7z(params, &case, finish, limit, refill);
    assert_eq!(a, b, "fast and checked (verdict, input) differ");
    assert!(
        fast_out == checked_out,
        "fast {} bytes, checked {} bytes, first difference at {:?}",
        fast_out.len(),
        checked_out.len(),
        fast_out.iter().zip(&checked_out).position(|(x, y)| x != y)
    );
}

/// Runs one F5 input.
pub fn check(data: &[u8]) {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let refill = 1 + usize::from((mode >> 1) & 7);
    if mode & 1 == 0 {
        check_rar(rest, refill);
    } else {
        check_7z(rest, refill, mode & 0x10 != 0);
    }
}

/// The mode byte for a seed.
pub fn mode(sevenz: bool, refill: usize) -> u8 {
    assert!((1..=8).contains(&refill));
    u8::from(sevenz) | (((refill - 1) as u8) << 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{Kind, generate};
    use crate::reference::{encode_7z, encode_carryless};

    #[test]
    fn trickle_hands_out_the_data_then_zeros() {
        for cap in 1..=8 {
            let mut t = Trickle::new(b"abcdefghij", cap);
            let got: Vec<u8> = (0..12).map(|_| t.next_byte()).collect();
            assert_eq!(&got, b"abcdefghij\0\0");
            assert_eq!((t.position(), t.zero_bytes_past_eof()), (10, 2));
        }
    }

    #[test]
    fn valid_and_damaged_streams_agree() {
        let payload = generate(Kind::Text, 9, 1200);
        let rar = encode_carryless(&payload, 6, 1 << 20, true);
        let z7 = encode_7z(&payload, 6, 1 << 16, false);
        let z7_eos = encode_7z(&payload, 6, 1 << 16, true);
        for refill in [1, 3, 8] {
            let mut input = vec![mode(false, refill)];
            input.extend(RarBlock::seed(true, true, 6, 1, None, &rar));
            input.extend(RarBlock::seed(false, false, 6, 1, Some(500), &rar));
            check(&input);
            let mut damaged = input.clone();
            damaged[40] ^= 0x5A;
            check(&damaged);

            for finish in [0, 0x10] {
                for (stream, known) in
                    [(&z7, Some(1200u16)), (&z7_eos, None), (&z7_eos, Some(1200))]
                {
                    let mut input = vec![mode(true, refill) | finish];
                    input.extend(Decode7z::seed(6, 1 << 16, known, stream));
                    check(&input);
                    let mut damaged = input.clone();
                    damaged[30] ^= 0xA5;
                    check(&damaged);
                    let mut cut = input.clone();
                    cut.truncate(input.len() - 9);
                    check(&cut);
                }
            }
        }
    }

    /// Blocks without a model: corrupt on both sides, before the coder reads
    /// its four bytes and even with nothing to decode (fuzz regressions).
    #[test]
    fn a_block_without_a_model_is_corrupt_on_both_sides() {
        for unpacked in [Some(0), Some(5), None] {
            for rc in [&[0xFFu8, 3, 3][..], &[0xFF, 0xFF, 0xFF, 1, 2, 3]] {
                let mut input = vec![mode(false, 1)];
                input.extend(RarBlock::seed(true, false, 6, 1, unpacked, rc));
                check(&input);
            }
        }
    }
}
