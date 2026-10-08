//! Invented payloads. Nothing here is real text or a real file: words come
//! from a made-up vocabulary and binaries are synthetic.

use arbitrary::Arbitrary;

use crate::SplitMix64;

/// The shape of a generated payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Arbitrary)]
pub enum Kind {
    /// Prose from an invented vocabulary: low entropy, deep contexts.
    Text,
    /// Runs of a few repeated bytes: binary contexts and long successor chains.
    Runs,
    /// Every byte value in turn with occasional jumps: wide order-0 contexts.
    Ramp,
    /// Fixed-size little-endian records: mixed binary with structure.
    Records,
    /// Uniform random bytes: escape-heavy, the model never settles.
    Random,
}

impl Kind {
    /// Every kind, in a fixed order.
    pub const ALL: [Self; 5] = [
        Self::Text,
        Self::Runs,
        Self::Ramp,
        Self::Records,
        Self::Random,
    ];

    /// A short name for file names.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Runs => "runs",
            Self::Ramp => "ramp",
            Self::Records => "records",
            Self::Random => "random",
        }
    }
}

const WORDS: &[&str] = &[
    "brindle", "quovar", "talsen", "mirrow", "vexa", "plunder", "osk", "tremai", "galvo",
    "dunmere", "sorrel", "kith", "yandle", "pemberly", "zorrin", "flax", "hollow", "ivet",
    "carrow", "nemble", "a", "the", "of", "and", "to", "in", "upon", "beneath",
];

/// `len` bytes of `kind`, from `seed`.
pub fn generate(kind: Kind, seed: u64, len: usize) -> Vec<u8> {
    let mut rng = SplitMix64::new(seed ^ 0x5050_4D44_7475_7262);
    let mut out = Vec::with_capacity(len + 16);
    match kind {
        Kind::Text => {
            let mut sentence = 0;
            while out.len() < len {
                let word = WORDS[rng.below(WORDS.len() as u64) as usize];
                if sentence == 0 {
                    let mut chars = word.bytes();
                    if let Some(first) = chars.next() {
                        out.push(first.to_ascii_uppercase());
                    }
                    out.extend(chars);
                } else {
                    out.extend_from_slice(word.as_bytes());
                }
                sentence += 1;
                match rng.below(16) {
                    0 if sentence > 3 => {
                        out.extend_from_slice(b".\n");
                        sentence = 0;
                    }
                    1 if sentence > 3 => {
                        out.extend_from_slice(b". ");
                        sentence = 0;
                    }
                    2 => out.extend_from_slice(b", "),
                    _ => out.push(b' '),
                }
            }
        }
        Kind::Runs => {
            let alphabet = rng.bytes(4);
            while out.len() < len {
                let byte = alphabet[rng.below(4) as usize];
                let run = 1 + rng.below(64) as usize;
                out.extend(std::iter::repeat_n(byte, run));
            }
        }
        Kind::Ramp => {
            let mut b = rng.next_u64() as u8;
            while out.len() < len {
                out.push(b);
                b = if rng.below(32) == 0 {
                    rng.next_u64() as u8
                } else {
                    b.wrapping_add(1)
                };
            }
        }
        Kind::Records => {
            let mut counter = rng.below(1000) as u32;
            while out.len() < len {
                counter = counter.wrapping_add(1 + rng.below(3) as u32);
                out.extend_from_slice(&counter.to_le_bytes());
                out.extend_from_slice(&[0, 0, rng.below(4) as u8, 0x7F]);
                out.extend_from_slice(&(rng.below(1 << 16) as u16).to_le_bytes());
                out.extend_from_slice(b"rec\0");
            }
        }
        Kind::Random => out = rng.bytes(len),
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_length_and_deterministic() {
        for kind in Kind::ALL {
            for len in [0, 1, 17, 4096] {
                let a = generate(kind, 7, len);
                assert_eq!(a.len(), len);
                assert_eq!(a, generate(kind, 7, len));
            }
        }
    }
}
