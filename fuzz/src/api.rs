//! The shim over ppmd-turbo's decoder and encoder API.
//!
//! The targets are written against the API the crate is growing, the same
//! shape `tests/hostile_support` and the conformance suites use:
//!
//! - `ppmd_turbo::ppmd7::Ppmd7Decoder::new(reader, order, mem_size)`, read
//!   through `std::io::Read`;
//! - `ppmd_turbo::rar::RarDecoder::new()` and
//!   `decode_block(reset, order, mem_mb, rc_data, unpacked_remaining, out)`
//!   returning the bytes of `rc_data` consumed;
//! - `ppmd_turbo::ppmd7::Ppmd7Encoder::new(writer, order, mem_size)` with
//!   `std::io::Write` and `finish(with_end_marker)`;
//! - a carry-less encoder of the same shape whose output a `RarDecoder`
//!   block decodes.
//!
//! Until an entry point exists its function here returns `None` and the
//! targets skip the ppmd-turbo half of the iteration. To switch one on,
//! replace its body with the call in its doc comment; `docs/testing.md` keeps
//! the list.

use crate::outcome::{ErrKind, Outcome};

/// Decodes a raw 7z `PPMD` stream with ppmd-turbo: `known` bytes when
/// given, else to the end marker or `cap`.
///
/// Intended body:
///
/// ```ignore
/// use crate::outcome::{classify, drain};
/// Some(match ppmd_turbo::ppmd7::Ppmd7Decoder::new(stream, order, mem) {
///     Ok(dec) => drain(dec, known, cap),
///     Err(e) => Outcome::failed(classify(&e)),
/// })
/// ```
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    known: Option<usize>,
    cap: usize,
) -> Option<Outcome> {
    let _ = (stream, order, mem, known, cap);
    None
}

/// RAR's PPMd decoder, kept across the blocks of a member or a solid run.
pub struct RarSession {
    // Intended: `inner: ppmd_turbo::rar::RarDecoder`.
    _private: (),
}

impl RarSession {
    /// A decoder with no model yet, or `None` until the RAR decoder lands.
    ///
    /// Intended body:
    /// `Some(Self { inner: ppmd_turbo::rar::RarDecoder::new() })`.
    pub fn new() -> Option<Self> {
        None
    }

    /// Decodes one block; returns the bytes of `rc_data` consumed.
    ///
    /// Intended body:
    ///
    /// ```ignore
    /// self.inner
    ///     .decode_block(reset, order, mem_mb, rc_data, unpacked_remaining, out)
    ///     .map_err(|e| crate::outcome::classify(&e))
    /// ```
    pub fn decode_block(
        &mut self,
        reset: bool,
        order: u32,
        mem_mb: u32,
        rc_data: &[u8],
        unpacked_remaining: u64,
        out: &mut Vec<u8>,
    ) -> Result<usize, ErrKind> {
        let _ = (reset, order, mem_mb, rc_data, unpacked_remaining, out);
        Err(ErrKind::Other)
    }
}

/// Encodes with ppmd-turbo's 7z encoder.
///
/// Intended body:
///
/// ```ignore
/// use std::io::Write;
/// let run = || -> ppmd_turbo::Result<Vec<u8>> {
///     let mut enc = ppmd_turbo::ppmd7::Ppmd7Encoder::new(Vec::new(), order, mem)?;
///     enc.write_all(payload)?;
///     Ok(enc.finish(end_marker)?)
/// };
/// Some(run().map_err(|e| crate::outcome::classify(&e)))
/// ```
pub fn encode_7z(
    payload: &[u8],
    order: u32,
    mem: u32,
    end_marker: bool,
) -> Option<Result<Vec<u8>, ErrKind>> {
    let _ = (payload, order, mem, end_marker);
    None
}

/// Encodes with ppmd-turbo's carry-less encoder (`mem` in bytes).
///
/// Intended body: as [`encode_7z`], with the carry-less encoder type.
pub fn encode_carryless(
    payload: &[u8],
    order: u32,
    mem: u32,
    end_marker: bool,
) -> Option<Result<Vec<u8>, ErrKind>> {
    let _ = (payload, order, mem, end_marker);
    None
}
