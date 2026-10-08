//! The step decoder and encoder shared by the two whole-stream framings:
//! the 7z `PPMD` stream (`sevenz.rs`) and the raw carry-less stream
//! (`carryless.rs`). They differ only in the coder, so the stop rules live
//! here once, after 7-Zip's `PpmdDecoder.cpp` and `PpmdEncoder.cpp`:
//!
//! - **Reading past the input is truncation.** 7-Zip raises its `Extra`
//!   flag when the coder asks for a byte the input does not have and fails
//!   the stream; the symbol decoded with that byte is not output. A
//!   well-formed stream never needs one.
//! - **The end marker** ends the data when the coder has finished cleanly
//!   there (7z: `code == 0`; carry-less: `code == low`) and is corrupt
//!   otherwise.
//! - **A known unpacked size** stops decoding after that many bytes. With
//!   FinishStream on, the coder must also have finished cleanly at the size,
//!   and an end marker before it is corrupt.
//! - **The encoder** writes the optional end marker, then the coder's flush
//!   bytes, exactly once, however many `finish` calls it takes to drain them.

use alloc_crate::boxed::Box;

use crate::arena::{Arena, try_box};
use crate::engine::run::{self, Cursor, Padding, Stop};
use crate::error::{Error, Position, Progress, Result};
use crate::model::Model;
use crate::params::{Params, carryless_margin, sevenz_margin};
use crate::rc::{
    CarrylessEncoderRegs, CarrylessRangeDecoder, CarrylessRangeEncoder, Drain, Pending,
    RangeCoderState, SevenZipDecoderRegs, SevenZipEncoderRegs, SevenZipRangeDecoder,
    SevenZipRangeEncoder, SliceInput,
};

/// Why a stream decode call returned.
///
/// See [`Progress`] for the progress invariant: a call that took and wrote
/// nothing returns `NeedInput` (the input left is shorter than
/// [`Params::max_input_per_symbol`] and is not the last) or `OutputFull`
/// (the output slice is empty), or the stream has already stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SevenZStatus {
    /// More input is needed: present the unconsumed bytes again, followed
    /// by more, or say the input is the last.
    NeedInput,
    /// The output slice is full.
    OutputFull,
    /// The unpacked size was produced (and, with FinishStream, the coder
    /// finished cleanly there). Every later call returns this again.
    ReachedSize,
    /// The stream's end marker was decoded and the coder finished cleanly
    /// there. Without FinishStream this can come before a known size; the
    /// caller decides what that means. Every later call returns this again.
    EndMarker,
}

/// What [`SevenZEncoder::finish`](crate::SevenZEncoder::finish) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finish {
    /// Bytes written into the output slice.
    pub produced: usize,
    /// Whether the stream is complete: the end marker (if asked for) and
    /// the coder's flush bytes are all written. Later calls write nothing
    /// and return `done` again.
    pub done: bool,
}

/// One coder's part in a step decoder.
pub(crate) trait DecodeCoder {
    type Regs: Copy + Default;
    type Dec<'a>: Cursor;
    /// Initialization bytes at the start of a stream.
    const INIT: usize;
    fn init(bytes: &[u8]) -> Result<Self::Regs>;
    fn margin(order: u8) -> usize;
    fn resume(input: &[u8], regs: Self::Regs) -> Self::Dec<'_>;
    fn regs(dec: &Self::Dec<'_>) -> Self::Regs;
    fn finished_ok(regs: Self::Regs) -> bool;
}

/// The 7z coder.
pub(crate) enum SevenZ {}

impl DecodeCoder for SevenZ {
    type Regs = SevenZipDecoderRegs;
    type Dec<'a> = SevenZipRangeDecoder<SliceInput<'a>>;
    const INIT: usize = 5;

    fn init(bytes: &[u8]) -> Result<Self::Regs> {
        Ok(SevenZipRangeDecoder::new(bytes)?.regs())
    }
    #[inline]
    fn margin(order: u8) -> usize {
        sevenz_margin(order)
    }
    #[inline]
    fn resume(input: &[u8], regs: Self::Regs) -> Self::Dec<'_> {
        SevenZipRangeDecoder::resume(SliceInput::new(input), regs)
    }
    #[inline]
    fn regs(dec: &Self::Dec<'_>) -> Self::Regs {
        dec.regs()
    }
    fn finished_ok(regs: Self::Regs) -> bool {
        regs.code == 0
    }
}

