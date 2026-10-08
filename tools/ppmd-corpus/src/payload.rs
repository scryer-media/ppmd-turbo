//! Deterministic fixture payloads.
//!
//! Every payload is a pure function of its kind and size: a SplitMix64
//! generator seeded from both drives it, so `fixture-text-1m.bin` is the same
//! bytes on every host and in every run. Nothing here reads the clock, the
//! environment or a file. The content is invented: the "text" kind is word
//! salad over syllables the generator makes up, never real prose.

/// The shapes of input a PPMd decoder sees in practice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Word salad over an invented vocabulary, with punctuation and lines.
    Text,
    /// Little-endian records: counters, small deltas, tagged fields.
    Binary,
    /// 4 KiB runs of text, binary and random, interleaved.
    Mixed,
    /// One phrase repeated with rare mutations.
    Repetitive,
    /// Uniform random bytes: incompressible.
    Random,
}

impl Kind {
    /// Every kind, in manifest order.
    pub const ALL: [Kind; 5] = [
        Kind::Text,
        Kind::Binary,
        Kind::Mixed,
        Kind::Repetitive,
        Kind::Random,
    ];

    /// The kind's name in fixture names.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Binary => "binary",
            Kind::Mixed => "mixed",
            Kind::Repetitive => "repetitive",
            Kind::Random => "random",
        }
    }

    /// Parses a kind name.
    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// A size label in fixture names: `0`, `1`, `1k`, `64k`, `1m`, `16m`.
pub fn size_label(size: usize) -> String {
    if size >= 1 << 20 && size.is_multiple_of(1 << 20) {
        format!("{}m", size >> 20)
    } else if size >= 1 << 10 && size.is_multiple_of(1 << 10) {
        format!("{}k", size >> 10)
    } else {
        size.to_string()
    }
}

/// Parses a size label back to bytes.
pub fn parse_size(label: &str) -> Option<usize> {
    if let Some(n) = label.strip_suffix('m') {
        n.parse::<usize>().ok()?.checked_mul(1 << 20)
    } else if let Some(n) = label.strip_suffix('k') {
        n.parse::<usize>().ok()?.checked_mul(1 << 10)
    } else {
        label.parse().ok()
    }
}

/// `fixture-<kind>-<size>.bin`.
pub fn file_name(kind: Kind, size: usize) -> String {
    format!("fixture-{}-{}.bin", kind.name(), size_label(size))
}

/// Parses `fixture-<kind>-<size>.bin`.
pub fn parse_file_name(name: &str) -> Option<(Kind, usize)> {
    let stem = name.strip_prefix("fixture-")?.strip_suffix(".bin")?;
    let (kind, size) = stem.split_once('-')?;
    Some((Kind::parse(kind)?, parse_size(size)?))
}

