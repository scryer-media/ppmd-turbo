//! The shim over ppmd-turbo's step API.
//!
//! The targets call the crate through these functions, in the shape
//! `tests/common/api.rs` uses: whole-buffer helpers that drive the step
//! codecs through input and output split points. The split points come from
//! a SplitMix64 seeded with a hash of the call's bytes, so every input picks
//! its own chunking and a failing input reproduces it exactly. Every call
//! checks the step contract as it goes: `consumed` and `produced` within
//! their slices, and a call that does neither says why (`NeedInput` short
//! of the per-symbol margin, `OutputFull` with no room, or a stop).

use crate::SplitMix64;
use crate::outcome::{ErrKind, Outcome, Verdict, classify};
use ppmd_turbo::{
    CarrylessEncoder, Params, RarPpmd, RarStatus, SevenZDecoder, SevenZEncoder, SevenZStatus,
};

/// FNV-1a over `parts`: the split seed of one call.
pub fn seed_of(parts: &[&[u8]]) -> u64 {
    parts
        .iter()
        .flat_map(|p| p.iter())
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Split sizes: mostly small (around the per-symbol margins), sometimes
/// zero, sometimes large.
pub struct Splits(SplitMix64);

impl Splits {
    /// Splits from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(SplitMix64::new(seed))
    }

    /// The next piece length.
    pub fn piece(&mut self) -> usize {
        match self.0.below(8) {
            0 => 0,
            1 => 1,
            2..=4 => self.0.below(300) as usize,
            5 => 264 + self.0.below(16) as usize,
            _ => self.0.below(1 << 14) as usize,
        }
    }
}

/// Decodes a raw 7z `PPMD` stream with ppmd-turbo: `known` bytes when
/// given, else to the end marker or `cap`.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    known: Option<usize>,
    cap: usize,
) -> Option<Outcome> {
    let params = match Params::new(order, mem) {
        Ok(p) => p,
        Err(e) => return Some(Outcome::failed(classify(&e))),
    };
    let mut dec = match SevenZDecoder::new(params, known.map(|n| n as u64)) {
        Ok(d) => d,
        Err(e) => return Some(Outcome::failed(classify(&e))),
    };
    let mut splits = Splits::new(seed_of(&[stream, &order.to_le_bytes()]));
    Some(drive_7z(&mut dec, stream, known, cap, &mut splits))
}

/// Drives `dec` over `stream` through the split points.
pub fn drive_7z(
    dec: &mut SevenZDecoder,
    stream: &[u8],
    known: Option<usize>,
    cap: usize,
    splits: &mut Splits,
) -> Outcome {
    let limit = known.map_or(cap, |n| n.min(cap));
    let mut output = Vec::new();
    let mut buf = vec![0u8; 1 << 14];
    let (mut pos, mut end) = (0usize, splits.piece().min(stream.len()));
    loop {
        if output.len() == limit && known.is_none_or(|n| n > cap) {
            return Outcome {
                output,
                verdict: Verdict::Capped,
            };
        }
        let last = end == stream.len();
        let room = splits.piece().min(buf.len()).min(limit - output.len());
        let before = dec.total_out();
        let step = match dec.decode(&stream[pos..end], last, &mut buf[..room]) {
            Ok(step) => step,
            Err(e) => {
                // The error's position counts the valid output.
                let valid = (e.at.output - before) as usize;
                assert!(valid <= room, "error position past the output");
                output.extend_from_slice(&buf[..valid]);
                return Outcome {
                    output,
                    verdict: Verdict::Failed(classify(&e)),
                };
            }
        };
        assert!(step.consumed <= end - pos && step.produced <= room);
        assert_eq!(dec.total_out() - before, step.produced as u64);
        output.extend_from_slice(&buf[..step.produced]);
        pos += step.consumed;
        match step.status {
            SevenZStatus::NeedInput => {
                assert!(!last, "NeedInput on the last input");
                // Five init bytes, then a symbol's margin.
                assert!(
                    end - pos < 5 + ppmd_turbo::MAX_INPUT_PER_SYMBOL,
                    "NeedInput with a whole symbol's input"
                );
                end = (end + splits.piece().max(1)).min(stream.len());
            }
            SevenZStatus::OutputFull => {
                assert!(
                    room == 0 || step.produced == room,
                    "OutputFull with room left"
                );
            }
            SevenZStatus::ReachedSize => {
                return Outcome {
                    output,
                    verdict: Verdict::Complete,
                };
            }
            SevenZStatus::EndMarker => {
                return Outcome {
                    output,
                    verdict: Verdict::Ended,
                };
            }
            _ => unreachable!("unknown status"),
        }
    }
}

/// RAR's PPMd decoder, kept across the blocks of a member or a solid run.
pub struct RarSession {
    inner: RarPpmd,
    splits: Splits,
}

/// The escape byte the session decodes with; escapes are put back as
/// literals, so the output is the raw symbol stream.
const SESSION_ESC: u8 = 2;

impl RarSession {
    /// A decoder with no model yet.
    pub fn new() -> Option<Self> {
        Some(Self {
            inner: RarPpmd::new(),
            splits: Splits::new(0x5241_5233),
        })
    }

