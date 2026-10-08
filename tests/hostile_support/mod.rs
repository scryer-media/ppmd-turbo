//! Shared by the hostile-input and differential suites: the decoder API shim,
//! the no-panic wrapper, the fixtures under `tests/hostile_fixtures`, and a
//! deterministic PRNG.
//!
//! The shim has the shape of `tests/common/api.rs` (the conformance suites'),
//! over the crate's real decoders.

#![allow(dead_code)]

use std::io::{self, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use ppmd_turbo::{Error, Result};

pub mod api {
    //! The crate's API, one call per function.

    use super::*;

    #[allow(unused_imports)]
    pub use ppmd_turbo::rar::RarDecoder;

    /// The most output an unsized 7z decode collects. Garbage can decode to
    /// many symbols per input byte; past this the decode stops and returns
    /// what it has, which no expected payload reaches.
    pub const OUTPUT_CAP: usize = 4 << 20;

    /// Decodes a raw 7z `PPMD` stream: exactly `unpacked_len` bytes when it
    /// is given (7z's folder size), otherwise to the end marker or
    /// [`OUTPUT_CAP`].
    pub fn decode_7z(
        stream: &[u8],
        order: u32,
        mem_size: u32,
        unpacked_len: Option<u64>,
    ) -> Result<Vec<u8>> {
        let decoder = match unpacked_len {
            Some(n) => ppmd_turbo::Ppmd7Decoder::with_unpacked_size(stream, order, mem_size, n)?,
            None => ppmd_turbo::Ppmd7Decoder::new(stream, order, mem_size)?,
        };
        read_bounded(decoder, unpacked_len, OUTPUT_CAP)
    }

    /// Puts RAR's carry-less range decoder in the given registers, asks it to
    /// scale its range by `total`, and returns whether it reported the fault
    /// (`RangeDecoder::faulted`) instead of dividing by zero. No stream is
    /// needed: the registers are crafted so that `range / total == 0`.
    pub fn carryless_threshold_faults(low: u32, code: u32, range: u32, total: u32) -> bool {
        use ppmd_turbo::rc::{RangeCoderState, RangeDecoder, RarRangeDecoder};
        let mut rc = RarRangeDecoder::from_state(&[][..], RangeCoderState::new(low, code, range));
        let _ = rc.get_threshold(total);
        rc.faulted()
    }
}

/// Reads `reader` to exactly `known` bytes, or to its end, or to `cap`
/// bytes, one bounded `read` at a time. Running out before `known` is
/// `Error::Truncated`, as `read_exact` would report it.
pub fn read_bounded<R: Read>(mut reader: R, known: Option<u64>, cap: usize) -> Result<Vec<u8>> {
    let limit = match known {
        Some(n) => usize::try_from(n).map_err(|_| Error::InvalidParameters)?,
        None => cap,
    };
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    while out.len() < limit {
        let want = (limit - out.len()).min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) if known.is_some() => return Err(Error::Truncated),
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n.min(want)]),
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(out)
}

/// The class of an error, independent of the variants' shapes. Struct
/// patterns with `..` match unit, tuple and struct variants alike, so this
/// keeps compiling as variants gain fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// [`Error::InvalidParameters`].
    InvalidParameters,
    /// [`Error::CorruptStream`].
    Corrupt,
    /// [`Error::Truncated`], or an I/O `UnexpectedEof`.
    Truncated,
    /// Any other I/O error.
    Io,
    /// A variant this suite does not know.
    Other,
}

/// See [`Kind`].
pub fn kind(e: &Error) -> Kind {
    #[allow(unreachable_patterns, clippy::unneeded_struct_pattern)]
    match e {
        Error::InvalidParameters { .. } => Kind::InvalidParameters,
        Error::CorruptStream { .. } => Kind::Corrupt,
        Error::Truncated { .. } => Kind::Truncated,
        Error::Io(inner) => io_kind(inner),
        _ => Kind::Other,
    }
}

fn io_kind(e: &io::Error) -> Kind {
    if let Some(inner) = e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        return kind(inner);
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => Kind::Truncated,
        io::ErrorKind::InvalidData => Kind::Corrupt,
        io::ErrorKind::InvalidInput => Kind::InvalidParameters,
        _ => Kind::Io,
    }
}

