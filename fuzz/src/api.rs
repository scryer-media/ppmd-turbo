//! The shim over ppmd-turbo's decoder and encoder API.
//!
//! The targets call the crate through these functions, in the same shape
//! `tests/hostile_support` and the conformance suites use:
//!
//! - `ppmd_turbo::Ppmd7Decoder` (with a known unpacked size or to the end
//!   marker), read through `std::io::Read`;
//! - `ppmd_turbo::rar::RarDecoder::new()` and
//!   `decode_block(reset, order, mem_mb, rc_data, unpacked_remaining, out)`
//!   returning the bytes of `rc_data` consumed;
//! - the 7z and carry-less encoders, which do not exist yet: their
//!   functions return `None` and the targets skip the ppmd-turbo half of the
//!   iteration. To switch one on, replace its body with the call in its doc
//!   comment; `docs/testing.md` keeps the list.

use crate::outcome::{ErrKind, Outcome, classify, drain};

/// Decodes a raw 7z `PPMD` stream with ppmd-turbo: `known` bytes when
/// given, else to the end marker or `cap`.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    known: Option<usize>,
    cap: usize,
) -> Option<Outcome> {
    let decoder = match known {
        Some(n) => ppmd_turbo::Ppmd7Decoder::with_unpacked_size(stream, order, mem, n as u64),
        None => ppmd_turbo::Ppmd7Decoder::new(stream, order, mem),
    };
    Some(match decoder {
        Ok(dec) => drain(dec, known, cap),
        Err(e) => Outcome::failed(classify(&e)),
    })
}

/// RAR's PPMd decoder, kept across the blocks of a member or a solid run.
pub struct RarSession {
    inner: ppmd_turbo::rar::RarDecoder,
}

impl RarSession {
    /// A decoder with no model yet.
    pub fn new() -> Option<Self> {
        Some(Self {
            inner: ppmd_turbo::rar::RarDecoder::new(),
        })
    }

    /// Decodes one block; returns the bytes of `rc_data` consumed.
    pub fn decode_block(
        &mut self,
        reset: bool,
        order: u32,
        mem_mb: u32,
        rc_data: &[u8],
        unpacked_remaining: u64,
        out: &mut Vec<u8>,
    ) -> Result<usize, ErrKind> {
        self.inner
            .decode_block(reset, order, mem_mb, rc_data, unpacked_remaining, out)
            .map_err(|e| classify(&e))
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
