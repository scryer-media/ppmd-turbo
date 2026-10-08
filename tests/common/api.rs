//! The crate's API as the test suites call it, in one place: whole-buffer
//! helpers over the step codecs, fed in uneven pieces so every suite also
//! exercises the step contract (exact `consumed`, `NeedInput` below the
//! per-symbol margin, `OutputFull`).
//!
//! Included by path from `tests/hostile_support` and `tests/encode_support`
//! too, so there is one shim.

#![allow(dead_code)]

use ppmd_turbo::{
    CarrylessDecoder, CarrylessEncoder, Error, ErrorKind, Params, RarPpmd, RarStatus, Result,
    SevenZDecoder, SevenZEncoder, SevenZStatus, Symbol,
};

/// The most output an unsized decode collects. Garbage can decode to many
/// symbols per input byte; past this the decode stops and returns what it
/// has, which no expected payload reaches.
pub const OUTPUT_CAP: usize = 4 << 20;

/// Input pieces the helpers feed, cycled: below, at and above the largest
/// per-symbol margin.
const FEED: [usize; 5] = [4093, 7, 300, 65_536, 264];

/// Output pieces the helpers offer, cycled.
const ROOM: [usize; 4] = [65_536, 1, 4093, 300];

fn truncated() -> Error {
    Error::new(ErrorKind::Truncated)
}

/// A step decoder over a whole stream, as the 7z pair and the raw
/// carry-less pair share it.
trait StepDecoder {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> Result<(usize, usize, SevenZStatus)>;
    fn total_in(&self) -> u64;
}

impl StepDecoder for SevenZDecoder {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> Result<(usize, usize, SevenZStatus)> {
        let p = self.decode(input, last, out)?;
        Ok((p.consumed, p.produced, p.status))
    }
    fn total_in(&self) -> u64 {
        SevenZDecoder::total_in(self)
    }
}

impl StepDecoder for CarrylessDecoder {
    fn step(
        &mut self,
        input: &[u8],
        last: bool,
        out: &mut [u8],
    ) -> Result<(usize, usize, SevenZStatus)> {
        let p = self.decode(input, last, out)?;
        Ok((p.consumed, p.produced, p.status))
    }
    fn total_in(&self) -> u64 {
        CarrylessDecoder::total_in(self)
    }
}

/// What a whole-stream decode ended with.
#[derive(Debug)]
pub struct Decoded {
    /// The output.
    pub data: Vec<u8>,
    /// Input bytes the coder consumed.
    pub consumed: u64,
    /// How it stopped: `ReachedSize`, `EndMarker`, or `OutputFull` at the cap.
    pub status: SevenZStatus,
}

fn run(
    dec: &mut impl StepDecoder,
    stream: &[u8],
    unpacked: Option<u64>,
    cap: usize,
) -> Result<Decoded> {
    let limit = match unpacked {
        Some(n) => usize::try_from(n).map_err(|_| Error::new(ErrorKind::InvalidParameters))?,
        None => cap,
    };
    let mut data = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut pos, mut end, mut turn) = (0usize, 0usize, 0usize);
    end = (end + FEED[0]).min(stream.len());
    loop {
        let last = end == stream.len();
        let room = ROOM[turn % ROOM.len()].min(limit - data.len());
        turn += 1;
        if room == 0 && unpacked.is_none() {
            return Ok(Decoded {
                data,
                consumed: dec.total_in(),
                status: SevenZStatus::OutputFull,
            });
        }
        let (consumed, produced, status) = dec.step(&stream[pos..end], last, &mut buf[..room])?;
        assert!(
            consumed <= end - pos && produced <= room,
            "step overran its slices"
        );
        data.extend_from_slice(&buf[..produced]);
        pos += consumed;
        match status {
            SevenZStatus::NeedInput => {
                assert!(!last, "NeedInput on the last input");
                end = (end + FEED[turn % FEED.len()]).min(stream.len());
            }
            SevenZStatus::OutputFull => {
                assert!(consumed > 0 || produced > 0 || room == 0, "no progress");
            }
            SevenZStatus::ReachedSize | SevenZStatus::EndMarker => {
                if unpacked.is_some_and(|n| data.len() as u64 != n) {
                    // The stream ended before its size, as `read_exact`
                    // would report it.
                    return Err(truncated());
                }
                return Ok(Decoded {
                    data,
                    consumed: dec.total_in(),
                    status,
                });
            }
            _ => unreachable!("unknown status"),
        }
    }
}

