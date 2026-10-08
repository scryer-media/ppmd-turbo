//! PPMd variant H (7-Zip's "Ppmd7") compression and decompression.
//!
//! `ppmd-turbo` implements Dmitry Shkarin's PPMd variant H context model, its
//! sub-allocator and secondary escape estimation, and the two range coders
//! that carry it in the wild:
//!
//! - the LZMA-style range coder 7-Zip uses for the `PPMD` method in `.7z`
//!   ([`SevenZDecoder`], [`SevenZEncoder`]), and
//! - the carry-less range coder RAR 2.9 through 4.x uses for its PPMd blocks
//!   ([`RarPpmd`]; RAR5 has no PPMd).
//!
//! Output is bit-exact with 7-Zip and RARLAB unrar: a 7z stream encoded here
//! is byte-identical to 7-Zip's for the same parameters, and a RAR stream
//! decodes to exactly what unrar produces. Writing RAR blocks or archives is
//! out of scope by design; [`CarrylessEncoder`] and [`CarrylessDecoder`]
//! handle raw carry-less streams for round-trip testing only.
//!
//! # The step API
//!
//! Every codec is a step machine over slices the caller owns: a call takes
//! input and an output slice and reports a [`Progress`] with exact
//! `consumed` and `produced` counts and a status. The codecs never keep
//! caller bytes between calls; a non-final call decodes only while a whole
//! symbol's worth of input ([`Params::max_input_per_symbol`], at most
//! [`MAX_INPUT_PER_SYMBOL`]) remains. Model memory is an [`Arena`] the
//! caller can reuse across streams. Errors are typed ([`ErrorKind`]),
//! `Copy`, carry the stream [`Position`], and are sticky until a reset. The
//! [`io`] module wraps the 7z pair in `Read` and `Write`.
//!
//! ```
//! use ppmd_turbo::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};
//!
//! let data = b"the quick brown fox jumps over the lazy dog";
//! let params = Params::new(6, 1 << 20)?.reduced_for(data.len() as u64);
//!
//! let mut encoder = SevenZEncoder::new(params)?;
//! let mut stream = vec![0u8; 128];
//! let mut len = encoder.encode(data, &mut stream)?.produced;
//! while !{
//!     let fin = encoder.finish(&mut stream[len..], false)?;
//!     len += fin.produced;
//!     fin.done
//! } {}
//!
//! let mut decoder = SevenZDecoder::new(params, Some(data.len() as u64))?;
//! let mut out = vec![0u8; data.len()];
//! let step = decoder.decode(&stream[..len], true, &mut out)?;
//! assert_eq!(step.status, SevenZStatus::ReachedSize);
//! assert_eq!(step.consumed, len);
//! assert_eq!(&out[..], data);
//! # Ok::<(), ppmd_turbo::Error>(())
//! ```
//!
//! # Features
//!
//! - `std` (default): the [`io`] adapters, `std::error::Error`, the
//!   `io::Error` conversion and run-time x86 SIMD detection. Without it the
//!   crate builds on `core` and `alloc`, choosing SIMD tiers at compile
//!   time; this is checked, not promised.
//! - `internals`: the engine, for the crate's own fuzz targets, benches and
//!   differential tests. Hidden and unstable.
//!
//! `unsafe` is permitted where it pays for itself, and only with a
//! `// SAFETY:` proof on every block, Miri coverage where Miri can run, and a
//! fuzz target over every decoder and encoder entry point.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc as alloc_crate;
#[cfg(not(feature = "std"))]
extern crate core as std;

#[cfg_attr(not(feature = "internals"), allow(dead_code))]
pub(crate) mod alloc;
mod arena;
pub mod carryless;
mod engine;
mod error;
#[cfg(feature = "std")]
pub mod io;
#[cfg_attr(not(feature = "internals"), allow(dead_code))]
pub(crate) mod model;
mod params;
pub mod rar;
#[cfg_attr(not(feature = "internals"), allow(dead_code))]
pub(crate) mod rc;
#[cfg_attr(not(feature = "internals"), allow(dead_code))]
pub(crate) mod see;
pub mod sevenz;
mod stream;

#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod internals;

pub use arena::Arena;
pub use carryless::{CarrylessDecoder, CarrylessEncoder};
pub use error::{Error, ErrorKind, Position, Progress, Result};
pub use params::{MAX_INPUT_PER_SYMBOL, Params};
pub use rar::{RarPpmd, RarStatus, Symbol};
pub use sevenz::{Finish, SevenZDecoder, SevenZEncoder, SevenZStatus};

pub(crate) const PPMD7_MIN_ORDER: u32 = Params::MIN_ORDER;
pub(crate) const PPMD7_MAX_ORDER: u32 = Params::MAX_ORDER;
pub(crate) const PPMD7_MIN_MEM_SIZE: u32 = Params::MIN_MEM;
pub(crate) const PPMD7_MAX_MEM_SIZE: u32 = Params::MAX_MEM;

/// The symbol value of the end marker in the model's encoder
/// (`PPMD7_SYM_END` in 7-Zip).
pub(crate) const SYM_END: i32 = -1;

/// The model's decoder result for a count past the frequency total, which a
/// valid stream never produces (`PPMD7_SYM_ERROR` in 7-Zip).
pub(crate) const SYM_ERROR: i32 = -2;

/// Every codec moves between threads; none is shared (`Sync`), because the
/// sub-allocator's free lists use `Cell`.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Arena>();
    assert_send::<Error>();
    assert_send::<SevenZDecoder>();
    assert_send::<SevenZEncoder>();
    assert_send::<RarPpmd>();
    assert_send::<CarrylessDecoder>();
    assert_send::<CarrylessEncoder>();
    #[cfg(feature = "std")]
    {
        assert_send::<io::SevenZReader<&[u8]>>();
        assert_send::<io::SevenZWriter<alloc_crate::vec::Vec<u8>>>();
    }
};