/// The carry-less coder, with 7-Zip's `Ppmd7a` initialization check.
pub(crate) enum Carryless {}

impl DecodeCoder for Carryless {
    type Regs = RangeCoderState;
    type Dec<'a> = CarrylessRangeDecoder<SliceInput<'a>>;
    const INIT: usize = 4;

    fn init(bytes: &[u8]) -> Result<Self::Regs> {
        Ok(CarrylessRangeDecoder::new_7a(bytes)?.state())
    }
    #[inline]
    fn margin(order: u8) -> usize {
        carryless_margin(order)
    }
    #[inline]
    fn resume(input: &[u8], regs: Self::Regs) -> Self::Dec<'_> {
        CarrylessRangeDecoder::from_state(SliceInput::new(input), regs)
    }
    #[inline]
    fn regs(dec: &Self::Dec<'_>) -> Self::Regs {
        dec.state()
    }
    fn finished_ok(regs: Self::Regs) -> bool {
        regs.code == regs.low
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Init,
    Decoding,
    ReachedSize,
    EndMarker,
    Failed(Error),
}

/// A step decoder over coder `C`.
pub(crate) struct StreamDecoder<C: DecodeCoder> {
    model: Box<Model>,
    params: Params,
    regs: C::Regs,
    phase: Phase,
    unpacked: Option<u64>,
    finish_stream: bool,
    total_in: u64,
    total_out: u64,
}

impl<C: DecodeCoder> StreamDecoder<C> {
    pub(crate) fn with_arena(params: Params, unpacked: Option<u64>, arena: Arena) -> Result<Self> {
        let model = Model::with_arena(params.order(), params.mem_size(), arena.buf)?;
        Ok(Self {
            model: try_box(model)?,
            params,
            regs: C::Regs::default(),
            phase: Phase::Init,
            unpacked,
            finish_stream: false,
            total_in: 0,
            total_out: 0,
        })
    }

    pub(crate) fn reset(&mut self, params: Params, unpacked: Option<u64>) -> Result<()> {
        self.total_in = 0;
        self.total_out = 0;
        self.regs = C::Regs::default();
        self.unpacked = unpacked;
        if let Err(e) = self.model.start(params.order(), params.mem_size()) {
            self.phase = Phase::Failed(e);
            return Err(e);
        }
        self.params = params;
        self.phase = Phase::Init;
        Ok(())
    }

    pub(crate) fn set_finish_stream(&mut self, on: bool) {
        self.finish_stream = on;
    }

    pub(crate) fn total_in(&self) -> u64 {
        self.total_in
    }

    pub(crate) fn total_out(&self) -> u64 {
        self.total_out
    }

    pub(crate) fn memory_footprint(&self) -> u64 {
        self.model.arena_capacity() as u64 + core::mem::size_of::<Model>() as u64
    }

    pub(crate) fn into_arena(self) -> Arena {
        let model = *self.model;
        Arena {
            buf: model.into_arena(),
        }
    }

    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn arena_addr(&self) -> usize {
        self.model.arena_addr()
    }

    #[cold]
    fn fail(&mut self, e: Error) -> Result<Progress<SevenZStatus>> {
        let e = e.at(Position {
            input: self.total_in,
            output: self.total_out,
        });
        self.phase = Phase::Failed(e);
        Err(e)
    }

    /// The known size was reached.
    fn at_size(&mut self, consumed: usize, produced: usize) -> Result<Progress<SevenZStatus>> {
        if self.finish_stream && !C::finished_ok(self.regs) {
            return self.fail(Error::corrupt(
                "PPMd stream does not end at its unpacked size",
            ));
        }
        self.phase = Phase::ReachedSize;
        Ok(progress(consumed, produced, SevenZStatus::ReachedSize))
    }

