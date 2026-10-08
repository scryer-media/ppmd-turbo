//! The 7z framing: the `PPMD` method (`03 04 01`) as `.7z` carries it.
//!
//! A 7z PPMd stream is variant H over Igor Pavlov's range coder: one
//! leading zero byte, four bytes of coder initialization, then the coded
//! symbols. The coder properties are the model order and the arena size,
//! stored as five bytes ([`Params::from_7z_props`]). 7-Zip writes no end
//! marker: the folder's unpacked size says where the data ends.
//!
//! [`SevenZDecoder`] and [`SevenZEncoder`] are step codecs over slices the
//! caller owns. They never hold caller bytes, so `consumed` is exact, which
//! is what 7-Zip's FinishStream check (consumed equals the packed size) and
//! a container's next-stream offset need. The stop rules follow 7-Zip's
//! `PpmdDecoder.cpp`: a symbol that needs a byte past the last input is
//! [`ErrorKind::Truncated`](crate::ErrorKind::Truncated) and is not output;
//! an end marker where the coder's `code` is not 0 is corrupt; with
//! [`set_finish_stream`](SevenZDecoder::set_finish_stream), `code` must be 0
//! at the known size and an end marker before it is corrupt.
//!
//! Output is identical to 7-Zip's in both directions: the encoder writes
//! the bytes 7-Zip writes for the same parameters (with
//! [`Params::reduced_for`] applied the way 7-Zip applies `ReduceSize`).
//! [`crate::io`] wraps both in `Read` and `Write`.

pub use crate::stream::{Finish, SevenZStatus};

use crate::arena::Arena;
use crate::error::{Progress, Result};
use crate::params::Params;
use crate::stream::{SevenZ, StreamDecoder, StreamEncoder};

/// A 7z `PPMD` stream decoder.
///
/// ```
/// use ppmd_turbo::{Params, SevenZDecoder, SevenZStatus};
///
/// // The empty stream: a zero byte and four flush bytes.
/// let params = Params::new(6, 1 << 16)?;
/// let mut decoder = SevenZDecoder::new(params, Some(0))?;
/// let step = decoder.decode(&[0; 5], true, &mut [])?;
/// assert_eq!(step.consumed, 5);
/// assert_eq!(step.status, SevenZStatus::ReachedSize);
/// # Ok::<(), ppmd_turbo::Error>(())
/// ```
pub struct SevenZDecoder {
    inner: StreamDecoder<SevenZ>,
}

impl SevenZDecoder {
    /// A decoder for one stream, allocating its model memory
    /// ([`Params::memory_footprint`] bytes). `unpacked` is the folder's
    /// unpacked size, or `None` to decode to the end marker.
    ///
    /// Errors: [`ErrorKind::AllocationFailed`](crate::ErrorKind::AllocationFailed).
    pub fn new(params: Params, unpacked: Option<u64>) -> Result<Self> {
        Self::with_arena(params, unpacked, Arena::empty())
    }

    /// [`new`](Self::new) in `arena` when its capacity fits (at least the
    /// arena the parameters lay out and at most twice it), otherwise in a
    /// fresh allocation.
    pub fn with_arena(params: Params, unpacked: Option<u64>, arena: Arena) -> Result<Self> {
        Ok(Self {
            inner: StreamDecoder::with_arena(params, unpacked, arena)?,
        })
    }

    /// Starts the next stream (the next folder): restarts the model, keeps
    /// the arena when it fits the new parameters, and expects the five
    /// initialization bytes again. Clears an earlier error.
    ///
    /// Errors: [`ErrorKind::AllocationFailed`](crate::ErrorKind::AllocationFailed)
    /// when the arena has to grow and cannot; the decoder then repeats it.
    pub fn reset(&mut self, params: Params, unpacked: Option<u64>) -> Result<()> {
        self.inner.reset(params, unpacked)
    }

    /// 7-Zip's `FinishStream` mode, off by default: at the known size the
    /// coder must have finished (`code == 0`), and an end marker before the
    /// size is corrupt. Checking that the whole packed stream was consumed
    /// is the container's: compare [`total_in`](Self::total_in) with the
    /// packed size.
    pub fn set_finish_stream(&mut self, on: bool) {
        self.inner.set_finish_stream(on);
    }

    /// Decodes from `input` into `out`.
    ///
    /// The decoder takes no byte it cannot use yet: unless `input_is_last`,
    /// it decodes only while at least [`Params::max_input_per_symbol`] bytes
    /// remain and then returns [`SevenZStatus::NeedInput`]; present the
    /// bytes after `consumed` again on the next call, followed by more.
    /// With `input_is_last`, the input ends here and the last symbols are
    /// decoded against it.
    ///
    /// Errors are sticky: every later call repeats the error until
    /// [`reset`](Self::reset). The error's position says how far the stream
    /// got: the bytes written before it (`at.output - total_out()` before
    /// the call) are valid.
    pub fn decode(
        &mut self,
        input: &[u8],
        input_is_last: bool,
        out: &mut [u8],
    ) -> Result<Progress<SevenZStatus>> {
        self.inner.decode(input, input_is_last, out)
    }