/// SplitMix64: small, fast, and fully specified, so the payloads never move.
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (`n > 0`); the slight modulo bias is irrelevant here.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// The seed for a kind and size. FNV-1a over the kind name, mixed with the
/// size, so every (kind, size) pair is an independent stream.
pub fn seed(kind: Kind, size: usize) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in kind.name().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h ^ (size as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Generates the payload for `kind` at `size` bytes.
pub fn generate(kind: Kind, size: usize) -> Vec<u8> {
    let mut rng = SplitMix64::new(seed(kind, size));
    let mut out = Vec::with_capacity(size);
    match kind {
        Kind::Text => {
            let vocab = vocabulary(&mut rng);
            text(&mut rng, &vocab, &mut out, size);
        }
        Kind::Binary => binary(&mut rng, &mut out, size),
        Kind::Mixed => {
            let vocab = vocabulary(&mut rng);
            let mut part = 0u64;
            while out.len() < size {
                let end = (out.len() + 4096).min(size);
                match part % 3 {
                    0 => text(&mut rng, &vocab, &mut out, end),
                    1 => binary(&mut rng, &mut out, end),
                    _ => random(&mut rng, &mut out, end),
                }
                part += 1;
            }
        }
        Kind::Repetitive => {
            let phrase = b"fixture record 0000: the quick invented gloop vexes a plinth; ";
            let mut n = 0u32;
            while out.len() < size {
                let mut line = phrase.to_vec();
                let digits = format!("{:04}", n % 10_000);
                line[15..19].copy_from_slice(digits.as_bytes());
                if rng.below(64) == 0 {
                    let at = rng.below(line.len() as u64) as usize;
                    line[at] = b'a' + rng.below(26) as u8;
                }
                let take = line.len().min(size - out.len());
                out.extend_from_slice(&line[..take]);
                n += 1;
            }
        }
        Kind::Random => random(&mut rng, &mut out, size),
    }
    debug_assert_eq!(out.len(), size);
    out
}

/// 2048 invented words of one to four syllables.
fn vocabulary(rng: &mut SplitMix64) -> Vec<Vec<u8>> {
    const ONSETS: &[&[u8]] = &[
        b"b", b"d", b"f", b"g", b"k", b"l", b"m", b"n", b"p", b"r", b"s", b"t", b"v", b"z", b"br",
        b"dr", b"gl", b"pl", b"st", b"tr", b"qu", b"sh", b"th", b"",
    ];
    const NUCLEI: &[&[u8]] = &[
        b"a", b"e", b"i", b"o", b"u", b"ai", b"ou", b"ee", b"oo", b"y",
    ];
    const CODAS: &[&[u8]] = &[b"", b"", b"n", b"r", b"s", b"th", b"x", b"m", b"nd", b"ck"];
    (0..2048)
        .map(|_| {
            let syllables = 1 + rng.below(4);
            let mut word = Vec::new();
            for _ in 0..syllables {
                word.extend_from_slice(ONSETS[rng.below(ONSETS.len() as u64) as usize]);
                word.extend_from_slice(NUCLEI[rng.below(NUCLEI.len() as u64) as usize]);
                word.extend_from_slice(CODAS[rng.below(CODAS.len() as u64) as usize]);
            }
            word
        })
        .collect()
}

/// Appends word salad until `out` is `end` bytes long. Word choice is skewed
/// towards the front of the vocabulary, roughly as natural word frequency is.
fn text(rng: &mut SplitMix64, vocab: &[Vec<u8>], out: &mut Vec<u8>, end: usize) {
    let mut sentence_start = true;
    let mut line = 0usize;
    while out.len() < end {
        // min of two uniforms: a cheap skew towards small indices.
        let a = rng.below(vocab.len() as u64);
        let b = rng.below(vocab.len() as u64);
        let mut word = vocab[a.min(b) as usize].clone();
        if sentence_start {
            word[0] = word[0].to_ascii_uppercase();
            sentence_start = false;
        }
        let tail: &[u8] = match rng.below(20) {
            0 => {
                sentence_start = true;
                b". "
            }
            1 => b", ",
            2 if line > 60 => {
                line = 0;
                b"\n"
            }
            _ => b" ",
        };
        word.extend_from_slice(tail);
        line += word.len();
        let take = word.len().min(end - out.len());
        out.extend_from_slice(&word[..take]);
    }
}

/// Appends 16-byte little-endian records until `out` is `end` bytes long:
/// a running counter, a slowly drifting value, a tag from a small set and a
/// few noisy bytes.
fn binary(rng: &mut SplitMix64, out: &mut Vec<u8>, end: usize) {
    let mut counter = rng.below(1 << 20) as u32;
    let mut drift = rng.next_u64() as u32;
    while out.len() < end {
        let mut record = [0u8; 16];
        record[0..4].copy_from_slice(&counter.to_le_bytes());
        drift = drift.wrapping_add(rng.below(512) as u32);
        record[4..8].copy_from_slice(&drift.to_le_bytes());
        let tag: u32 = [0x0001_0000, 0x0002_0001, 0x0010_00FF, 0x7F45_4C46][rng.below(4) as usize];
        record[8..12].copy_from_slice(&tag.to_le_bytes());
        let noise = rng.next_u64().to_le_bytes();
        let noisy = rng.below(5) as usize;
        record[12..12 + noisy].copy_from_slice(&noise[..noisy]);
        counter = counter.wrapping_add(1);
        let take = record.len().min(end - out.len());
        out.extend_from_slice(&record[..take]);
    }
}

/// Appends uniform random bytes until `out` is `end` bytes long.
fn random(rng: &mut SplitMix64, out: &mut Vec<u8>, end: usize) {
    while out.len() < end {
        let word = rng.next_u64().to_le_bytes();
        let take = word.len().min(end - out.len());
        out.extend_from_slice(&word[..take]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for kind in Kind::ALL {
            for size in [0, 1, 1 << 10, 64 << 10, 1 << 20, 16 << 20] {
                let name = file_name(kind, size);
                assert_eq!(parse_file_name(&name), Some((kind, size)), "{name}");
            }
        }
        assert_eq!(file_name(Kind::Text, 1 << 20), "fixture-text-1m.bin");
    }

    #[test]
    fn payloads_are_deterministic_and_sized() {
        for kind in Kind::ALL {
            for size in [0, 1, 1000, 4097, 65536] {
                let a = generate(kind, size);
                assert_eq!(a.len(), size);
                assert_eq!(a, generate(kind, size));
            }
        }
    }

    #[test]
    fn kinds_differ() {
        let a = generate(Kind::Text, 4096);
        let b = generate(Kind::Random, 4096);
        assert_ne!(a, b);
        assert!(a.iter().all(|&c| c.is_ascii()));
    }
}