    pub(crate) fn decode(
        &mut self,
        input: &[u8],
        input_is_last: bool,
        out: &mut [u8],
    ) -> Result<Progress<SevenZStatus>> {
        match self.phase {
            Phase::Failed(e) => return Err(e),
            Phase::ReachedSize => return Ok(progress(0, 0, SevenZStatus::ReachedSize)),
            Phase::EndMarker => return Ok(progress(0, 0, SevenZStatus::EndMarker)),
            Phase::Init | Phase::Decoding => {}
        }
        let mut consumed = 0;
        if let Phase::Init = self.phase {
            let Some(init) = input.get(..C::INIT) else {
                if input_is_last {
                    return self.fail(Error::truncated());
                }
                return Ok(progress(0, 0, SevenZStatus::NeedInput));
            };
            match C::init(init) {
                Ok(regs) => self.regs = regs,
                Err(e) => return self.fail(e),
            }
            consumed = C::INIT;
            self.total_in += C::INIT as u64;
            self.phase = Phase::Decoding;
        }

        let want = match self.unpacked {
            Some(size) => {
                let left = size - self.total_out;
                if left == 0 {
                    return self.at_size(consumed, 0);
                }
                out.len().min(usize::try_from(left).unwrap_or(usize::MAX))
            }
            None => out.len(),
        };
        if want == 0 {
            return Ok(progress(consumed, 0, SevenZStatus::OutputFull));
        }
        let out = &mut out[..want];

        let rest = &input[consumed..];
        let margin = C::margin(self.params.order_u8());
        let mut rc = C::resume(rest, self.regs);
        let (mut produced, mut stop) = match rest.len().checked_sub(margin) {
            Some(fast_end) => run::fast::<_, false>(&mut self.model, &mut rc, out, fast_end, 0),
            None => (0, Ok(Stop::Margin)),
        };
        if input_is_last && stop == Ok(Stop::Margin) {
            let rule = Padding {
                before: 0,
                allowance: 0,
                truncates_errors: true,
            };
            let (more, edge_stop) =
                run::edge::<_, false>(&mut self.model, &mut rc, &mut out[produced..], 0, rule);
            produced += more;
            stop = edge_stop;
        }
        let padded = rc.padding() != 0;
        consumed += rc.position();
        self.regs = C::regs(&rc);
        self.total_in += rc.position() as u64;
        self.total_out += produced as u64;

        match stop {
            Err(e) => self.fail(e),
            Ok(Stop::Margin) => Ok(progress(consumed, produced, SevenZStatus::NeedInput)),
            Ok(Stop::OutputFull) => {
                if self.unpacked == Some(self.total_out) {
                    self.at_size(consumed, produced)
                } else {
                    Ok(progress(consumed, produced, SevenZStatus::OutputFull))
                }
            }
            Ok(Stop::EndMarker) => {
                if padded {
                    return self.fail(Error::truncated());
                }
                if !C::finished_ok(self.regs) {
                    return self.fail(Error::corrupt(
                        "PPMd end marker where the range coder has not finished",
                    ));
                }
                if self.finish_stream && self.unpacked.is_some_and(|n| n != self.total_out) {
                    return self.fail(Error::corrupt("PPMd end marker before the unpacked size"));
                }
                self.phase = Phase::EndMarker;
                Ok(progress(consumed, produced, SevenZStatus::EndMarker))
            }
            Ok(Stop::Escape) => self.fail(Error::corrupt("unexpected escape stop")),
        }
    }
}

#[inline]
fn progress<S>(consumed: usize, produced: usize, status: S) -> Progress<S> {
    Progress {
        consumed,
        produced,
        status,
    }
}

/// One coder's part in a step encoder.
pub(crate) trait EncodeCoder {
    type Regs: Copy + Default;
    /// Encodes `input` while the sink has room and nothing is queued.
    /// Returns the bytes taken.
    fn encode(
        model: &mut Model,
        regs: &mut Self::Regs,
        sink: &mut Drain<'_, '_>,
        input: &[u8],
    ) -> (usize, Result<()>);
    fn end_marker(model: &mut Model, regs: &mut Self::Regs, sink: &mut Drain<'_, '_>)
    -> Result<()>;
    fn flush(regs: &mut Self::Regs, sink: &mut Drain<'_, '_>);
    /// Bytes the coder holds back plus what its flush adds.
    fn owed(regs: &Self::Regs) -> u64;
}

