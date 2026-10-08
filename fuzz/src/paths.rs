//! F5 `checked_vs_unchecked`: the fast decode path against the checked one.
//!
//! The same model decodes the same stream twice, once through the fast path
//! and once through the slowest path the public API offers, and the two must
//! agree on every byte, every verdict and every coder position:
//!
//! - **fast**: RAR's block API ([`RarDecoder::decode_block`]) over a
//!   borrowed slice, or for the 7z coder [`Model::decode_symbol`] over a
//!   [`SevenZipRangeDecoder`] on a borrowed slice;
//! - **checked**: the model driven one [`RarDecoder::decode_symbol`] or
//!   [`Model::decode_symbol`] call at a time through a coder reading a
//!   [`ReadInput`] whose refill buffer is 1 to 8 bytes over a reader that
//!   hands out one byte per call, so every byte crosses a refill edge.
//!
//! Every arena pointer the model follows is validated today, so the two
//! sides run the same model code and differ in the input layer, the block
//! loop and the coder's monomorphisation. When an unchecked model path
//! lands (`docs/backlog.md` D4), the checked side is where the validated
//! model goes: any disagreement is then a broken D4 invariant.
//!
//! Input: one mode byte (bit 0: 7z, else RAR; bits 1..4: the refill size
//! minus one), then a [`RarBlock`] sequence or a [`Decode7z`] case.

use std::io::{self, Read};

use ppmd_turbo::model::Model;
use ppmd_turbo::rar::{MAX_ZERO_BYTES_PAST_EOF, RarDecoder};
use ppmd_turbo::rc::{
    CarrylessRangeDecoder, RangeDecoder, ReadInput, SevenZipRangeDecoder, SliceInput,
};

use crate::layout::{Decode7z, RarBlock};
use crate::outcome::{ErrKind, classify};
use crate::params::OUTPUT_CAP;

/// The most symbols one 7z case decodes on each side. The decode targets
/// cover long runs; this one runs every input twice, once byte by byte.
pub const MAX_SYMBOLS_7Z: usize = 1 << 16;

/// A reader that returns at most one byte per `read`.
#[derive(Debug)]
pub struct Trickle<'a>(&'a [u8]);

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match (self.0.split_first(), buf.first_mut()) {
            (Some((&b, rest)), Some(slot)) => {
                *slot = b;
                self.0 = rest;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}

fn trickle_input(data: &[u8], refill: usize) -> ReadInput<Trickle<'_>> {
    ReadInput::with_refill_size(Trickle(data), refill)
}

/// One block on the checked side: [`RarDecoder::decode_block`]'s contract,
/// spelled out one symbol at a time over a trickling input.
fn checked_block(
    dec: &mut RarDecoder,
    block: &RarBlock<'_>,
    refill: usize,
    out: &mut Vec<u8>,
) -> Result<usize, ErrKind> {
    if block.rc_data.is_empty() {
        return Ok(0);
    }
    if block.reset {
        dec.init_model(block.order, block.mem_mb)
            .map_err(|e| classify(&e))?;
    }
    let mut rc = CarrylessRangeDecoder::new(trickle_input(block.rc_data, refill))
        .map_err(|e| classify(&e))?;
    let mut produced = 0u64;
    while produced < block.unpacked_remaining {
        let Some(byte) = dec.decode_symbol(&mut rc).map_err(|e| classify(&e))? else {
            break;
        };
        if rc.zero_bytes_past_eof() > MAX_ZERO_BYTES_PAST_EOF {
            return Err(ErrKind::Truncated);
        }
        out.push(byte);
        produced += 1;
    }
    Ok(rc.position())
}

/// The RAR mode: a block sequence through two long-lived decoders.
pub fn check_rar(data: &[u8], refill: usize) {
    let mut fast = RarDecoder::new();
    let mut checked = RarDecoder::new();
    let (mut fast_out, mut checked_out) = (Vec::new(), Vec::new());
    for (i, block) in RarBlock::parse_all(data).into_iter().enumerate() {
        if block.fresh {
            fast = RarDecoder::new();
            checked = RarDecoder::new();
        }
        fast_out.clear();
        checked_out.clear();
        let a = fast
            .decode_block(
                block.reset,
                block.order,
                block.mem_mb,
                block.rc_data,
                block.unpacked_remaining,
                &mut fast_out,
            )
            .map_err(|e| classify(&e));
        let b = checked_block(&mut checked, &block, refill, &mut checked_out);
        assert_eq!(a, b, "block {i}: fast and checked verdicts differ");
        assert!(
            fast_out == checked_out,
            "block {i}: fast {} bytes, checked {} bytes, first difference at {:?}",
            fast_out.len(),
            checked_out.len(),
            fast_out.iter().zip(&checked_out).position(|(x, y)| x != y)
        );
        assert_eq!(fast.has_model(), checked.has_model(), "block {i}");
        assert_eq!(fast.mem_size(), checked.mem_size(), "block {i}");
    }
}

