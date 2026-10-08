//! Invented payloads. Nothing here is real text or a real file: words come
//! from a made-up vocabulary and binaries are synthetic. The generator
//! itself lives in [`crate::synth`], which the root crate's hostile suites
//! share; this module adds what only the fuzz targets need.

use arbitrary::{Arbitrary, Result, Unstructured};

pub use crate::synth::{Kind, generate};

/// The encoding `#[derive(Arbitrary)]` gives a five-variant enum, kept so
/// existing corpora decode to the same cases.
impl<'a> Arbitrary<'a> for Kind {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let pick = (u64::from(u32::arbitrary(u)?) * Kind::ALL.len() as u64) >> 32;
        Ok(Kind::ALL[pick as usize])
    }

    fn size_hint(depth: usize) -> (usize, Option<usize>) {
        u32::size_hint(depth)
    }
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

    #[test]
    fn arbitrary_matches_the_derive() {
        for (raw, want) in [
            (0u32, Kind::Text),
            (u32::MAX / 5, Kind::Text),
            (u32::MAX / 5 + 1, Kind::Runs),
            (u32::MAX / 2, Kind::Ramp),
            (u32::MAX, Kind::Random),
        ] {
            let bytes = raw.to_le_bytes();
            let mut u = Unstructured::new(&bytes);
            assert_eq!(Kind::arbitrary(&mut u).unwrap(), want, "{raw:#x}");
        }
    }
}