impl EncodeCoder for SevenZ {
    type Regs = SevenZipEncoderRegs;

    #[inline]
    fn encode(
        model: &mut Model,
        regs: &mut Self::Regs,
        sink: &mut Drain<'_, '_>,
        input: &[u8],
    ) -> (usize, Result<()>) {
        let mut rc = SevenZipRangeEncoder::resume(sink, *regs);
        let mut taken = 0;
        let mut result = Ok(());
        for &byte in input {
            if rc.output().blocked() {
                break;
            }
            taken += 1;
            if let Err(e) = model.encode_symbol(&mut rc, Some(byte)) {
                result = Err(e);
                break;
            }
        }
        *regs = rc.regs();
        (taken, result)
    }

    fn end_marker(
        model: &mut Model,
        regs: &mut Self::Regs,
        sink: &mut Drain<'_, '_>,
    ) -> Result<()> {
        let mut rc = SevenZipRangeEncoder::resume(sink, *regs);
        let result = model.encode_symbol(&mut rc, None);
        *regs = rc.regs();
        result
    }

    fn flush(regs: &mut Self::Regs, sink: &mut Drain<'_, '_>) {
        let mut rc = SevenZipRangeEncoder::resume(sink, *regs);
        rc.flush();
        *regs = rc.regs();
    }

    fn owed(regs: &Self::Regs) -> u64 {
        regs.held_back() + 4
    }
}

impl EncodeCoder for Carryless {
    type Regs = CarrylessEncoderRegs;

    #[inline]
    fn encode(
        model: &mut Model,
        regs: &mut Self::Regs,
        sink: &mut Drain<'_, '_>,
        input: &[u8],
    ) -> (usize, Result<()>) {
        let mut rc = CarrylessRangeEncoder::resume(sink, *regs);
        let mut taken = 0;
        let mut result = Ok(());
        for &byte in input {
            if rc.output().blocked() {
                break;
            }
            taken += 1;
            if let Err(e) = model.encode_symbol(&mut rc, Some(byte)) {
                result = Err(e);
                break;
            }
        }
        *regs = rc.regs();
        (taken, result)
    }

    fn end_marker(
        model: &mut Model,
        regs: &mut Self::Regs,
        sink: &mut Drain<'_, '_>,
    ) -> Result<()> {
        let mut rc = CarrylessRangeEncoder::resume(sink, *regs);
        let result = model.encode_symbol(&mut rc, None);
        *regs = rc.regs();
        result
    }

    fn flush(regs: &mut Self::Regs, sink: &mut Drain<'_, '_>) {
        let mut rc = CarrylessRangeEncoder::resume(sink, *regs);
        rc.flush();
        *regs = rc.regs();
    }

    fn owed(_regs: &Self::Regs) -> u64 {
        4
    }
}

#[derive(Clone, Copy, Debug)]
enum EncPhase {
    Encoding,
    /// The end marker step is done (or was not asked for).
    Marked,
    /// The flush step is done; the queue may still hold bytes.
    Flushed,
    Done,
    Failed(Error),
}

/// A step encoder over coder `C`.
pub(crate) struct StreamEncoder<C: EncodeCoder> {
    model: Box<Model>,
    regs: C::Regs,
    queue: Pending,
    phase: EncPhase,
    total_in: u64,
    total_out: u64,
}

impl<C: EncodeCoder> StreamEncoder<C> {
    pub(crate) fn with_arena(params: Params, arena: Arena) -> Result<Self> {
        let model = Model::with_arena(params.order(), params.mem_size(), arena.buf)?;
        Ok(Self {
            model: try_box(model)?,
            regs: C::Regs::default(),
            queue: Pending::new(),
            phase: EncPhase::Encoding,
            total_in: 0,
            total_out: 0,
        })
    }

