//! The ppmd-turbo side of the driver, over the crate's step API: the
//! caller-owned slices a 7z reader or a RAR unpacker hands the codecs.
//!
//! Each operation's flag says whether the crate provides it; `ppmd-bench
//! info` reports them so the harness plans ppmd-turbo rows only for
//! operations that exist.

use std::io::Write;

use crate::{CHUNK, Failure, Sink};
use ppmd_corpus::rar::{PpmHeader, Stop, Unescaper};
use ppmd_turbo::{Params, RarPpmd, RarStatus, SevenZDecoder, SevenZEncoder, SevenZStatus, Symbol};

/// The largest model order the crate accepts, as a link check.
pub const MAX_ORDER: u32 = Params::MAX_ORDER;
/// `decode-7z` is wired to the crate.
pub const DECODE_7Z: bool = true;
/// `decode-rar` is wired to the crate.
pub const DECODE_RAR: bool = true;
/// `encode-7z` is wired to the crate.
pub const ENCODE_7Z: bool = true;

fn codec(e: impl std::fmt::Display) -> Failure {
    Failure::Codec(format!("ppmd-turbo: {e}"))
}

fn write(sink: &mut Sink, bytes: &[u8]) -> Result<(), Failure> {
    sink.write_all(bytes)
        .map_err(|e| Failure::Codec(e.to_string()))
}

/// Decodes the whole stream as one last input into a `CHUNK` output
/// buffer, told the unpacked size when it is known.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    size: Option<u64>,
    sink: &mut Sink,
) -> Result<(), Failure> {
    let params = Params::new(order, mem).map_err(codec)?;
    let mut dec = SevenZDecoder::new(params, size).map_err(codec)?;
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0;
    loop {
        let step = dec.decode(&stream[pos..], true, &mut buf).map_err(codec)?;
        pos += step.consumed;
        write(sink, &buf[..step.produced])?;
        match step.status {
            SevenZStatus::OutputFull => {}
            SevenZStatus::ReachedSize | SevenZStatus::EndMarker => return Ok(()),
            other => return Err(codec(format!("stopped with {other:?}"))),
        }
    }
}

/// Decodes one reset PPMd block through the RAR3 escape layer, literals
/// through the batched `decode` up to the escape byte and the code after
/// each escape through `next_symbol`, as a RAR unpacker drives it; returns
/// the symbols the layer consumed.
pub fn decode_rar(
    header: &PpmHeader,
    rc: &[u8],
    limit: usize,
    sink: &mut Sink,
) -> Result<u64, Failure> {
    let esc = header.esc.unwrap_or(ppmd_corpus::rar::DEFAULT_ESC);
    let params = Params::rar(header.order, header.mem_mb).map_err(codec)?;
    let mut dec = RarPpmd::new();
    dec.start_block(Some(params)).map_err(codec)?;
    let mut layer = Unescaper::new(esc, limit);
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0;
    let layer_error = |e, layer: &Unescaper| {
        Failure::Codec(format!(
            "escape layer: {e:?} after {} symbols",
            layer.symbols
        ))
    };
    if limit == 0 {
        write(sink, &layer.out)?;
        return Ok(layer.symbols);
    }
    'blocks: loop {
        let room = (limit - layer.out.len()).clamp(1, CHUNK);
        let step = dec
            .decode(&rc[pos..], true, &mut buf[..room], esc)
            .map_err(codec)?;
        pos += step.consumed;
        for &symbol in &buf[..step.produced] {
            match layer.push(symbol) {
                Ok(Some(Stop::Full | Stop::EndOfFile)) => break 'blocks,
                Ok(None) => {}
                Err(e) => return Err(layer_error(e, &layer)),
            }
        }
        match step.status {
            RarStatus::OutputFull => {}
            RarStatus::Escape => {
                let code = match dec.next_symbol(&rc[pos..], true).map_err(codec)? {
                    (n, Symbol::Byte(code)) => {
                        pos += n;
                        code
                    }
                    (_, other) => return Err(codec(format!("after an escape: {other:?}"))),
                };
                for symbol in [esc, code] {
                    match layer.push(symbol) {
                        Ok(Some(Stop::Full | Stop::EndOfFile)) => break 'blocks,
                        Ok(None) => {}
                        Err(e) => return Err(layer_error(e, &layer)),
                    }
                }
            }
            other => {
                return Err(Failure::Codec(format!(
                    "the PPMd stream stopped ({other:?}) after {} symbols",
                    layer.symbols
                )));
            }
        }
    }
    write(sink, &layer.out)?;
    Ok(layer.symbols)
}

/// Encodes the whole input in one call into `CHUNK` output pieces.
pub fn encode_7z(
    data: &[u8],
    order: u32,
    mem: u32,
    end_marker: bool,
    mut sink: Sink,
) -> Result<Sink, Failure> {
    let params = Params::new(order, mem).map_err(codec)?;
    let mut enc = SevenZEncoder::new(params).map_err(codec)?;
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0;
    while pos < data.len() {
        let step = enc.encode(&data[pos..], &mut buf).map_err(codec)?;
        pos += step.consumed;
        write(&mut sink, &buf[..step.produced])?;
    }
    loop {
        let fin = enc.finish(&mut buf, end_marker).map_err(codec)?;
        write(&mut sink, &buf[..fin.produced])?;
        if fin.done {
            return Ok(sink);
        }
    }
}
