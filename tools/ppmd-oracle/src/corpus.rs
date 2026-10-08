//! Invented payloads for oracle runs. No real text or files: words come from
//! a made-up vocabulary and binaries are synthetic. Standard library only,
//! so `tests/differential_binaries.rs` includes this file with `#[path]`.

/// SplitMix64: a small deterministic generator.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
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

    /// A value below `n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "brindle", "quovar", "talsen", "mirrow", "vexa", "osk", "tremai", "galvo", "dunmere", "sorrel",
    "kith", "yandle", "zorrin", "flax", "ivet", "carrow", "nemble", "the", "of", "upon", "beneath",
];

/// Prose from the invented vocabulary.
pub fn text(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
        out.extend_from_slice(match rng.below(12) {
            0 => b".\n",
            1 => b", ",
            _ => b" ",
        });
    }
    out.truncate(len);
    out
}

/// Every byte value in turn, with occasional jumps.
pub fn ramp(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut b = rng.next_u64() as u8;
    (0..len)
        .map(|_| {
            let v = b;
            b = if rng.below(32) == 0 {
                rng.next_u64() as u8
            } else {
                b.wrapping_add(1)
            };
            v
        })
        .collect()
}

/// Fixed-size little-endian records.
pub fn records(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 16);
    let mut counter = rng.below(1000) as u32;
    while out.len() < len {
        counter = counter.wrapping_add(1 + rng.below(3) as u32);
        out.extend_from_slice(&counter.to_le_bytes());
        out.extend_from_slice(&[0, rng.below(4) as u8, 0x7F, 0]);
        out.extend_from_slice(b"rec\0");
    }
    out.truncate(len);
    out
}

/// Uniform random bytes.
pub fn random(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// The default oracle corpus: `(name, data)` pairs, deterministic.
pub fn default_corpus() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for (i, len) in [1usize, 37, 4096, 70_000].into_iter().enumerate() {
        let seed = 0x0DD5_EED0 + i as u64;
        out.push((format!("text-{len}"), text(seed, len)));
        out.push((format!("ramp-{len}"), ramp(seed, len)));
        out.push((format!("records-{len}"), records(seed, len)));
        out.push((format!("random-{len}"), random(seed, len)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "no unsafe code; slow under Miri")]
    fn deterministic_and_exact() {
        for (name, data) in default_corpus() {
            let len: usize = name
                .rsplit('-')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            assert_eq!(data.len(), len, "{name}");
        }
        assert_eq!(default_corpus(), default_corpus());
    }
}
