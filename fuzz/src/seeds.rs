//! The committed seed corpora (`fuzz/seeds/<target>/`) and the hostile-test
//! fixtures (`tests/hostile_fixtures/`), generated deterministically from
//! invented payloads and ppmd-rust's encoders.
//!
//! `cargo test --locked --manifest-path fuzz/Cargo.toml` checks that the
//! committed files are exactly what this module generates. To regenerate
//! after changing it:
//!
//! ```text
//! cargo test --locked --manifest-path fuzz/Cargo.toml -- --ignored regenerate_seeds
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::SplitMix64;
use crate::layout::{Decode7z, RarBlock, Roundtrip7z, RoundtripCarryless};
use crate::payload::{Kind, generate};
use crate::reference::{encode_7z, encode_carryless};

/// Every fuzz target, in the order CI lists them.
pub const TARGETS: [&str; 6] = [
    "decode_7z",
    "decode_rar",
    "decode_differential_7z",
    "roundtrip_7z",
    "roundtrip_carryless",
    "structure_7z",
];

/// The directories this module owns, relative to the fuzz crate, and every
/// file it writes into them.
pub type Files = BTreeMap<PathBuf, Vec<u8>>;

/// The hostile fixtures directory, relative to the fuzz crate.
pub const HOSTILE_DIR: &str = "../tests/hostile_fixtures";

fn seed_dir(target: &str) -> PathBuf {
    Path::new("seeds").join(target)
}

fn name(kind: Kind, order: u32, mem: u32, marker: bool, known: bool) -> String {
    format!(
        "{}-o{order}-m{mem}-{}-{}",
        kind.name(),
        if marker { "eos" } else { "noeos" },
        if known { "sized" } else { "unsized" }
    )
}

fn decode_7z_seeds(files: &mut Files) {
    let cases: [(Kind, u32, u32, usize, bool, bool); 10] = [
        (Kind::Text, 6, 1 << 16, 2000, false, true),
        (Kind::Text, 6, 1 << 16, 2000, true, false),
        (Kind::Runs, 2, 2048, 3000, true, false),
        (Kind::Ramp, 16, 1 << 20, 1500, false, true),
        (Kind::Records, 32, 1 << 16, 2500, true, true),
        (Kind::Random, 4, 4096, 600, false, true),
        (Kind::Text, 64, 2048, 4000, true, false),
        (Kind::Text, 8, 64 << 20, 1000, true, false),
        (Kind::Text, 6, 1 << 16, 0, true, false),
        (Kind::Text, 6, 1 << 16, 0, false, true),
    ];
    let mut out = Vec::new();
    for (i, (kind, order, mem, len, marker, known)) in cases.into_iter().enumerate() {
        let payload = generate(kind, 100 + i as u64, len);
        let stream = encode_7z(&payload, order, mem, marker);
        let size = known.then(|| u16::try_from(len).expect("seed payload fits u16"));
        out.push((
            format!("{}-len{len}", name(kind, order, mem, marker, known)),
            Decode7z::seed(order, mem, size, &stream),
        ));
    }
    let valid = encode_7z(&generate(Kind::Text, 1, 300), 6, 1 << 16, true);
    for bad in 0..4u8 {
        out.push((
            format!("invalid-order-{bad}"),
            Decode7z::seed_invalid(Some(bad), None, &valid),
        ));
        out.push((
            format!("invalid-mem-{bad}"),
            Decode7z::seed_invalid(None, Some(bad), &valid),
        ));
    }
    let mut rng = SplitMix64::new(0x7A);
    let mut garbage = rng.bytes(96);
    garbage[0] = 0;
    out.push((
        "garbage-unsized".into(),
        Decode7z::seed(6, 1 << 16, None, &garbage),
    ));
    out.push((
        "garbage-sized".into(),
        Decode7z::seed(16, 2048, Some(5000), &garbage),
    ));
    garbage[0] = 0xFF;
    out.push((
        "garbage-bad-first-byte".into(),
        Decode7z::seed(6, 1 << 16, None, &garbage),
    ));
    out.push((
        "code-all-ones".into(),
        Decode7z::seed(6, 1 << 16, None, &[0, 0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3]),
    ));
    out.push((
        "empty-stream".into(),
        Decode7z::seed(6, 1 << 16, Some(1), &[]),
    ));
    for target in ["decode_7z", "decode_differential_7z"] {
        for (n, data) in &out {
            files.insert(seed_dir(target).join(n), data.clone());
        }
    }
}

