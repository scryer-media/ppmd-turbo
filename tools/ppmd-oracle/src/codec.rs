//! The codecs under test: ppmd-rust 1.5.0 (a port of 7-Zip's C, used to
//! validate the oracle itself) and ppmd-turbo.

use std::fmt;
use std::io::{Read, Write};

/// Which implementation encodes and decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// ppmd-rust 1.5.0.
    Reference,
    /// ppmd-turbo, this repository.
    Turbo,
}

impl Codec {
    /// Parses `reference` or `turbo`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reference" => Some(Self::Reference),
            "turbo" => Some(Self::Turbo),
            _ => None,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Reference => "reference",
            Self::Turbo => "turbo",
        })
    }
}

/// Why a codec call produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The entry point does not exist yet.
    Unavailable(&'static str),
    /// The codec returned an error.
    Failed(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(what) => write!(f, "unavailable: {what}"),
            Self::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

fn failed(e: impl fmt::Display) -> CodecError {
    CodecError::Failed(e.to_string())
}

impl Codec {
    /// Encodes `data` with the 7z coder and no end marker, as 7-Zip does.
    pub fn encode_7z(self, data: &[u8], order: u32, mem: u32) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::Reference => {
                let mut enc =
                    ppmd_rust::Ppmd7Encoder::new(Vec::new(), order, mem).map_err(failed)?;
                enc.write_all(data).map_err(failed)?;
                enc.finish(false).map_err(failed)
            }
            Self::Turbo => {
                let params = ppmd_turbo::Params::new(order, mem).map_err(failed)?;
                let mut writer =
                    ppmd_turbo::io::SevenZWriter::new(Vec::new(), params).map_err(failed)?;
                writer.write_all(data).map_err(failed)?;
                writer.finish().map_err(failed)?;
                Ok(writer.into_inner())
            }
        }
    }

    /// Decodes exactly `size` bytes of a 7z-coder stream.
    pub fn decode_7z(
        self,
        stream: &[u8],
        order: u32,
        mem: u32,
        size: u64,
    ) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::Reference => {
                let size = usize::try_from(size).map_err(failed)?;
                let mut dec = ppmd_rust::Ppmd7Decoder::new(stream, order, mem).map_err(failed)?;
                let mut out = vec![0u8; size];
                let mut filled = 0;
                while filled < size {
                    match dec.read(&mut out[filled..]).map_err(failed)? {
                        0 => return Err(failed(format!("ended after {filled} of {size} bytes"))),
                        n => filled += n,
                    }
                }
                Ok(out)
            }
            Self::Turbo => {
                let size = usize::try_from(size).map_err(failed)?;
                let params = ppmd_turbo::Params::new(order, mem).map_err(failed)?;
                let mut dec =
                    ppmd_turbo::SevenZDecoder::new(params, Some(size as u64)).map_err(failed)?;
                dec.set_finish_stream(true);
                let mut out = vec![0u8; size];
                let step = dec.decode(stream, true, &mut out).map_err(failed)?;
                if step.produced != size || step.consumed != stream.len() {
                    return Err(failed(format!(
                        "{} of {size} bytes from {} of {} packed bytes ({:?})",
                        step.produced,
                        step.consumed,
                        stream.len(),
                        step.status
                    )));
                }
                Ok(out)
            }
        }
    }

    /// Decodes one RAR 2.9-4.x member whose packed data starts with a
    /// PPMd block that resets the model, through the RAR3 escape layer, to
    /// exactly `unpacked_len` bytes. Only ppmd-turbo is wired; the
    /// reference has no RAR framing of its own.
    pub fn decode_rar_member(
        self,
        packed: &[u8],
        unpacked_len: u64,
    ) -> Result<Vec<u8>, CodecError> {
        use ppmd_corpus::rar::{DEFAULT_ESC, Stop, Unescaper, ppm_header};
        use ppmd_turbo::{Params, RarPpmd, RarStatus, Symbol};

        if self == Self::Reference {
            return Err(CodecError::Unavailable(
                "check-rar compares ppmd-turbo with unrar",
            ));
        }
        let header = ppm_header(packed).ok_or(CodecError::Unavailable(
            "a member that does not start with a PPMd block",
        ))?;
        if !header.reset {
            return Err(failed("the member's first PPMd block does not reset"));
        }
        let limit = usize::try_from(unpacked_len).map_err(failed)?;
        let rc = &packed[header.len..];
        let esc = header.esc.unwrap_or(DEFAULT_ESC);
        let mut dec = RarPpmd::new();
        dec.start_block(Some(
            Params::rar(header.order, header.mem_mb).map_err(failed)?,
        ))
        .map_err(failed)?;
        let mut layer = Unescaper::new(esc, limit);
        let mut buf = vec![0u8; 1 << 16];
        let mut pos = 0;
        // LZ blocks and RarVM filters need a full RAR unpacker.
        let unavailable =
            |_| CodecError::Unavailable("a member that leaves PPMd (LZ block or RarVM filter)");
        'decode: while layer.out.len() < limit {
            let room = (limit - layer.out.len()).min(buf.len());
            let step = dec
                .decode(&rc[pos..], true, &mut buf[..room], esc)
                .map_err(failed)?;
            pos += step.consumed;
            let mut symbols: Vec<u8> = buf[..step.produced].to_vec();
            match step.status {
                RarStatus::OutputFull => {}
                RarStatus::Escape => match dec.next_symbol(&rc[pos..], true).map_err(failed)? {
                    (n, Symbol::Byte(code)) => {
                        pos += n;
                        symbols.extend([esc, code]);
                    }
                    (_, other) => return Err(failed(format!("after an escape: {other:?}"))),
                },
                other => {
                    return Err(failed(format!(
                        "PPMd stream stopped ({other:?}) after {} symbols",
                        layer.symbols + symbols.len() as u64
                    )));
                }
            }
            for symbol in symbols {
                if let Some(Stop::Full | Stop::EndOfFile) =
                    layer.push(symbol).map_err(unavailable)?
                {
                    break 'decode;
                }
            }
        }
        if layer.out.len() != limit {
            return Err(failed(format!("{} of {limit} bytes", layer.out.len())));
        }
        Ok(layer.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_round_trips() {
        let data = crate::corpus::text(3, 5000);
        let stream = Codec::Reference
            .encode_7z(&data, 6, 1 << 16)
            .expect("encodes");
        let back = Codec::Reference
            .decode_7z(&stream, 6, 1 << 16, data.len() as u64)
            .expect("decodes");
        assert_eq!(back, data);
    }
}