/// Decodes a raw 7z `PPMD` stream: exactly `unpacked_len` bytes when it is
/// given (7z's folder size), otherwise to the end marker or [`OUTPUT_CAP`].
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
) -> Result<Vec<u8>> {
    Ok(decode_7z_full(stream, order, mem_size, unpacked_len, false)?.data)
}

/// [`decode_7z`] with FinishStream as asked, reporting where it stopped.
pub fn decode_7z_full(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
    finish_stream: bool,
) -> Result<Decoded> {
    let mut dec = SevenZDecoder::new(Params::new(order, mem_size)?, unpacked_len)?;
    dec.set_finish_stream(finish_stream);
    run(&mut dec, stream, unpacked_len, OUTPUT_CAP)
}

/// Decodes a raw carry-less stream (7-Zip's `Ppmd7a`, Shkarin's `.pmd`):
/// exactly `unpacked_len` symbols when given, otherwise to the end marker.
pub fn decode_carryless(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
) -> Result<Vec<u8>> {
    let mut dec = CarrylessDecoder::new(Params::new(order, mem_size)?, unpacked_len)?;
    Ok(run(&mut dec, stream, unpacked_len, OUTPUT_CAP)?.data)
}

/// A step encoder, as the 7z pair and the raw carry-less pair share it.
trait StepEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> Result<(usize, usize)>;
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> Result<(usize, bool)>;
}

impl StepEncoder for SevenZEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> Result<(usize, bool)> {
        self.finish(out, end_marker).map(|f| (f.produced, f.done))
    }
}

impl StepEncoder for CarrylessEncoder {
    fn step(&mut self, input: &[u8], out: &mut [u8]) -> Result<(usize, usize)> {
        self.encode(input, out).map(|p| (p.consumed, p.produced))
    }
    fn end(&mut self, out: &mut [u8], end_marker: bool) -> Result<(usize, bool)> {
        self.finish(out, end_marker).map(|f| (f.produced, f.done))
    }
}

/// Encodes `data` as a raw 7z `PPMD` stream, through uneven input and
/// output pieces.
pub fn encode_7z(data: &[u8], order: u32, mem_size: u32, end_marker: bool) -> Result<Vec<u8>> {
    let mut enc = SevenZEncoder::new(Params::new(order, mem_size)?)?;
    encode_with(&mut enc, data, end_marker)
}

/// Encodes `data` as a raw carry-less stream.
pub fn encode_carryless(
    data: &[u8],
    order: u32,
    mem_size: u32,
    end_marker: bool,
) -> Result<Vec<u8>> {
    let mut enc = CarrylessEncoder::new(Params::new(order, mem_size)?)?;
    encode_with(&mut enc, data, end_marker)
}

fn encode_with(enc: &mut impl StepEncoder, data: &[u8], end_marker: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let (mut pos, mut turn) = (0usize, 0usize);
    while pos < data.len() {
        let take = FEED[turn % FEED.len()].min(data.len() - pos);
        let room = ROOM[turn % ROOM.len()];
        turn += 1;
        let (consumed, produced) = enc.step(&data[pos..pos + take], &mut buf[..room])?;
        assert!(consumed > 0 || produced > 0, "encode made no progress");
        out.extend_from_slice(&buf[..produced]);
        pos += consumed;
    }
    loop {
        let room = ROOM[turn % ROOM.len()];
        turn += 1;
        let (produced, done) = enc.end(&mut buf[..room], end_marker)?;
        out.extend_from_slice(&buf[..produced]);
        if done {
            // Idempotent: a finished encoder writes nothing more.
            assert_eq!(enc.end(&mut buf, end_marker)?, (0, true));
            return Ok(out);
        }
    }
}