fn decode_rar_seeds(files: &mut Files) {
    let text = generate(Kind::Text, 200, 2000);
    let records = generate(Kind::Records, 201, 2000);
    let a = encode_carryless(&text, 6, 1 << 20, true);
    let b = encode_carryless(&records, 16, 2 << 20, false);
    let long = encode_carryless(&generate(Kind::Text, 202, 4000), 64, 1 << 20, true);
    let mut put = |n: &str, blocks: Vec<Vec<u8>>| {
        files.insert(seed_dir("decode_rar").join(n), blocks.concat());
    };
    put(
        "single-reset-eos",
        vec![RarBlock::seed(true, true, 6, 1, None, &a)],
    );
    put(
        "single-reset-sized",
        vec![RarBlock::seed(true, true, 16, 2, Some(2000), &b)],
    );
    put(
        "order64-mem1-eos",
        vec![RarBlock::seed(true, true, 64, 1, None, &long)],
    );
    put(
        "solid-continuation",
        vec![
            RarBlock::seed(true, true, 6, 1, None, &a),
            RarBlock::seed(false, false, 6, 1, None, &a),
            RarBlock::seed(false, true, 16, 2, Some(2000), &b),
        ],
    );
    put(
        "no-model-yet",
        vec![RarBlock::seed(true, false, 6, 1, None, &a)],
    );
    put(
        "restart-storm",
        (0..8)
            .map(|_| RarBlock::seed(false, true, 6, 1, None, &a))
            .collect(),
    );
    put(
        "size-lie-no-marker",
        vec![RarBlock::seed(true, true, 16, 2, None, &b)],
    );
    put(
        "empty-block",
        vec![
            RarBlock::seed(true, true, 6, 1, None, &[]),
            RarBlock::seed(false, true, 6, 1, None, &a),
        ],
    );
    // Hand-built headers: invalid order (flags bit 1), invalid arena (bit 2).
    let raw = |flags: u8, sel: u8, data: &[u8]| {
        let len = (data.len() as u16).to_le_bytes();
        let mut v = vec![flags, sel, sel, 0xFF, 0xFF, len[0], len[1]];
        v.extend_from_slice(data);
        v
    };
    put(
        "invalid-order",
        (0..4).map(|sel| raw(0x11 | 2, sel, &a)).collect(),
    );
    put(
        "invalid-arena",
        (0..3).map(|sel| raw(0x11 | 4, sel, &a)).collect(),
    );
}

fn roundtrip_seeds(files: &mut Files) {
    let cases: [(Kind, u32, u32, bool, usize); 7] = [
        (Kind::Text, 6, 1 << 16, true, 1500),
        (Kind::Runs, 2, 2048, false, 2000),
        (Kind::Ramp, 16, 1 << 20, true, 1024),
        (Kind::Records, 32, 1 << 16, false, 1500),
        (Kind::Random, 4, 4096, true, 400),
        (Kind::Text, 64, 2048, false, 3000),
        (Kind::Text, 6, 1 << 16, true, 0),
    ];
    for (i, (kind, order, mem, marker, len)) in cases.into_iter().enumerate() {
        let payload = generate(kind, 300 + i as u64, len);
        files.insert(
            seed_dir("roundtrip_7z")
                .join(format!("{}-len{len}", name(kind, order, mem, marker, true))),
            Roundtrip7z::seed(order, mem, marker, &payload),
        );
        let mem_mb = 1 + (i as u32 % 3);
        files.insert(
            seed_dir("roundtrip_carryless").join(format!(
                "{}-len{len}",
                name(kind, order, mem_mb << 20, marker, true)
            )),
            RoundtripCarryless::seed(order, mem_mb, marker, &payload),
        );
    }
}

fn structure_seeds(files: &mut Files) {
    let mut rng = SplitMix64::new(0x5354_5255);
    for i in 0..12 {
        let len = 32 + rng.below(224) as usize;
        files.insert(
            seed_dir("structure_7z").join(format!("prng-{i:02}")),
            rng.bytes(len),
        );
    }
}

