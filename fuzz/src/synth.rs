//! Deterministic test data that needs nothing but `std` and ppmd-rust: the
//! SplitMix64 generator, the invented payloads, ppmd-rust's two encoders and
//! the hostile-test fixtures built from them.
//!
//! The fuzz harness compiles this file as `crate::synth`, and the root
//! crate's hostile suites include the same file with `#[path]`, so `cargo
//! test` builds the hostile fixtures in memory through the very code that
//! `fuzz/src/seeds.rs` writes to `tests/hostile_fixtures/`. It must stay
//! free of other harness modules and of every dependency but ppmd-rust.

#![allow(dead_code)]

use std::io::Write;

use ppmd_rust::{Ppmd7Encoder, Ppmd7aEncoder};

/// SplitMix64: the harness's only source of pseudo-randomness, so seeds,
/// payloads and fixtures are identical on every machine.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound > 0`).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// `len` pseudo-random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// The shape of a generated payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// `len` bytes of `kind`, from `seed`. Nothing here is real text or a real
/// file: words come from a made-up vocabulary and binaries are synthetic.
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

/// Encodes with ppmd-rust's 7z coder. The parameters must be legal.
pub fn encode_7z(payload: &[u8], order: u32, mem: u32, end_marker: bool) -> Vec<u8> {
    let mut enc = Ppmd7Encoder::new(Vec::new(), order, mem).expect("legal 7z encoder parameters");
    enc.write_all(payload).expect("encoding into a Vec");
    enc.finish(end_marker).expect("finishing into a Vec")
}

/// Encodes with ppmd-rust's carry-less (`7a`) coder, the coder RAR's PPMd
/// blocks use. The parameters must be legal.
pub fn encode_carryless(payload: &[u8], order: u32, mem: u32, end_marker: bool) -> Vec<u8> {
    let mut enc =
        Ppmd7aEncoder::new(Vec::new(), order, mem).expect("legal carry-less encoder parameters");
    enc.write_all(payload).expect("encoding into a Vec");
    enc.finish(end_marker).expect("finishing into a Vec")
}

/// The recipe of one hostile fixture: a raw stream's parameters and its
/// payload generator.
#[derive(Debug, Clone)]
pub struct HostileRecipe {
    /// File stem.
    pub name: &'static str,
    /// `7z` or `carryless`.
    pub coder: &'static str,
    /// Model order.
    pub order: u32,
    /// Arena size in bytes (a whole number of MiB for `carryless`).
    pub mem: u32,
    /// Encoded with an end marker.
    pub end_marker: bool,
    /// Payload generator.
    pub kind: Kind,
    /// Payload length.
    pub len: usize,
}

/// The hostile fixtures, in index order. The payload of entry `i` is
/// generated from seed `400 + i`.
pub const HOSTILE: [HostileRecipe; 10] = [
    HostileRecipe {
        name: "z7-text-o6-m64k",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: false,
        kind: Kind::Text,
        len: 3000,
    },
    HostileRecipe {
        name: "z7-text-o6-m64k-eos",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: true,
        kind: Kind::Text,
        len: 3000,
    },
    HostileRecipe {
        name: "z7-records-o2-m2k",
        coder: "7z",
        order: 2,
        mem: 2048,
        end_marker: false,
        kind: Kind::Records,
        len: 2000,
    },
    HostileRecipe {
        name: "z7-ramp-o16-m1m-eos",
        coder: "7z",
        order: 16,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Ramp,
        len: 1024,
    },
    HostileRecipe {
        name: "z7-empty-o6-m64k",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: false,
        kind: Kind::Text,
        len: 0,
    },
    HostileRecipe {
        name: "z7-empty-o6-m64k-eos",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: true,
        kind: Kind::Text,
        len: 0,
    },
    HostileRecipe {
        name: "z7-long-o64-m2k",
        coder: "7z",
        order: 64,
        mem: 2048,
        end_marker: false,
        kind: Kind::Text,
        len: 16 << 10,
    },
    HostileRecipe {
        name: "cl-text-o6-m1-eos",
        coder: "carryless",
        order: 6,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Text,
        len: 3000,
    },
    HostileRecipe {
        name: "cl-records-o16-m1",
        coder: "carryless",
        order: 16,
        mem: 1 << 20,
        end_marker: false,
        kind: Kind::Records,
        len: 2000,
    },
    HostileRecipe {
        name: "cl-random-o4-m1-eos",
        coder: "carryless",
        order: 4,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Random,
        len: 1500,
    },
];

/// The header line of `tests/hostile_fixtures/index.txt`.
pub const HOSTILE_INDEX_HEADER: &str = "# Generated by fuzz/src/seeds.rs; do not edit. Columns: name coder order mem end_marker payload_len\n";

/// One built hostile fixture.
#[derive(Debug, Clone)]
pub struct HostileFixture {
    /// Its recipe.
    pub recipe: &'static HostileRecipe,
    /// The coded stream.
    pub stream: Vec<u8>,
    /// What it decodes to.
    pub payload: Vec<u8>,
}

impl HostileFixture {
    /// Its line in `index.txt`.
    pub fn index_line(&self) -> String {
        let r = self.recipe;
        format!(
            "{} {} {} {} {} {}\n",
            r.name,
            r.coder,
            r.order,
            r.mem,
            u8::from(r.end_marker),
            self.payload.len()
        )
    }
}

/// Builds every hostile fixture, in index order.
pub fn hostile_fixtures() -> Vec<HostileFixture> {
    HOSTILE
        .iter()
        .enumerate()
        .map(|(i, recipe)| {
            let payload = generate(recipe.kind, 400 + i as u64, recipe.len);
            let stream = match recipe.coder {
                "7z" => encode_7z(&payload, recipe.order, recipe.mem, recipe.end_marker),
                _ => encode_carryless(&payload, recipe.order, recipe.mem, recipe.end_marker),
            };
            HostileFixture {
                recipe,
                stream,
                payload,
            }
        })
        .collect()
}
