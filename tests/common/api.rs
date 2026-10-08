//! The decoder API the conformance suites call, in one place.
//!
//! The suites are written against the API the crate is growing: the 7z
//! framing as `ppmd_turbo::ppmd7::Ppmd7Decoder::new(reader, order, mem_size)`
//! read through `std::io::Read`, and the RAR framing as
//! `ppmd_turbo::rar::RarDecoder::decode_block(..)` in the shape unrar-rs's
//! `PpmdDecoder::decode_block` has. Until those exist every function here is
//! a `todo!()` and every test that reaches one is
//! `#[ignore = "awaiting decoder"]`. When the decoder lands, replace each body
//! with the call in its comment and drop the `ignore` attributes; nothing
//! else in the suites changes.

use ppmd_turbo::Result;

/// Decodes a raw 7z `PPMD` stream (no container) with `order` and
/// `mem_size` from the coder properties: exactly `unpacked_len` bytes when
/// it is given, as 7z does with the folder's unpacked size, otherwise up to
/// the end marker.
///
/// Intended body:
///
/// ```ignore
/// use std::io::Read;
/// let mut decoder = ppmd_turbo::ppmd7::Ppmd7Decoder::new(stream, order, mem_size)?;
/// let mut out = Vec::new();
/// match unpacked_len {
///     Some(n) => {
///         out.resize(usize::try_from(n).map_err(|_| ppmd_turbo::Error::InvalidParameters)?, 0);
///         decoder.read_exact(&mut out)?;
///     }
///     None => {
///         decoder.read_to_end(&mut out)?;
///     }
/// }
/// Ok(out)
/// ```
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
) -> Result<Vec<u8>> {
    let _ = (stream, order, mem_size, unpacked_len);
    todo!("awaiting ppmd_turbo::ppmd7::Ppmd7Decoder")
}

/// RAR's PPMd decoder state, kept across the blocks of one member (or of a
/// solid run of members). Intended: `ppmd_turbo::rar::RarDecoder`.
pub struct RarDecoder {
    _private: (),
}

impl RarDecoder {
    /// A decoder with no model yet; the first block must reset.
    ///
    /// Intended body: `Self(ppmd_turbo::rar::RarDecoder::new())`.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Decodes one PPMd block from the carry-less range coder's data
    /// (`rc_data`, the bytes after the RAR block header), in unrar-rs's shape:
    ///
    /// - `reset`: rebuild the model (the header's 0x20 flag);
    /// - `max_order`: the model order after RAR's mapping, used when `reset`;
    /// - `alloc_mb`: the sub-allocator size in MiB (`MaxMB + 1`), used when
    ///   `reset`;
    /// - `unpacked_remaining`: the most symbols to decode; decoding also
    ///   stops at the end marker;
    /// - `output`: receives the raw PPMd symbols (the RAR3 escape layer is the
    ///   caller's).
    ///
    /// Returns how many bytes of `rc_data` the range coder consumed. A block
    /// that does not reset before any model exists is an error, and so is
    /// running out of `rc_data` before `unpacked_remaining` symbols.
    ///
    /// Intended body:
    /// `self.0.decode_block(reset, max_order, alloc_mb, rc_data, unpacked_remaining, output)`.
    pub fn decode_block(
        &mut self,
        reset: bool,
        max_order: usize,
        alloc_mb: usize,
        rc_data: &[u8],
        unpacked_remaining: u64,
        output: &mut Vec<u8>,
    ) -> Result<usize> {
        let _ = (
            reset,
            max_order,
            alloc_mb,
            rc_data,
            unpacked_remaining,
            output,
        );
        todo!("awaiting ppmd_turbo::rar::RarDecoder")
    }
}