/// One hostile fixture: a raw stream, its parameters and its payload.
#[derive(Debug, Clone)]
pub struct Fixture {
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

/// The hostile fixtures. `tests/hostile_decode.rs` reads them through
/// `tests/hostile_fixtures/index.txt`.
pub const FIXTURES: [Fixture; 10] = [
    Fixture {
        name: "z7-text-o6-m64k",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: false,
        kind: Kind::Text,
        len: 3000,
    },
    Fixture {
        name: "z7-text-o6-m64k-eos",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: true,
        kind: Kind::Text,
        len: 3000,
    },
    Fixture {
        name: "z7-records-o2-m2k",
        coder: "7z",
        order: 2,
        mem: 2048,
        end_marker: false,
        kind: Kind::Records,
        len: 2000,
    },
    Fixture {
        name: "z7-ramp-o16-m1m-eos",
        coder: "7z",
        order: 16,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Ramp,
        len: 1024,
    },
    Fixture {
        name: "z7-empty-o6-m64k",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: false,
        kind: Kind::Text,
        len: 0,
    },
    Fixture {
        name: "z7-empty-o6-m64k-eos",
        coder: "7z",
        order: 6,
        mem: 1 << 16,
        end_marker: true,
        kind: Kind::Text,
        len: 0,
    },
    Fixture {
        name: "z7-long-o64-m2k",
        coder: "7z",
        order: 64,
        mem: 2048,
        end_marker: false,
        kind: Kind::Text,
        len: 16 << 10,
    },
    Fixture {
        name: "cl-text-o6-m1-eos",
        coder: "carryless",
        order: 6,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Text,
        len: 3000,
    },
    Fixture {
        name: "cl-records-o16-m1",
        coder: "carryless",
        order: 16,
        mem: 1 << 20,
        end_marker: false,
        kind: Kind::Records,
        len: 2000,
    },
    Fixture {
        name: "cl-random-o4-m1-eos",
        coder: "carryless",
        order: 4,
        mem: 1 << 20,
        end_marker: true,
        kind: Kind::Random,
        len: 1500,
    },
];

fn hostile_fixtures(files: &mut Files) {
    let dir = Path::new(HOSTILE_DIR);
    let mut index = String::from(
        "# Generated by fuzz/src/seeds.rs; do not edit. Columns: name coder order mem end_marker payload_len\n",
    );
    for (i, f) in FIXTURES.iter().enumerate() {
        let payload = generate(f.kind, 400 + i as u64, f.len);
        let stream = match f.coder {
            "7z" => encode_7z(&payload, f.order, f.mem, f.end_marker),
            _ => encode_carryless(&payload, f.order, f.mem, f.end_marker),
        };
        index.push_str(&format!(
            "{} {} {} {} {} {}\n",
            f.name,
            f.coder,
            f.order,
            f.mem,
            u8::from(f.end_marker),
            payload.len()
        ));
        files.insert(dir.join(format!("{}.stream", f.name)), stream);
        files.insert(dir.join(format!("{}.payload", f.name)), payload);
    }
    files.insert(dir.join("index.txt"), index.into_bytes());
}

/// Every generated file, keyed by its path relative to the fuzz crate.
pub fn all() -> Files {
    let mut files = Files::new();
    decode_7z_seeds(&mut files);
    decode_rar_seeds(&mut files);
    roundtrip_seeds(&mut files);
    structure_seeds(&mut files);
    hostile_fixtures(&mut files);
    files
}

/// The directories [`all`] owns: anything else in them is stale.
pub fn owned_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = TARGETS.iter().map(|t| seed_dir(t)).collect();
    dirs.push(PathBuf::from(HOSTILE_DIR));
    dirs
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn listed(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(root().join(dir))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| dir.join(e.file_name()))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn every_target_has_seeds() {
        let files = all();
        for t in TARGETS {
            let n = files.keys().filter(|p| p.starts_with(seed_dir(t))).count();
            assert!(n >= 5, "{t} has {n} seeds");
        }
    }

    #[test]
    fn seeds_are_current() {
        let files = all();
        for (path, data) in &files {
            let on_disk = fs::read(root().join(path))
                .unwrap_or_else(|e| panic!("{}: {e}; run regenerate_seeds", path.display()));
            assert!(
                on_disk == *data,
                "{} is stale; run regenerate_seeds",
                path.display()
            );
        }
        for dir in owned_dirs() {
            for path in listed(&dir) {
                assert!(
                    files.contains_key(&path),
                    "{} is not generated",
                    path.display()
                );
            }
        }
    }

    #[test]
    #[ignore = "writes the committed seeds and fixtures; run by hand"]
    fn regenerate_seeds() {
        let files = all();
        for dir in owned_dirs() {
            fs::create_dir_all(root().join(&dir)).unwrap();
            for path in listed(&dir) {
                if !files.contains_key(&path) {
                    fs::remove_file(root().join(&path)).unwrap();
                }
            }
        }
        for (path, data) in &files {
            fs::write(root().join(path), data).unwrap();
        }
    }
}
