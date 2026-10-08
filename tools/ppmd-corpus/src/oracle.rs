//! ppmd-rust as the oracle the corpus is checked against, and the digests the
//! manifest records.
//!
//! ppmd-rust 1.5.0 is a port of 7-Zip's PPMd code: `Ppmd7Encoder` /
//! `Ppmd7Decoder` over 7-Zip's 7z range coder, and `Ppmd7aEncoder` /
//! `Ppmd7aDecoder` over the carry-less range coder that RAR's PPMd blocks use.
//! It is a dependency of the repository tools only, never of the crate.

use std::io::{Read, Write};

use ppmd_rust::{Ppmd7Decoder, Ppmd7Encoder, Ppmd7aDecoder, Ppmd7aEncoder};
use sha2::{Digest, Sha256};

use crate::rar::{self, Stop, Unescaper};

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// CRC-32 (IEEE), as lower-case hex: the digest `ppmd-bench` prints.
pub fn crc32_hex(data: &[u8]) -> String {
    format!("{:08x}", crc32fast::hash(data))
}

/// Which range coder a raw stream uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coder {
    /// 7-Zip's range coder with carry propagation: the 7z `PPMD` method.
    SevenZ,
    /// The carry-less range coder: RAR's PPMd blocks (7-Zip's `Ppmd7a`).
    CarryLess,
}

/// Encodes `payload` with ppmd-rust.
pub fn encode(
    coder: Coder,
    payload: &[u8],
    order: u32,
    mem: u32,
    eos: bool,
) -> Result<Vec<u8>, String> {
    let err = |e: ppmd_rust::Error| format!("ppmd-rust: {e:?}");
    match coder {
        Coder::SevenZ => {
            let mut enc = Ppmd7Encoder::new(Vec::new(), order, mem).map_err(err)?;
            enc.write_all(payload).map_err(|e| e.to_string())?;
            enc.finish(eos).map_err(|e| e.to_string())
        }
        Coder::CarryLess => {
            let mut enc = Ppmd7aEncoder::new(Vec::new(), order, mem).map_err(err)?;
            enc.write_all(payload).map_err(|e| e.to_string())?;
            enc.finish(eos).map_err(|e| e.to_string())
        }
    }
}

/// Decodes a raw stream with ppmd-rust: exactly `len` bytes when given (the
/// 7z shape, size from the container), else to the end marker.
pub fn decode(
    coder: Coder,
    stream: &[u8],
    order: u32,
    mem: u32,
    len: Option<u64>,
) -> Result<Vec<u8>, String> {
    let err = |e: ppmd_rust::Error| format!("ppmd-rust: {e:?}");
    let mut reader: Box<dyn Read + '_> = match coder {
        Coder::SevenZ => Box::new(Ppmd7Decoder::new(stream, order, mem).map_err(err)?),
        Coder::CarryLess => Box::new(Ppmd7aDecoder::new(stream, order, mem).map_err(err)?),
    };
    let mut out = Vec::new();
    match len {
        Some(n) => {
            out.resize(usize::try_from(n).map_err(|e| e.to_string())?, 0);
            reader.read_exact(&mut out).map_err(|e| e.to_string())?;
        }
        None => {
            reader.read_to_end(&mut out).map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}

/// What decoding a RAR member's PPMd stream produced.
#[derive(Debug, Clone)]
pub struct RarDecode {
    /// The block header in front of the range coder's data.
    pub header: rar::PpmHeader,
    /// The raw PPMd symbols the layer consumed, in order.
    pub symbols: Vec<u8>,
    /// The member's bytes after the escape layer.
    pub payload: Vec<u8>,
    /// Why the layer stopped.
    pub stop: Stop,
}

/// Decodes a single-PPMd-block RAR member: the header, then symbols one at a
/// time through ppmd-rust's carry-less decoder into the escape layer, until
/// the member's unpacked size is reached.
pub fn decode_rar_member(packed: &[u8], unpacked_len: u64) -> Result<RarDecode, String> {
    let header = rar::ppm_header(packed).ok_or("the member does not start with a PPMd block")?;
    if !header.reset {
        return Err("the first PPMd block does not reset the model".into());
    }
    let mem = header
        .mem_mb
        .checked_mul(1 << 20)
        .ok_or("memory size overflows")?;
    let rc = &packed[header.len..];
    let mut dec =
        Ppmd7aDecoder::new(rc, header.order, mem).map_err(|e| format!("ppmd-rust: {e:?}"))?;
    let limit = usize::try_from(unpacked_len).map_err(|e| e.to_string())?;
    let mut layer = Unescaper::new(header.esc.unwrap_or(rar::DEFAULT_ESC), limit);
    let mut symbols = Vec::new();
    let mut one = [0u8; 1];
    let stop = if limit == 0 {
        Stop::Full
    } else {
        loop {
            if dec.read(&mut one).map_err(|e| e.to_string())? == 0 {
                return Err(format!(
                    "the PPMd stream ended after {} symbols",
                    symbols.len()
                ));
            }
            symbols.push(one[0]);
            match layer.push(one[0]) {
                Ok(Some(stop)) => break stop,
                Ok(None) => {}
                Err(e) => {
                    return Err(format!(
                        "escape layer: {e:?} after {} symbols",
                        symbols.len()
                    ));
                }
            }
        }
    };
    Ok(RarDecode {
        header,
        symbols,
        payload: layer.out,
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_both_coders() {
        let payload = crate::payload::generate(crate::payload::Kind::Mixed, 20_000);
        for coder in [Coder::SevenZ, Coder::CarryLess] {
            for eos in [false, true] {
                let s = encode(coder, &payload, 6, 1 << 20, eos).unwrap();
                let len = (!eos).then_some(payload.len() as u64);
                assert_eq!(decode(coder, &s, 6, 1 << 20, len).unwrap(), payload);
            }
        }
    }

    #[test]
    fn truncation_is_an_error_with_a_known_length() {
        let payload = crate::payload::generate(crate::payload::Kind::Text, 20_000);
        let s = encode(Coder::SevenZ, &payload, 6, 1 << 20, false).unwrap();
        assert!(
            decode(
                Coder::SevenZ,
                &s[..s.len() / 2],
                6,
                1 << 20,
                Some(payload.len() as u64)
            )
            .is_err()
        );
    }
}
