//! F8: the step contract. Every codec decodes (and encodes) the same bytes,
//! with the same verdict and the same error position, whether its input and
//! output arrive in one piece or in arbitrary pieces, and every call keeps
//! the progress invariant: a call that returns `NeedInput` had less than one
//! symbol's input left (the per-symbol margin, plus the initialization bytes
//! at a stream's start), and `OutputFull` means the output slice is full.
//!
//! The streams are valid (encoded from the payload by ppmd-turbo) or raw
//! input bytes. For RAR, an escape is followed by `next_symbol`, as an
//! unpacker reads a command byte, so both entry points are split.
//!
//! Input layout: `[mode, order_sel, mem_sel, mem_low, seed..8, payload..]`.
//! `mode` bits 0-1 pick 7z, carry-less or RAR; bit 2 raw input instead of
//! an encoded payload; bit 3 an end marker; bit 4 a known size (7z and
//! carry-less), bit 5 FinishStream.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo::internals::{carryless_margin, sevenz_margin};
use ppmd_turbo::{
    CarrylessDecoder, CarrylessEncoder, Error, Params, RarPpmd, RarStatus, SevenZDecoder,
    SevenZEncoder, SevenZStatus, Symbol,
};
use ppmd_turbo_fuzz::api::Splits;
use ppmd_turbo_fuzz::params::{mem_from, order_from};

/// The most output collected from one stream.
const CAP: usize = 1 << 18;
/// The arena ceiling: one codec per stream, a handful alive at once.
const MAX_MEM: u32 = 4 << 20;
/// The escape byte RAR decodes with.
const ESC: u8 = 2;

/// A decode reduced to what must not depend on chunking.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    output: Vec<u8>,
    consumed: u64,
    end: End,
}

#[derive(Debug, PartialEq, Eq)]
enum End {
    Size,
    Marker,
    Capped,
    Failed(Error),
}

/// The 7z and carry-less decoders, which share a status.
trait Decode {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> ppmd_turbo::Result<ppmd_turbo::Progress<SevenZStatus>>;
    fn produced_total(&self) -> u64;
}

impl Decode for SevenZDecoder {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> ppmd_turbo::Result<ppmd_turbo::Progress<SevenZStatus>> {
        self.decode(input, last, out)
    }
    fn produced_total(&self) -> u64 {
        self.total_out()
    }
}

impl Decode for CarrylessDecoder {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> ppmd_turbo::Result<ppmd_turbo::Progress<SevenZStatus>> {
        self.decode(input, last, out)
    }
    fn produced_total(&self) -> u64 {
        self.total_out()
    }
}

/// Decodes `stream` through `splits`, or in one piece without them.
/// `head` is the most input a `NeedInput` may leave unconsumed.
fn decode(
    dec: &mut impl Decode,
    stream: &[u8],
    limit: usize,
    head: usize,
    mut splits: Option<&mut Splits>,
) -> Run {
    let mut output = Vec::new();
    let mut buf = vec![0u8; CAP];
    let mut pos = 0usize;
    let mut end = match splits.as_deref_mut() {
        Some(s) => s.piece().min(stream.len()),
        None => stream.len(),
    };
    loop {
        if output.len() == limit {
            return Run {
                output,
                consumed: pos as u64,
                end: End::Capped,
            };
        }
        let last = end == stream.len();
        let room = match splits.as_deref_mut() {
            Some(s) => s.piece(),
            None => CAP,
        }
        .min(limit - output.len());
        let before = dec.produced_total();
        let step = match dec.step(&stream[pos..end], last, &mut buf[..room]) {
            Ok(step) => step,
            Err(e) => {
                let valid = (e.at.output - before) as usize;
                assert!(valid <= room, "error position past the output");
                output.extend_from_slice(&buf[..valid]);
                // Sticky: the same error again.
                assert_eq!(dec.step(&stream[pos..end], last, &mut buf[..room]), Err(e));
                return Run {
                    output,
                    consumed: e.at.input,
                    end: End::Failed(e),
                };
            }
        };
        assert!(step.consumed <= end - pos && step.produced <= room);
        assert_eq!(dec.produced_total() - before, step.produced as u64);
        output.extend_from_slice(&buf[..step.produced]);
        pos += step.consumed;
        match step.status {
            SevenZStatus::NeedInput => {
                assert!(!last, "NeedInput on the last input");
                assert!(end - pos < head, "NeedInput with {} bytes left", end - pos);
                let more = splits.as_deref_mut().map_or(1, |s| s.piece().max(1));
                end = (end + more).min(stream.len());
            }
            SevenZStatus::OutputFull => {
                assert_eq!(step.produced, room, "OutputFull with room left");
            }
            SevenZStatus::ReachedSize => {
                return Run {
                    output,
                    consumed: pos as u64,
                    end: End::Size,
                };
            }
            SevenZStatus::EndMarker => {
                return Run {
                    output,
                    consumed: pos as u64,
                    end: End::Marker,
                };
            }
            _ => unreachable!("unknown status"),
        }
    }
}