/// Lower-case hex of at most the first 4 KiB of `data`, with the length.
pub fn hex(data: &[u8]) -> String {
    let shown = &data[..data.len().min(4096)];
    let mut s: String = shown.iter().map(|b| format!("{b:02x}")).collect();
    if shown.len() < data.len() {
        s.push_str(&format!("... ({} bytes)", data.len()));
    }
    s
}

/// Runs `f`; a panic fails the test with `what` and the input in hex.
pub fn no_panic<T>(what: &str, input: &[u8], f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!("{what}: panicked ({msg}) on input {}", hex(input));
        }
    }
}

/// Asserts `r` is an error of one of `kinds`.
#[track_caller]
pub fn assert_err<T: std::fmt::Debug>(what: &str, r: &Result<T>, kinds: &[Kind]) {
    match r {
        Err(e) => assert!(
            kinds.contains(&kind(e)),
            "{what}: {e:?} is not one of {kinds:?}"
        ),
        Ok(v) => panic!("{what}: expected one of {kinds:?}, got Ok({v:?})"),
    }
}

/// SplitMix64, so cut points and garbage are the same on every machine.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
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

    /// `len` bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// A stable seed from a name (FNV-1a), so each fixture gets its own cuts.
pub fn seed_of(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Which range coder a fixture uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coder {
    /// 7-Zip's coder: the 7z `PPMD` method.
    SevenZ,
    /// The carry-less coder: a RAR PPMd block's data.
    CarryLess,
}

/// One fixture from `tests/hostile_fixtures/index.txt`.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// File stem.
    pub name: String,
    /// Range coder.
    pub coder: Coder,
    /// Model order.
    pub order: u32,
    /// Arena size in bytes.
    pub mem: u32,
    /// Encoded with an end marker.
    pub end_marker: bool,
    /// The coded stream.
    pub stream: Vec<u8>,
    /// What it decodes to.
    pub payload: Vec<u8>,
}

impl Fixture {
    /// The arena in MiB, for the RAR API.
    pub fn mem_mb(&self) -> u32 {
        self.mem >> 20
    }
}

/// `tests/hostile_fixtures`.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("hostile_fixtures")
}

/// Every fixture, in index order. Generated by `fuzz/src/seeds.rs`.
pub fn fixtures() -> Vec<Fixture> {
    let dir = fixtures_dir();
    let index = std::fs::read_to_string(dir.join("index.txt")).expect("index.txt");
    index
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(f.len(), 6, "index line {line:?}");
            let read = |ext: &str| {
                let p = dir.join(format!("{}.{ext}", f[0]));
                std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
            };
            let fx = Fixture {
                name: f[0].to_string(),
                coder: match f[1] {
                    "7z" => Coder::SevenZ,
                    "carryless" => Coder::CarryLess,
                    other => panic!("coder {other}"),
                },
                order: f[2].parse().expect("order"),
                mem: f[3].parse().expect("mem"),
                end_marker: f[4] == "1",
                stream: read("stream"),
                payload: read("payload"),
            };
            assert_eq!(fx.payload.len(), f[5].parse::<usize>().expect("len"));
            fx
        })
        .collect()
}

/// The fixtures that use `coder`.
pub fn fixtures_of(coder: Coder) -> Vec<Fixture> {
    fixtures()
        .into_iter()
        .filter(|f| f.coder == coder)
        .collect()
}

/// The fixture called `name`.
pub fn fixture(name: &str) -> Fixture {
    fixtures()
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no fixture {name}"))
}

/// Cut points for a stream of `len` bytes: every length below
/// `min(len, 65)`, then 16 pseudo-random lengths below `len`.
pub fn cut_points(name: &str, len: usize) -> Vec<usize> {
    let mut cuts: Vec<usize> = (0..len.min(65)).collect();
    if len > 0 {
        let mut rng = Rng::new(seed_of(name));
        cuts.extend((0..16).map(|_| (rng.next_u64() % len as u64) as usize));
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}
