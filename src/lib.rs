//! PPMd variant H (7-Zip's "Ppmd7") compression and decompression.
//!
//! `ppmd-turbo` implements Dmitry Shkarin's PPMd variant H context model, its
//! sub-allocator and secondary escape estimation, and the two range coders
//! that carry it in the wild:
//!
//! - the carry-less range coder RAR 2.9 through 4.x uses for its PPMd blocks, and
//! - the LZMA-style range coder 7-Zip uses for the `PPMD` method in `.7z`.
//!
//! Both framings are covered for decoding and encoding. Output is bit-exact
//! with RARLAB unrar and 7-Zip: a 7z stream encoded here is byte-identical to
//! 7-Zip's for the same parameters, and a RAR stream decodes to exactly what
//! unrar produces.
//!
//! The crate is pre-release and the API is unstable until 1.0. Decoding is
//! implemented for both framings: 7z ([`Ppmd7Decoder`] through
//! [`std::io::Read`], or [`decode_7z`] for a stream in memory) and RAR
//! ([`rar`]: RAR 2.9 through 4.x; RAR5 has no PPMd). Both encoders are still
//! to come.
//!
//! ```
//! use std::io::Read;
//!
//! // The empty 7z PPMd stream: a zero byte and four flush bytes.
//! let stream = [0u8; 5];
//! let mut decoder = ppmd_turbo::Ppmd7Decoder::with_unpacked_size(&stream[..], 6, 1 << 16, 0)?;
//! let mut out = Vec::new();
//! decoder.read_to_end(&mut out)?;
//! assert!(out.is_empty());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! `unsafe` is permitted where it pays for itself, and only with a
//! `// SAFETY:` proof on every block, Miri coverage where Miri can run, and a
//! fuzz target over every decoder and encoder entry point.

pub(crate) mod alloc;
pub mod error;
pub mod model;
pub mod ppmd7;
pub mod rar;
pub mod rc;
pub(crate) mod see;

pub use error::{Error, Result};
pub use ppmd7::{Ppmd7Decoder, decode_7z};

/// The smallest model order variant H accepts (`PPMD7_MIN_ORDER` in 7-Zip).
pub const PPMD7_MIN_ORDER: u32 = 2;

/// The largest model order variant H accepts (`PPMD7_MAX_ORDER` in 7-Zip).
pub const PPMD7_MAX_ORDER: u32 = 64;

/// The smallest sub-allocator size in bytes (`PPMD7_MIN_MEM_SIZE` in 7-Zip).
pub const PPMD7_MIN_MEM_SIZE: u32 = 1 << 11;

/// The largest sub-allocator size in bytes (`PPMD7_MAX_MEM_SIZE` in 7-Zip):
/// `0xFFFF_FFFF - 12 * 3`, so the allocator's three trailing units still fit
/// in 32-bit offsets.
pub const PPMD7_MAX_MEM_SIZE: u32 = 0xFFFF_FFFF - 12 * 3;

/// Symbol value a decoder returns at the end-of-stream marker
/// (`PPMD7_SYM_END` in 7-Zip).
pub const SYM_END: i32 = -1;

/// Symbol value a decoder returns when the stream is corrupt
/// (`PPMD7_SYM_ERROR` in 7-Zip).
pub const SYM_ERROR: i32 = -2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_match_7zip() {
        assert_eq!(PPMD7_MIN_ORDER, 2);
        assert_eq!(PPMD7_MAX_ORDER, 64);
        assert_eq!(PPMD7_MIN_MEM_SIZE, 2048);
        assert_eq!(PPMD7_MAX_MEM_SIZE, 0xFFFF_FFDB);
        assert_eq!((SYM_END, SYM_ERROR), (-1, -2));
    }
}