/// Decodes one RAR block of `stream` through `splits`, or in one piece. An
/// escape is followed by one `next_symbol`; both bytes go to the output.
fn decode_rar(
    dec: &mut RarPpmd,
    stream: &[u8],
    head: usize,
    mut splits: Option<&mut Splits>,
) -> Run {
    let mut output = Vec::new();
    let mut buf = vec![0u8; CAP];
    let mut pos = 0usize;
    let mut end = match splits.as_deref_mut() {
        Some(s) => s.piece().min(stream.len()),
        None => stream.len(),
    };
    let mut after_escape = false;
    loop {
        if output.len() >= CAP {
            return Run {
                output,
                consumed: pos as u64,
                end: End::Capped,
            };
        }
        let last = end == stream.len();
        let more = |splits: &mut Option<&mut Splits>| {
            splits.as_deref_mut().map_or(1, |s| s.piece().max(1))
        };
        if after_escape {
            match dec.next_symbol(&stream[pos..end], last) {
                Ok((consumed, symbol)) => {
                    assert!(consumed <= end - pos);
                    pos += consumed;
                    match symbol {
                        Symbol::Byte(b) => {
                            output.push(b);
                            after_escape = false;
                        }
                        Symbol::NeedInput => {
                            assert!(!last, "NeedInput on the last input");
                            assert!(end - pos < head, "NeedInput with {} bytes left", end - pos);
                            end = (end + more(&mut splits)).min(stream.len());
                        }
                        Symbol::ModelEnd => {
                            return Run {
                                output,
                                consumed: pos as u64,
                                end: End::Marker,
                            };
                        }
                    }
                }
                Err(e) => {
                    return Run {
                        output,
                        consumed: e.at.input,
                        end: End::Failed(e),
                    };
                }
            }
            continue;
        }
        let room = match splits.as_deref_mut() {
            Some(s) => s.piece(),
            None => CAP,
        }
        .min(CAP - output.len());
        let step = match dec.decode(&stream[pos..end], last, &mut buf[..room], ESC) {
            Ok(step) => step,
            Err(e) => {
                // The error's position counts every symbol decoded since
                // the block started, the bytes of this call included.
                let valid = (e.at.output - output.len() as u64) as usize;
                assert!(valid <= room, "error position past the output");
                output.extend_from_slice(&buf[..valid]);
                assert_eq!(
                    dec.decode(&stream[pos..end], last, &mut buf[..room], ESC),
                    Err(e)
                );
                return Run {
                    output,
                    consumed: e.at.input,
                    end: End::Failed(e),
                };
            }
        };
        assert!(step.consumed <= end - pos && step.produced <= room);
        output.extend_from_slice(&buf[..step.produced]);
        pos += step.consumed;
        match step.status {
            RarStatus::NeedInput => {
                assert!(!last, "NeedInput on the last input");
                assert!(end - pos < head, "NeedInput with {} bytes left", end - pos);
                end = (end + more(&mut splits)).min(stream.len());
            }
            RarStatus::OutputFull => assert_eq!(step.produced, room, "OutputFull with room left"),
            RarStatus::Escape => {
                output.push(ESC);
                after_escape = true;
            }
            RarStatus::ModelEnd => {
                return Run {
                    output,
                    consumed: pos as u64,
                    end: End::Marker,
                };
            }
            _ => unreachable!("unknown status"),
        }
    }
}

/// The step encoders.
trait Encode {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)>;
    fn end(&mut self, out: &mut [u8], marker: bool) -> ppmd_turbo::Result<(usize, bool)>;
}

impl Encode for SevenZEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], marker: bool) -> ppmd_turbo::Result<(usize, bool)> {
        self.finish(out, marker).map(|f| (f.produced, f.done))
    }
}

impl Encode for CarrylessEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> ppmd_turbo::Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], marker: bool) -> ppmd_turbo::Result<(usize, bool)> {
        self.finish(out, marker).map(|f| (f.produced, f.done))
    }
}