    /// Input bytes consumed since construction or the last reset.
    pub fn total_in(&self) -> u64 {
        self.inner.total_in()
    }

    /// Output bytes produced since construction or the last reset.
    pub fn total_out(&self) -> u64 {
        self.inner.total_out()
    }

    /// Heap bytes the decoder holds: its arena allocation and the model.
    pub fn memory_footprint(&self) -> u64 {
        self.inner.memory_footprint()
    }

    /// The arena, for the next codec.
    pub fn into_arena(self) -> Arena {
        self.inner.into_arena()
    }

    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn arena_addr(&self) -> usize {
        self.inner.arena_addr()
    }
}

/// A 7z `PPMD` stream encoder.
///
/// Every byte placed in the output is final; the encoder holds back at most
/// the coder's carry state (one cached byte and a count) and, when a symbol
/// spills past the output slice, a short queue that the next call drains
/// first ([`pending_output`](Self::pending_output)).
///
/// ```
/// use ppmd_turbo::{Params, SevenZDecoder, SevenZEncoder};
///
/// let data = b"abracadabra, abracadabra";
/// let params = Params::new(6, 1 << 20)?.reduced_for(data.len() as u64);
/// let mut encoder = SevenZEncoder::new(params)?;
/// let mut stream = vec![0u8; 64];
/// let step = encoder.encode(data, &mut stream)?;
/// assert_eq!(step.consumed, data.len());
/// let mut len = step.produced;
/// loop {
///     let fin = encoder.finish(&mut stream[len..], false)?;
///     len += fin.produced;
///     if fin.done {
///         break;
///     }
/// }
///
/// let mut decoder = SevenZDecoder::new(params, Some(data.len() as u64))?;
/// let mut out = [0u8; 24];
/// decoder.decode(&stream[..len], true, &mut out)?;
/// assert_eq!(&out, data);
/// # Ok::<(), ppmd_turbo::Error>(())
/// ```
pub struct SevenZEncoder {
    inner: StreamEncoder<SevenZ>,
}

impl SevenZEncoder {
    /// An encoder for one stream, allocating its model memory.
    ///
    /// Errors: [`ErrorKind::AllocationFailed`](crate::ErrorKind::AllocationFailed).
    pub fn new(params: Params) -> Result<Self> {
        Self::with_arena(params, Arena::empty())
    }

    /// [`new`](Self::new) in `arena` when its capacity fits.
    pub fn with_arena(params: Params, arena: Arena) -> Result<Self> {
        Ok(Self {
            inner: StreamEncoder::with_arena(params, arena)?,
        })
    }

    /// Starts the next stream, keeping the arena when it fits. Bytes not yet
    /// drained from the previous stream are dropped.
    pub fn reset(&mut self, params: Params) -> Result<()> {
        self.inner.reset(params)
    }

    /// Encodes from `input` into `out`: first whatever an earlier call left
    /// queued, then one symbol per input byte while `out` has room. An
    /// empty `out` takes nothing.
    ///
    /// Errors: corrupt if the model went inconsistent, which a correct
    /// encoder never does; invalid parameters after [`finish`](Self::finish)
    /// has started. Errors are sticky until [`reset`](Self::reset).
    pub fn encode(&mut self, input: &[u8], out: &mut [u8]) -> Result<Progress<()>> {
        self.inner.encode(input, out)
    }

    /// Ends the stream: the end marker when `end_marker` (7-Zip writes none
    /// in a `.7z`; pass `false` to match it), then the coder's five flush
    /// bytes. Call until [`Finish::done`]; the tail is written exactly once
    /// however many calls that takes, and calls after `done` write nothing.
    /// `end_marker` is read on the first call only.
    pub fn finish(&mut self, out: &mut [u8], end_marker: bool) -> Result<Finish> {
        self.inner.finish(out, end_marker)
    }

    /// Output bytes the encoder owes before any further symbol: the queue
    /// plus the coder's held-back carry bytes and flush. Adding a stream's
    /// produced bytes to this bounds its final length (without an end
    /// marker).
    pub fn pending_output(&self) -> u64 {
        self.inner.pending_output()
    }

    /// Input bytes encoded since construction or the last reset.
    pub fn total_in(&self) -> u64 {
        self.inner.total_in()
    }

    /// Output bytes produced since construction or the last reset.
    pub fn total_out(&self) -> u64 {
        self.inner.total_out()
    }

    /// Heap bytes the encoder holds: its arena allocation and the model.
    pub fn memory_footprint(&self) -> u64 {
        self.inner.memory_footprint()
    }

    /// The arena, for the next codec.
    pub fn into_arena(self) -> Arena {
        self.inner.into_arena()
    }

    #[cfg(any(test, feature = "internals"))]
    #[doc(hidden)]
    pub fn arena_addr(&self) -> usize {
        self.inner.arena_addr()
    }
}
