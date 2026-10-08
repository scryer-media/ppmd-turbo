//! ppmd-rust 1.5.0, the in-process reference.
//!
//! ppmd-rust is a line-for-line port of 7-Zip's `Ppmd7.c`, `Ppmd7Dec.c`,
//! `Ppmd7Enc.c` and the `7a` carry-less coder. It is only a proxy: `7zz` and
//! `unrar` are the true references, checked out of process by
//! `tools/ppmd-oracle`. Known ppmd-rust behaviours the harness allows for:
//!
//! - its `Read` impl treats the end of the input as the end of the data;
//! - its carry-less decoder divides by zero when the coder's range falls
//!   below the context total (7-Zip's `Ppmd7aDec.c` checks first), so the
//!   harness never hands it hostile carry-less input.

use std::io::{self, Read};

use ppmd_rust::{Ppmd7Decoder, Ppmd7aDecoder};

use crate::outcome::{ErrKind, Outcome, Reference, drain};

/// A slice reader that records whether anyone asked for bytes past its end.
#[derive(Debug)]
pub struct EdgeReader<'a> {
    data: &'a [u8],
    hit_end: bool,
}

impl<'a> EdgeReader<'a> {
    /// A reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            hit_end: false,
        }
    }

    /// Whether a read found no bytes left.
    pub fn hit_end(&self) -> bool {
        self.hit_end
    }
}

impl Read for EdgeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !buf.is_empty() && self.data.is_empty() {
            self.hit_end = true;
        }
        self.data.read(buf)
    }
}

fn ref_err(e: &ppmd_rust::Error) -> (ErrKind, bool) {
    match e {
        ppmd_rust::Error::InvalidParameter => (ErrKind::InvalidParameters, false),
        ppmd_rust::Error::RangeDecoderInitialization => (ErrKind::Corrupt, false),
        ppmd_rust::Error::IoError(io) if io.kind() == io::ErrorKind::UnexpectedEof => {
            (ErrKind::Truncated, true)
        }
        ppmd_rust::Error::IoError(_) => (ErrKind::Io, false),
        ppmd_rust::Error::MemoryAllocation => (ErrKind::Other, false),
    }
}

/// Decodes a 7z-coder stream with ppmd-rust: `known` bytes when given, else
/// to the end marker, the end of input or `cap`.
pub fn decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    known: Option<usize>,
    cap: usize,
) -> Reference {
    match Ppmd7Decoder::new(EdgeReader::new(stream), order, mem) {
        Ok(mut dec) => {
            let outcome = drain(&mut dec, known, cap);
            Reference {
                outcome,
                hit_input_end: dec.get_ref().hit_end(),
            }
        }
        Err(e) => {
            let (kind, hit_input_end) = ref_err(&e);
            Reference {
                outcome: Outcome::failed(kind),
                hit_input_end,
            }
        }
    }
}

/// Decodes a carry-less stream with ppmd-rust's `7a` decoder. Only for
/// streams an encoder produced: hostile input can make it divide by zero.
pub fn decode_carryless_trusted(
    stream: &[u8],
    order: u32,
    mem: u32,
    known: Option<usize>,
    cap: usize,
) -> Reference {
    match Ppmd7aDecoder::new(EdgeReader::new(stream), order, mem) {
        Ok(mut dec) => {
            let outcome = drain(&mut dec, known, cap);
            Reference {
                outcome,
                hit_input_end: dec.get_ref().hit_end(),
            }
        }
        Err(e) => {
            let (kind, hit_input_end) = ref_err(&e);
            Reference {
                outcome: Outcome::failed(kind),
                hit_input_end,
            }
        }
    }
}

pub use crate::synth::{encode_7z, encode_carryless};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::Verdict;
    use crate::payload::{Kind, generate};

    #[test]
    fn reference_round_trips() {
        let payload = generate(Kind::Text, 1, 3000);
        let stream = encode_7z(&payload, 6, 1 << 16, false);
        let r = decode_7z(&stream, 6, 1 << 16, Some(payload.len()), 1 << 20);
        assert_eq!(r.outcome.verdict, Verdict::Complete);
        assert_eq!(r.outcome.output, payload);

        let stream = encode_7z(&payload, 6, 1 << 16, true);
        let r = decode_7z(&stream, 6, 1 << 16, None, 1 << 20);
        assert_eq!(r.outcome.verdict, Verdict::Ended);
        assert_eq!(r.outcome.output, payload);
        assert!(!r.hit_input_end);

        let stream = encode_carryless(&payload, 6, 1 << 20, true);
        let r = decode_carryless_trusted(&stream, 6, 1 << 20, None, 1 << 20);
        assert_eq!(r.outcome.output, payload);
    }

    #[test]
    fn reference_reports_the_input_end() {
        let payload = generate(Kind::Text, 2, 3000);
        let stream = encode_7z(&payload, 6, 1 << 16, false);
        let r = decode_7z(
            &stream[..stream.len() / 2],
            6,
            1 << 16,
            Some(payload.len()),
            1 << 20,
        );
        assert!(r.hit_input_end);
        assert_eq!(r.outcome.verdict, Verdict::Ended);
        assert_eq!(r.outcome.output, payload[..r.outcome.output.len()]);
    }
}