    pub(crate) fn reset(&mut self, params: Params) -> Result<()> {
        self.regs = C::Regs::default();
        self.queue.clear();
        self.total_in = 0;
        self.total_out = 0;
        if let Err(e) = self.model.start(params.order(), params.mem_size()) {
            self.phase = EncPhase::Failed(e);
            return Err(e);
        }
        self.phase = EncPhase::Encoding;
        Ok(())
    }

    pub(crate) fn pending_output(&self) -> u64 {
        let owed = match self.phase {
            EncPhase::Encoding | EncPhase::Marked => C::owed(&self.regs),
            EncPhase::Flushed | EncPhase::Done | EncPhase::Failed(_) => 0,
        };
        self.queue.len() + owed
    }

    pub(crate) fn total_in(&self) -> u64 {
        self.total_in
    }

    pub(crate) fn total_out(&self) -> u64 {
        self.total_out
    }

    pub(crate) fn memory_footprint(&self) -> u64 {
        self.model.arena_capacity() as u64 + core::mem::size_of::<Model>() as u64
    }

    pub(crate) fn into_arena(self) -> Arena {
        let model = *self.model;
        Arena {
            buf: model.into_arena(),
        }
    }

    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn arena_addr(&self) -> usize {
        self.model.arena_addr()
    }

    #[cold]
    fn fail(&mut self, e: Error) -> Error {
        let e = e.at(Position {
            input: self.total_in,
            output: self.total_out,
        });
        self.phase = EncPhase::Failed(e);
        e
    }

    fn check_queue(&mut self) -> Result<()> {
        if self.queue.overflowed() {
            return Err(self.fail(Error::corrupt("encoder emitted past its per-symbol bound")));
        }
        Ok(())
    }

    pub(crate) fn encode(&mut self, input: &[u8], out: &mut [u8]) -> Result<Progress<()>> {
        match self.phase {
            EncPhase::Encoding => {}
            EncPhase::Failed(e) => return Err(e),
            _ => return Err(Error::invalid_parameters()),
        }
        let mut produced = self.queue.drain_into(out);
        let mut consumed = 0;
        if self.queue.is_empty() && !input.is_empty() {
            let mut sink = Drain::new(&mut out[produced..], &mut self.queue);
            let (taken, result) = C::encode(&mut self.model, &mut self.regs, &mut sink, input);
            produced += sink.written();
            consumed = taken;
            self.total_in += taken as u64;
            self.total_out += produced as u64;
            if let Err(e) = result {
                return Err(self.fail(e));
            }
            self.check_queue()?;
        } else {
            self.total_out += produced as u64;
        }
        Ok(Progress {
            consumed,
            produced,
            status: (),
        })
    }

    pub(crate) fn finish(&mut self, out: &mut [u8], end_marker: bool) -> Result<Finish> {
        let mut produced = 0;
        loop {
            if let EncPhase::Failed(e) = self.phase {
                return Err(e);
            }
            let n = self.queue.drain_into(&mut out[produced..]);
            produced += n;
            self.total_out += n as u64;
            if !self.queue.is_empty() {
                return Ok(Finish {
                    produced,
                    done: false,
                });
            }
            let mut sink = Drain::new(&mut out[produced..], &mut self.queue);
            let next = match self.phase {
                EncPhase::Encoding => {
                    if end_marker {
                        let result = C::end_marker(&mut self.model, &mut self.regs, &mut sink);
                        if let Err(e) = result {
                            self.total_out += sink.written() as u64;
                            return Err(self.fail(e));
                        }
                    }
                    EncPhase::Marked
                }
                EncPhase::Marked => {
                    C::flush(&mut self.regs, &mut sink);
                    EncPhase::Flushed
                }
                EncPhase::Flushed | EncPhase::Done => {
                    self.phase = EncPhase::Done;
                    return Ok(Finish {
                        produced,
                        done: true,
                    });
                }
                EncPhase::Failed(e) => return Err(e),
            };
            let written = sink.written();
            produced += written;
            self.total_out += written as u64;
            self.phase = next;
            self.check_queue()?;
        }
    }
}