    /// Decodes one block: restart the model first when `reset`, then raw
    /// symbols until `unpacked_remaining` or the model's end marker, through
    /// input and output split points. Returns the bytes of `rc_data`
    /// consumed. Empty `rc_data` decodes nothing and starts nothing.
    pub fn decode_block(
        &mut self,
        reset: bool,
        order: u32,
        mem_mb: u32,
        rc_data: &[u8],
        unpacked_remaining: u64,
        out: &mut Vec<u8>,
    ) -> Result<usize, ErrKind> {
        if rc_data.is_empty() {
            return Ok(0);
        }
        self.splits = Splits::new(seed_of(&[rc_data, &[order as u8, mem_mb as u8]]));
        let params = if reset {
            Some(Params::rar(order, mem_mb).map_err(|e| classify(&e))?)
        } else {
            None
        };
        self.inner.start_block(params).map_err(|e| classify(&e))?;
        let margin = self.inner.max_input_per_symbol();
        let mut buf = vec![0u8; 1 << 14];
        let (mut pos, mut produced) = (0usize, 0u64);
        let mut end = self.splits.piece().min(rc_data.len());
        while produced < unpacked_remaining {
            let last = end == rc_data.len();
            let room = self
                .splits
                .piece()
                .min(buf.len())
                .min(usize::try_from(unpacked_remaining - produced).unwrap_or(usize::MAX));
            let step =
                match self
                    .inner
                    .decode(&rc_data[pos..end], last, &mut buf[..room], SESSION_ESC)
                {
                    Ok(step) => step,
                    Err(e) => return Err(classify(&e)),
                };
            assert!(step.consumed <= end - pos && step.produced <= room);
            out.extend_from_slice(&buf[..step.produced]);
            produced += step.produced as u64;
            pos += step.consumed;
            match step.status {
                RarStatus::NeedInput => {
                    assert!(!last, "NeedInput on the last input");
                    assert!(
                        end - pos < margin + 4,
                        "NeedInput with a whole symbol's input"
                    );
                    end = (end + self.splits.piece().max(1)).min(rc_data.len());
                }
                RarStatus::OutputFull => assert!(room == 0 || step.produced == room),
                RarStatus::Escape => {
                    out.push(SESSION_ESC);
                    produced += 1;
                }
                RarStatus::ModelEnd => break,
                _ => unreachable!("unknown status"),
            }
        }
        Ok(pos)
    }
}

/// A step encoder, as the 7z and carry-less encoders share it.
trait StepEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)>;
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> ppmd_turbo::Result<(usize, bool)>;
}

impl StepEncoder for SevenZEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> ppmd_turbo::Result<(usize, bool)> {
        self.finish(out, end_marker).map(|f| (f.produced, f.done))
    }
}

impl StepEncoder for CarrylessEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> ppmd_turbo::Result<(usize, bool)> {
        self.finish(out, end_marker).map(|f| (f.produced, f.done))
    }
}

/// Encodes `payload` through input and output split points.
fn encode_with(
    enc: &mut impl StepEncoder,
    payload: &[u8],
    end_marker: bool,
    splits: &mut Splits,
) -> ppmd_turbo::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 14];
    let mut pos = 0;
    while pos < payload.len() {
        let take = splits.piece().max(1).min(payload.len() - pos);
        let room = splits.piece().max(1).min(buf.len());
        let (consumed, produced) = enc.step(&payload[pos..pos + take], &mut buf[..room])?;
        assert!(consumed <= take && produced <= room);
        assert!(consumed > 0 || produced > 0, "encode made no progress");
        out.extend_from_slice(&buf[..produced]);
        pos += consumed;
    }
    loop {
        let room = splits.piece().min(buf.len());
        let (produced, done) = enc.end(&mut buf[..room], end_marker)?;
        assert!(produced <= room);
        out.extend_from_slice(&buf[..produced]);
        if done {
            assert_eq!(enc.end(&mut buf, end_marker)?, (0, true), "finish twice");
            return Ok(out);
        }
        assert!(produced > 0 || room == 0, "finish made no progress");
    }
}

/// Encodes with ppmd-turbo's 7z encoder.
pub fn encode_7z(
    payload: &[u8],
    order: u32,
    mem: u32,
    end_marker: bool,
) -> Option<Result<Vec<u8>, ErrKind>> {
    let run = || -> ppmd_turbo::Result<Vec<u8>> {
        let mut enc = SevenZEncoder::new(Params::new(order, mem)?)?;
        let mut splits = Splits::new(seed_of(&[payload, &order.to_le_bytes()]));
        encode_with(&mut enc, payload, end_marker, &mut splits)
    };
    Some(run().map_err(|e| classify(&e)))
}

/// Encodes with ppmd-turbo's carry-less encoder (`mem` in bytes).
pub fn encode_carryless(
    payload: &[u8],
    order: u32,
    mem: u32,
    end_marker: bool,
) -> Option<Result<Vec<u8>, ErrKind>> {
    let run = || -> ppmd_turbo::Result<Vec<u8>> {
        let mut enc = CarrylessEncoder::new(Params::new(order, mem)?)?;
        let mut splits = Splits::new(seed_of(&[payload, &order.to_le_bytes()]));
        encode_with(&mut enc, payload, end_marker, &mut splits)
    };
    Some(run().map_err(|e| classify(&e)))
}