/// The old block-at-a-time RAR entry point over [`RarPpmd`]: the RAR
/// conformance and hostile suites are written against it.
pub struct RarDecoder {
    /// The decoder under test.
    pub ppmd: RarPpmd,
}

impl Default for RarDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The escape byte the shim decodes with. Every escape is put back as a
/// literal, so the output is the raw symbol stream whatever the byte.
const SHIM_ESC: u8 = 2;

impl RarDecoder {
    /// A decoder with no model.
    pub fn new() -> Self {
        Self {
            ppmd: RarPpmd::new(),
        }
    }

    /// Starts the model as a reset block header would.
    pub fn init_model(&mut self, order: u32, mem_mb: u32) -> Result<()> {
        self.ppmd.start_block(Some(Params::rar(order, mem_mb)?))
    }

    /// Whether there is a model.
    pub fn has_model(&self) -> bool {
        self.ppmd.has_model()
    }

    /// RAR's `CleanUp`.
    pub fn cleanup(&mut self) {
        self.ppmd.cleanup().expect("the cleanup model fits");
    }

    /// Forgets the model.
    pub fn reset(&mut self) {
        self.ppmd.forget();
    }

    /// Decodes one byte-aligned PPMd block: restart the model first when
    /// `reset`, then raw symbols until `unpacked_remaining` or the model's
    /// end marker. Returns the coder bytes consumed. Empty `rc_data`
    /// decodes nothing and starts nothing.
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
        let params = if reset {
            Some(Params::rar(order, mem_mb)?)
        } else {
            None
        };
        self.ppmd.start_block(params)?;
        let mut buf = vec![0u8; 1 << 16];
        let (mut pos, mut turn, mut produced) = (0usize, 0usize, 0u64);
        let mut end = FEED[0].min(rc_data.len());
        while produced < unpacked_remaining {
            let last = end == rc_data.len();
            let room = ROOM[turn % ROOM.len()]
                .min(usize::try_from(unpacked_remaining - produced).unwrap_or(usize::MAX));
            turn += 1;
            let step = self
                .ppmd
                .decode(&rc_data[pos..end], last, &mut buf[..room], SHIM_ESC)?;
            out.extend_from_slice(&buf[..step.produced]);
            produced += step.produced as u64;
            pos += step.consumed;
            match step.status {
                RarStatus::NeedInput => {
                    assert!(!last, "NeedInput on the last input");
                    end = (end + FEED[turn % FEED.len()]).min(rc_data.len());
                }
                RarStatus::OutputFull => {}
                RarStatus::Escape => {
                    out.push(SHIM_ESC);
                    produced += 1;
                }
                RarStatus::ModelEnd => break,
                _ => unreachable!("unknown status"),
            }
        }
        Ok(pos)
    }

    /// Every raw symbol of a block through [`RarPpmd::next_symbol`], to the
    /// end marker or the end of the input.
    pub fn decode_symbols(&mut self, reset: Option<Params>, rc_data: &[u8]) -> Result<Vec<u8>> {
        self.ppmd.start_block(reset)?;
        let mut out = Vec::new();
        let mut pos = 0;
        loop {
            let (consumed, symbol) = self.ppmd.next_symbol(&rc_data[pos..], true)?;
            pos += consumed;
            match symbol {
                Symbol::Byte(b) => out.push(b),
                Symbol::ModelEnd => return Ok(out),
                Symbol::NeedInput => unreachable!("the input is the last"),
            }
        }
    }
}

/// Puts RAR's carry-less range decoder in the given registers, asks it to
/// scale its range by `total`, and returns whether it reported the fault
/// (`RangeDecoder::faulted`) instead of dividing by zero. No stream is
/// needed: the registers are crafted so that `range / total == 0`.
#[cfg(feature = "internals")]
pub fn carryless_threshold_faults(low: u32, code: u32, range: u32, total: u32) -> bool {
    use ppmd_turbo::internals::{RangeCoderState, RangeDecoder, RarRangeDecoder};
    let mut rc = RarRangeDecoder::from_state(&[][..], RangeCoderState::new(low, code, range));
    let _ = rc.get_threshold(total);
    rc.faulted()
}
