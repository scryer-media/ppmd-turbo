//! The decoder API the conformance suites call, in one place: the 7z
//! framing through `ppmd_turbo::Ppmd7Decoder` read with `std::io::Read`,
//! and the RAR framing as `ppmd_turbo::rar::RarDecoder::decode_block`. Order
//! and arena sizes are `u32`, as the crate takes them.

use std::io::Read;

use ppmd_turbo::{Error, Result};

#[allow(unused_imports)]
pub use ppmd_turbo::rar::RarDecoder;

/// Decodes a raw 7z `PPMD` stream (no container) with `order` and
/// `mem_size` from the coder properties: exactly `unpacked_len` bytes when
/// it is given, as 7z does with the folder's unpacked size, otherwise up to
/// the end marker.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem_size: u32,
    unpacked_len: Option<u64>,
) -> Result<Vec<u8>> {
    let mut decoder = ppmd_turbo::Ppmd7Decoder::new(stream, order, mem_size)?;
    let mut out = Vec::new();
    match unpacked_len {
        Some(n) => {
            out.resize(usize::try_from(n).map_err(|_| Error::InvalidParameters)?, 0);
            decoder.read_exact(&mut out)?;
        }
        None => {
            decoder.read_to_end(&mut out)?;
        }
    }
    Ok(out)
}
