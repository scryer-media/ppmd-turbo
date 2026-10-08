//! The ppmd-turbo side of the driver.
//!
//! Each operation reports itself unavailable until the crate provides it.
//! Wiring one in means calling the crate here and setting its flag, which
//! `ppmd-bench info` reports so the harness plans ppmd-turbo rows only for
//! operations that exist.

use std::io::Write;

use crate::{Failure, Sink};
use ppmd_corpus::rar::{self, PpmHeader, Stop, Unescaper};
use ppmd_turbo::rar::{MAX_ZERO_BYTES_PAST_EOF, RarDecoder};
use ppmd_turbo::rc::RarRangeDecoder;

/// The largest model order the crate accepts, as a link check.
pub const MAX_ORDER: u32 = ppmd_turbo::PPMD7_MAX_ORDER;
/// `decode-7z` is wired to the crate.
pub const DECODE_7Z: bool = false;
/// `decode-rar` is wired to the crate.
pub const DECODE_RAR: bool = true;
/// `encode-7z` is wired to the crate.
pub const ENCODE_7Z: bool = false;

fn missing(op: &str) -> Failure {
    Failure::NotImplemented(format!("ppmd-turbo has no {op} yet; use --impl ppmd-rust"))
}

pub fn decode_7z(
    _stream: &[u8],
    _order: u32,
    _mem: u32,
    _size: Option<u64>,
    _sink: &mut Sink,
) -> Result<(), Failure> {
    Err(missing("7z decoder"))
}

/// Decodes one member's PPMd block through `rar::RarDecoder` and the RAR3
/// escape layer, symbol by symbol, as an unpacker drives it. Returns the
/// number of model symbols decoded.
pub fn decode_rar(
    header: &PpmHeader,
    rc: &[u8],
    limit: usize,
    sink: &mut Sink,
) -> Result<u64, Failure> {
    let mut decoder = RarDecoder::new();
    decoder
        .init_model(header.order, header.mem_mb)
        .map_err(|e| format!("ppmd-turbo: {e}"))?;
    let mut coder = RarRangeDecoder::new(rc).map_err(|e| format!("ppmd-turbo: {e}"))?;
    let mut layer = Unescaper::new(header.esc.unwrap_or(rar::DEFAULT_ESC), limit);
    if limit > 0 {
        loop {
            let symbol = decoder
                .decode_symbol(&mut coder)
                .map_err(|e| format!("ppmd-turbo: {e} after {} symbols", layer.symbols))?;
            let Some(symbol) = symbol else {
                return Err(Failure::Codec(format!(
                    "the PPMd stream ended after {} symbols",
                    layer.symbols
                )));
            };
            if coder.zero_bytes_past_eof() > MAX_ZERO_BYTES_PAST_EOF {
                return Err(Failure::Codec(format!(
                    "the PPMd stream ran dry after {} symbols",
                    layer.symbols
                )));
            }
            match layer.push(symbol) {
                Ok(Some(Stop::Full | Stop::EndOfFile)) => break,
                Ok(None) => {}
                Err(e) => {
                    return Err(Failure::Codec(format!(
                        "escape layer: {e:?} after {} symbols",
                        layer.symbols
                    )));
                }
            }
        }
    }
    sink.write_all(&layer.out).map_err(|e| e.to_string())?;
    Ok(layer.symbols)
}

pub fn encode_7z(
    _data: &[u8],
    _order: u32,
    _mem: u32,
    _end_marker: bool,
    _sink: Sink,
) -> Result<Sink, Failure> {
    Err(missing("7z encoder"))
}
