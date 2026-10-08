//! The ppmd-turbo side of the driver.
//!
//! Each operation reports itself unavailable until the crate provides it.
//! Wiring one in means calling the crate here and setting its flag, which
//! `ppmd-bench info` reports so the harness plans ppmd-turbo rows only for
//! operations that exist.

use std::io::Write;

use crate::{Failure, Sink, pump};
use ppmd_corpus::rar::{PpmHeader, Stop, Unescaper};
use ppmd_turbo::rar::{MAX_ZERO_BYTES_PAST_EOF, RarDecoder};
use ppmd_turbo::rc::RarRangeDecoder;

/// The largest model order the crate accepts, as a link check.
pub const MAX_ORDER: u32 = ppmd_turbo::PPMD7_MAX_ORDER;
/// `decode-7z` is wired to the crate.
pub const DECODE_7Z: bool = true;
/// `decode-rar` is wired to the crate.
pub const DECODE_RAR: bool = true;
/// `encode-7z` is wired to the crate.
pub const ENCODE_7Z: bool = false;

fn missing(op: &str) -> Failure {
    Failure::NotImplemented(format!("ppmd-turbo has no {op} yet; use --impl ppmd-rust"))
}

fn codec(e: impl std::fmt::Display) -> Failure {
    Failure::Codec(format!("ppmd-turbo: {e}"))
}

/// Decodes through `Ppmd7Decoder` (the `Read` path a 7z reader uses), told
/// the unpacked size when it is known.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    size: Option<u64>,
    sink: &mut Sink,
) -> Result<(), Failure> {
    let mut dec = match size {
        Some(n) => ppmd_turbo::Ppmd7Decoder::with_unpacked_size(stream, order, mem, n),
        None => ppmd_turbo::Ppmd7Decoder::new(stream, order, mem),
    }
    .map_err(codec)?;
    pump(&mut dec, sink, size)?;
    Ok(())
}

/// Decodes one reset PPMd block through the RAR3 escape layer; returns the
/// symbols the layer consumed.
pub fn decode_rar(
    header: &PpmHeader,
    rc: &[u8],
    limit: usize,
    sink: &mut Sink,
) -> Result<u64, Failure> {
    let esc = header.esc.unwrap_or(ppmd_corpus::rar::DEFAULT_ESC);
    let mut dec = RarDecoder::new();
    dec.init_model(header.order, header.mem_mb).map_err(codec)?;
    let mut rc = RarRangeDecoder::new(rc).map_err(codec)?;
    let mut layer = Unescaper::new(esc, limit);
    if limit > 0 {
        loop {
            let Some(symbol) = dec.decode_symbol(&mut rc).map_err(codec)? else {
                return Err(Failure::Codec(format!(
                    "the PPMd stream ended after {} symbols",
                    layer.symbols
                )));
            };
            if rc.zero_bytes_past_eof() > MAX_ZERO_BYTES_PAST_EOF {
                return Err(codec("the PPMd stream is truncated"));
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