/// Encodes `payload` through `splits`, or in one piece.
fn encode(
    enc: &mut impl Encode,
    payload: &[u8],
    marker: bool,
    mut splits: Option<&mut Splits>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut pos = 0;
    let piece = |splits: &mut Option<&mut Splits>, whole: usize| {
        splits
            .as_deref_mut()
            .map_or(whole, |s| s.piece().min(whole))
    };
    while pos < payload.len() {
        let take = piece(&mut splits, payload.len() - pos).max(1);
        let room = piece(&mut splits, buf.len());
        let (consumed, produced) = enc
            .step(&payload[pos..pos + take], &mut buf[..room])
            .expect("valid parameters encode");
        assert!(consumed <= take && produced <= room);
        assert!(
            consumed > 0 || produced > 0 || room == 0,
            "encode made no progress"
        );
        out.extend_from_slice(&buf[..produced]);
        pos += consumed;
    }
    loop {
        let room = piece(&mut splits, buf.len());
        let (produced, done) = enc
            .end(&mut buf[..room], marker)
            .expect("valid parameters finish");
        assert!(produced <= room);
        out.extend_from_slice(&buf[..produced]);
        if done {
            return out;
        }
        assert!(produced > 0 || room == 0, "finish made no progress");
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((head, rest)) = data.split_at_checked(12) else {
        return;
    };
    let mode = head[0];
    let order = order_from(head[1]);
    let mem = mem_from(head[2], u16::from(head[3]) << 4, MAX_MEM);
    let Ok(params) = Params::new(order, mem) else {
        return;
    };
    let seed = u64::from_le_bytes(head[4..12].try_into().expect("eight bytes"));
    let (raw, marker, known, finish) =
        (mode & 4 != 0, mode & 8 != 0, mode & 16 != 0, mode & 32 != 0);
    let o = order as u8;

    match mode & 3 {
        0 | 3 => {
            let stream = if raw {
                rest.to_vec()
            } else {
                let one = encode(
                    &mut SevenZEncoder::new(params).expect("encoder"),
                    rest,
                    marker,
                    None,
                );
                let split = encode(
                    &mut SevenZEncoder::new(params).expect("encoder"),
                    rest,
                    marker,
                    Some(&mut Splits::new(seed)),
                );
                assert_eq!(one, split, "7z encode depends on chunking");
                one
            };
            let size = (known || !marker) && !raw;
            let unpacked = size.then_some(rest.len() as u64);
            let limit = if size { rest.len().min(CAP) } else { CAP };
            let fresh = || {
                let mut d = SevenZDecoder::new(params, unpacked).expect("decoder");
                d.set_finish_stream(finish);
                d
            };
            let head = 5 + sevenz_margin(o);
            let one = decode(&mut fresh(), &stream, limit, head, None);
            let split = decode(
                &mut fresh(),
                &stream,
                limit,
                head,
                Some(&mut Splits::new(seed)),
            );
            assert_eq!(one, split, "7z decode depends on chunking");
            if !raw && size && rest.len() <= CAP {
                assert_eq!(one.output, rest, "7z round trip");
            }
        }
        1 => {
            let stream = if raw {
                rest.to_vec()
            } else {
                let one = encode(
                    &mut CarrylessEncoder::new(params).expect("encoder"),
                    rest,
                    marker,
                    None,
                );
                let split = encode(
                    &mut CarrylessEncoder::new(params).expect("encoder"),
                    rest,
                    marker,
                    Some(&mut Splits::new(seed)),
                );
                assert_eq!(one, split, "carry-less encode depends on chunking");
                one
            };
            let size = (known || !marker) && !raw;
            let unpacked = size.then_some(rest.len() as u64);
            let limit = if size { rest.len().min(CAP) } else { CAP };
            let fresh = || {
                let mut d = CarrylessDecoder::new(params, unpacked).expect("decoder");
                d.set_finish_stream(finish);
                d
            };
            let head = 4 + carryless_margin(o);
            let one = decode(&mut fresh(), &stream, limit, head, None);
            let split = decode(
                &mut fresh(),
                &stream,
                limit,
                head,
                Some(&mut Splits::new(seed)),
            );
            assert_eq!(one, split, "carry-less decode depends on chunking");
            if !raw && size && rest.len() <= CAP {
                assert_eq!(one.output, rest, "carry-less round trip");
            }
        }
        _ => {
            let stream = if raw {
                rest.to_vec()
            } else {
                encode(
                    &mut CarrylessEncoder::new(params).expect("encoder"),
                    rest,
                    true,
                    None,
                )
            };
            let fresh = || {
                let mut d = RarPpmd::new();
                d.start_block(Some(params)).expect("legal parameters start");
                d
            };
            let head = 4 + carryless_margin(o);
            let one = decode_rar(&mut fresh(), &stream, head, None);
            let split = decode_rar(&mut fresh(), &stream, head, Some(&mut Splits::new(seed)));
            assert_eq!(one, split, "RAR decode depends on chunking");
            if !raw && rest.len() < CAP {
                assert_eq!(one.output, rest, "RAR round trip");
                assert_eq!(one.end, End::Marker);
            }
        }
    }
});