/// What one symbol decode returned, comparable across the two sides.
type Step = Result<Option<u8>, ErrKind>;

fn run_7z<R: RangeDecoder>(model: &mut Model, rc: &mut R, limit: usize, steps: &mut Vec<Step>) {
    for _ in 0..limit {
        let step = model.decode_symbol(rc).map_err(|e| classify(&e));
        steps.push(step);
        if !matches!(step, Ok(Some(_))) {
            break;
        }
    }
}

/// The 7z mode: one stream through two models over the 7z coder.
pub fn check_7z(data: &[u8], refill: usize) {
    let Some(case) = Decode7z::parse(data) else {
        return;
    };
    let fast_model = Model::new(case.order, case.mem).map_err(|e| classify(&e));
    let checked_model = Model::new(case.order, case.mem).map_err(|e| classify(&e));
    let (mut fast_model, mut checked_model) = match (fast_model, checked_model) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(a), Err(b)) => {
            assert_eq!(a, b);
            assert_eq!(a, ErrKind::InvalidParameters);
            return;
        }
        (a, b) => panic!(
            "Model::new disagrees with itself: {:?} vs {:?}",
            a.err(),
            b.err()
        ),
    };
    let limit = case.known.unwrap_or(OUTPUT_CAP).min(MAX_SYMBOLS_7Z);

    let fast_rc = SevenZipRangeDecoder::<SliceInput<'_>>::new(case.stream);
    let checked_rc = SevenZipRangeDecoder::new(trickle_input(case.stream, refill));
    let (mut fast_rc, mut checked_rc) = match (fast_rc, checked_rc) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            assert_eq!(
                a.err().map(|e| classify(&e)),
                b.err().map(|e| classify(&e)),
                "coder initialisation differs"
            );
            return;
        }
    };
    let (mut fast_steps, mut checked_steps) = (Vec::new(), Vec::new());
    run_7z(&mut fast_model, &mut fast_rc, limit, &mut fast_steps);
    run_7z(
        &mut checked_model,
        &mut checked_rc,
        limit,
        &mut checked_steps,
    );
    if fast_steps != checked_steps {
        let at = fast_steps
            .iter()
            .zip(&checked_steps)
            .position(|(a, b)| a != b);
        panic!(
            "fast {} steps, checked {} steps, first difference at {at:?}",
            fast_steps.len(),
            checked_steps.len()
        );
    }
    assert_eq!(fast_rc.position(), checked_rc.position());
    assert_eq!(
        fast_rc.zero_bytes_past_eof(),
        checked_rc.zero_bytes_past_eof()
    );
    assert_eq!(fast_rc.is_finished_ok(), checked_rc.is_finished_ok());
    assert_eq!(fast_rc.faulted(), checked_rc.faulted());
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
        check_7z(rest, refill);
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
    fn trickle_reads_one_byte_at_a_time() {
        let mut t = Trickle(b"abc");
        let mut buf = [0u8; 8];
        assert_eq!(t.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'a');
        assert_eq!(t.read(&mut buf[..0]).unwrap(), 0);
        assert_eq!(t.read(&mut buf).unwrap(), 1);
        assert_eq!(t.read(&mut buf).unwrap(), 1);
        assert_eq!(t.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn valid_and_damaged_streams_agree() {
        let payload = generate(Kind::Text, 9, 1200);
        let rar = encode_carryless(&payload, 6, 1 << 20, true);
        let z7 = encode_7z(&payload, 6, 1 << 16, false);
        for refill in [1, 3, 8] {
            let mut input = vec![mode(false, refill)];
            input.extend(RarBlock::seed(true, true, 6, 1, None, &rar));
            input.extend(RarBlock::seed(false, false, 6, 1, Some(500), &rar));
            check(&input);
            let mut damaged = input.clone();
            damaged[40] ^= 0x5A;
            check(&damaged);

            let mut input = vec![mode(true, refill)];
            input.extend(Decode7z::seed(6, 1 << 16, Some(1200), &z7));
            check(&input);
            let mut damaged = input.clone();
            damaged[30] ^= 0xA5;
            check(&damaged);
        }
    }
}
