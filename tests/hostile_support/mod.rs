//! Shared by the hostile-input and differential suites: the decoder API shim,
//! the no-panic wrapper, the hostile fixtures, and a deterministic PRNG.
//!
//! The fixtures are built in memory by `fuzz/src/synth.rs`, the same code
//! that writes `tests/hostile_fixtures/` for the fuzz harness, so these
//! suites need no generated files on disk.
//!
//! The shim has the shape of `tests/common/api.rs` (the conformance suites'),
//! over the crate's real decoders.

#![allow(dead_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use ppmd_turbo::{Error, ErrorKind, Result};

/// The crate's API, one call per function (`tests/common/api.rs`).
#[path = "../common/api.rs"]
pub mod api;

/// The fuzz harness's std-only generator: payloads, ppmd-rust's encoders and
/// the hostile fixture recipes.
#[path = "../../fuzz/src/synth.rs"]
pub mod synth;

/// The class of an error. `ErrorKind` is non-exhaustive, so this keeps
/// compiling as kinds are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// [`ErrorKind::InvalidParameters`].
    InvalidParameters,
    /// [`ErrorKind::Corrupt`].
    Corrupt,
    /// [`ErrorKind::Truncated`].
    Truncated,
    /// A memory refusal.
    Memory,
    /// A kind this suite does not know.
    Other,
}

/// See [`Kind`].
pub fn kind(e: &Error) -> Kind {
    match e.kind {
        ErrorKind::InvalidParameters => Kind::InvalidParameters,
        ErrorKind::Corrupt(_) => Kind::Corrupt,
        ErrorKind::Truncated => Kind::Truncated,
        ErrorKind::AllocationFailed { .. } | ErrorKind::MemoryLimit { .. } => Kind::Memory,
        _ => Kind::Other,
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

/// One hostile fixture (`synth::HOSTILE`).
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

/// Every fixture, in index order, built once per test binary.
pub fn fixtures() -> Vec<Fixture> {
    static BUILT: OnceLock<Vec<Fixture>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            synth::hostile_fixtures()
                .into_iter()
                .map(|f| Fixture {
                    name: f.recipe.name.to_string(),
                    coder: match f.recipe.coder {
                        "7z" => Coder::SevenZ,
                        "carryless" => Coder::CarryLess,
                        other => panic!("coder {other}"),
                    },
                    order: f.recipe.order,
                    mem: f.recipe.mem,
                    end_marker: f.recipe.end_marker,
                    stream: f.stream,
                    payload: f.payload,
                })
                .collect()
        })
        .clone()
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
