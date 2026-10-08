//! The structure-aware input of `structure_7z`.
//!
//! The fuzzer chooses parameters, a payload generator and a short list of
//! edits; the harness encodes the payload with ppmd-rust and applies the
//! edits to that valid stream. Most iterations therefore start from a stream
//! that decodes and then step just off it, which is where decoders
//! disagree: truncation inside a normalisation, a flipped bit that changes
//! one symbol, an insertion that shifts the coder by a byte.

use arbitrary::Arbitrary;

use crate::params::{MAX_ROUNDTRIP_MEM, mem_from, order_from};
use crate::payload::{Kind, generate};

/// The most edits one iteration applies.
pub const MAX_EDITS: usize = 8;

/// The longest generated payload.
pub const MAX_PAYLOAD: usize = 16 << 10;

/// One edit to the encoded stream. Positions wrap modulo the stream length.
#[derive(Debug, Clone, Arbitrary)]
pub enum Edit {
    /// XOR one byte with `mask`.
    Flip {
        /// Where.
        pos: u16,
        /// What to XOR (zero is a no-op).
        mask: u8,
    },
    /// Keep only the first `keep` bytes.
    Truncate {
        /// How many to keep.
        keep: u16,
    },
    /// Insert up to 16 bytes.
    Insert {
        /// Where.
        pos: u16,
        /// What.
        bytes: Vec<u8>,
    },
    /// Remove up to 255 bytes.
    Delete {
        /// Where.
        pos: u16,
        /// How many.
        len: u8,
    },
    /// Append bytes after the stream: trailing data.
    Append {
        /// What.
        bytes: Vec<u8>,
    },
}

/// One `structure_7z` iteration.
#[derive(Debug, Clone, Arbitrary)]
pub struct Case {
    order_sel: u8,
    mem_exp_sel: u8,
    mem_low: u16,
    /// The payload generator.
    pub kind: Kind,
    payload_seed: u32,
    payload_len: u16,
    /// Encode with an end marker.
    pub end_marker: bool,
    /// Decode with the payload's size known (the 7z shape).
    pub known_size: bool,
    /// The edits, in order.
    pub edits: Vec<Edit>,
}

impl Case {
    /// Model order.
    pub fn order(&self) -> u32 {
        order_from(self.order_sel)
    }

    /// Arena size.
    pub fn mem(&self) -> u32 {
        mem_from(self.mem_exp_sel, self.mem_low, MAX_ROUNDTRIP_MEM)
    }

    /// The payload.
    pub fn payload(&self) -> Vec<u8> {
        let len = usize::from(self.payload_len) % (MAX_PAYLOAD + 1);
        generate(self.kind, u64::from(self.payload_seed), len)
    }

    /// Applies the first [`MAX_EDITS`] edits to `stream`.
    pub fn apply(&self, stream: &mut Vec<u8>) {
        for edit in self.edits.iter().take(MAX_EDITS) {
            apply(edit, stream);
        }
    }
}

fn at(pos: u16, len: usize) -> usize {
    if len == 0 { 0 } else { usize::from(pos) % len }
}

/// Applies one edit.
pub fn apply(edit: &Edit, stream: &mut Vec<u8>) {
    match edit {
        Edit::Flip { pos, mask } => {
            if !stream.is_empty() {
                let i = at(*pos, stream.len());
                stream[i] ^= mask;
            }
        }
        Edit::Truncate { keep } => stream.truncate(usize::from(*keep)),
        Edit::Insert { pos, bytes } => {
            let i = at(*pos, stream.len() + 1);
            let bytes = &bytes[..bytes.len().min(16)];
            stream.splice(i..i, bytes.iter().copied());
        }
        Edit::Delete { pos, len } => {
            let i = at(*pos, stream.len());
            let end = (i + usize::from(*len)).min(stream.len());
            stream.drain(i..end);
        }
        Edit::Append { bytes } => stream.extend_from_slice(&bytes[..bytes.len().min(64)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_stay_in_bounds() {
        let edits = [
            Edit::Flip { pos: 9, mask: 1 },
            Edit::Truncate { keep: 3 },
            Edit::Insert {
                pos: 99,
                bytes: vec![1; 40],
            },
            Edit::Delete { pos: 5, len: 255 },
            Edit::Append {
                bytes: vec![2; 100],
            },
        ];
        for e in &edits {
            for len in [0usize, 1, 7] {
                let mut s = vec![0u8; len];
                apply(e, &mut s);
            }
        }
        let mut s = vec![0u8; 4];
        apply(&edits[2], &mut s);
        assert_eq!(s.len(), 20);
    }
}
