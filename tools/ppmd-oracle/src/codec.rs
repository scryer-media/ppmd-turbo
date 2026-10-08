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

/// ppmd-turbo's 7z API has not landed. When it does, this calls the 7z
/// encoder with `end_marker = false`, as 7-Zip writes.
const TURBO_7Z: &str = "ppmd-turbo's 7z API has not landed";

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
            Self::Turbo => Err(CodecError::Unavailable(TURBO_7Z)),
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
            Self::Turbo => Err(CodecError::Unavailable(TURBO_7Z)),
        }
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
