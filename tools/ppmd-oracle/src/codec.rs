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

/// ppmd-turbo's 7z encoder has not landed. When it does, this calls it with
/// `end_marker = false`, as 7-Zip writes.
const TURBO_7Z_ENCODER: &str = "ppmd-turbo's 7z encoder has not landed";

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
            Self::Turbo => Err(CodecError::Unavailable(TURBO_7Z_ENCODER)),
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
            Self::Turbo => ppmd_turbo::decode_7z(stream, order, mem, Some(size)).map_err(failed),
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
        use ppmd_turbo::rar::{MAX_ZERO_BYTES_PAST_EOF, RarDecoder};
        use ppmd_turbo::rc::RarRangeDecoder;

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
        let mut dec = RarDecoder::new();
        dec.init_model(header.order, header.mem_mb)
            .map_err(failed)?;
        let mut rc = RarRangeDecoder::new(&packed[header.len..]).map_err(failed)?;
        let mut layer = Unescaper::new(header.esc.unwrap_or(DEFAULT_ESC), limit);
        while layer.out.len() < limit {
            let symbol = dec.decode_symbol(&mut rc).map_err(failed)?.ok_or_else(|| {
                failed(format!("PPMd stream ended after {} symbols", layer.symbols))
            })?;
            if rc.zero_bytes_past_eof() > MAX_ZERO_BYTES_PAST_EOF {
                return Err(failed("the PPMd stream is truncated"));
            }
            match layer.push(symbol) {
                Ok(Some(Stop::Full | Stop::EndOfFile)) => break,
                Ok(None) => {}
                // LZ blocks and RarVM filters need a full RAR unpacker.
                Err(_) => {
                    return Err(CodecError::Unavailable(
                        "a member that leaves PPMd (LZ block or RarVM filter)",
                    ));
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
