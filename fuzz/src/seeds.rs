//! The seed corpora (`fuzz/seeds/<target>/`), the minimised fuzz
//! regressions (`fuzz/regressions/<target>/`) and the hostile-test fixtures
//! (`tests/hostile_fixtures/`), generated deterministically from invented
//! payloads, ppmd-rust's encoders and the byte literals in [`REGRESSIONS`].
//!
//! None of these files is committed. The repository's one entry point,
//! `cargo run --locked -p ppmd-corpus -- fixtures`, writes them through
//! [`write_all`] (by running this crate's `regenerate_seeds` example) and
//! checks every file against `tools/ppmd-corpus/fixtures.sha256`. To write
//! only these files:
//!
//! ```text
//! cargo run --locked --manifest-path fuzz/Cargo.toml --example regenerate_seeds
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::SplitMix64;
use crate::layout::{Decode7z, RarBlock, Roundtrip7z, RoundtripCarryless};
use crate::ops;
use crate::paths;
use crate::payload::{Kind, generate};
use crate::reference::{encode_7z, encode_carryless};
use crate::synth;

/// Every fuzz target, in the order CI lists them.
pub const TARGETS: [&str; 9] = [
    "decode_7z",
    "decode_rar",
    "decode_differential_7z",
    "roundtrip_7z",
    "roundtrip_carryless",
    "structure_7z",
    "range_coders",
    "checked_vs_unchecked",
    "model_ops",
];

/// Every file this module writes, keyed by its path relative to the
/// repository root.
pub type Files = BTreeMap<PathBuf, Vec<u8>>;

/// The hostile fixtures directory, relative to the repository root.
pub const HOSTILE_DIR: &str = "tests/hostile_fixtures";

/// The fuzz regressions directory, relative to the repository root.
pub const REGRESSIONS_DIR: &str = "fuzz/regressions";

fn seed_dir(target: &str) -> PathBuf {
    Path::new("fuzz/seeds").join(target)
}

/// Minimised inputs from past fuzz findings, replayed by both fuzz workflows
/// on every run: `(target, name, input)`. A new finding is added here as a
/// byte literal, never committed as a file.
pub const REGRESSIONS: &[(&str, &str, &[u8])] = &[
    // A RAR block without a model and nothing to decode: corrupt on both
    // sides of checked_vs_unchecked before the coder reads its four bytes.
    (
        "checked_vs_unchecked",
        "rar-no-model-nothing-to-decode",
        &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x02, 0x03,
        ],
    ),
    // The same with too few coder bytes for the range decoder to start.
    (
        "checked_vs_unchecked",
        "rar-no-model-short-coder-data",
        &[0x7E, 0x40, 0x04, 0x05, 0x02, 0x00, 0x01, 0x00, 0x28],
    ),
];

fn regressions(files: &mut Files) {
    for (target, name, input) in REGRESSIONS {
        files.insert(
            Path::new(REGRESSIONS_DIR).join(target).join(name),
            input.to_vec(),
        );
    }
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

fn range_coder_seeds(files: &mut Files) {
    let mut rng = SplitMix64::new(0x5243);
    let mut put = |n: &str, split: u8, stream: &[u8], script: &[u8]| {
        let mut v = vec![split];
        v.extend_from_slice(&stream[..usize::from(split).min(stream.len())]);
        v.extend_from_slice(script);
        files.insert(seed_dir("range_coders").join(n), v);
    };
    let z7 = encode_7z(&generate(Kind::Text, 500, 400), 6, 1 << 16, true);
    let cl = encode_carryless(&generate(Kind::Text, 501, 400), 6, 1 << 20, true);
    let script = rng.bytes(9 * 64);
    put("sevenz-stream", 255, &z7, &script);
    put("carryless-stream", 255, &cl, &script);
    put("short-stream", 3, &cl, &script);
    put("no-stream", 0, &[], &script);
    // Operation kinds 0, 1, 2 in turn, with totals and sizes at the edges.
    let mut edges = Vec::new();
    for (i, (a, b)) in [
        (0u32, 0u32),
        (1, 1),
        (u32::MAX, u32::MAX),
        (0x8000, 0x7FFF),
        (16384, 95),
        (65535, 65535),
        (1 << 15, 1),
    ]
    .into_iter()
    .enumerate()
    {
        for kind in 0..3u8 {
            edges.push(kind + 3 * i as u8);
            edges.extend_from_slice(&a.to_le_bytes());
            edges.extend_from_slice(&b.to_le_bytes());
        }
    }
    put("edge-operations", 64, &rng.bytes(64), &edges);
    let garbage = rng.bytes(512);
    put("garbage", 128, &garbage, &garbage);
}

/// `checked_vs_unchecked` reuses the decode seeds behind its mode byte,
/// cycling the refill size.
fn checked_vs_unchecked_seeds(files: &mut Files) {
    let mut seeds = Vec::new();
    for (target, sevenz) in [("decode_rar", false), ("decode_7z", true)] {
        let dir = seed_dir(target);
        for (path, data) in files.range(dir.clone()..) {
            if !path.starts_with(&dir) {
                break;
            }
            let name = path.file_name().expect("seed file name").to_string_lossy();
            let refill = 1 + seeds.len() % 8;
            let mut v = vec![paths::mode(sevenz, refill)];
            v.extend_from_slice(data);
            let prefix = if sevenz { "7z" } else { "rar" };
            seeds.push((format!("{prefix}-r{refill}-{name}"), v));
        }
    }
    for (n, v) in seeds {
        files.insert(seed_dir("checked_vs_unchecked").join(n), v);
    }
}

fn model_ops_seeds(files: &mut Files) {
    let mut rng = SplitMix64::new(0x4F50);
    let cl = encode_carryless(&generate(Kind::Records, 510, 3000), 16, 2 << 20, false);
    let z7 = encode_7z(&generate(Kind::Text, 511, 3000), 6, 1 << 16, false);
    let mut put = |n: &str, op_list: &[[u8; 4]], pool: &[u8]| {
        files.insert(seed_dir("model_ops").join(n), ops::seed(op_list, pool));
    };
    // Every kind once, over a valid carry-less stream.
    let every: Vec<[u8; 4]> = (0..ops::OP_KINDS).map(|k| [k, 14, 1, 0x40]).collect();
    put("every-op-carryless-pool", &every, &cl);
    // Every kind with 7z-coder decodes, over a valid 7z stream.
    let every7: Vec<[u8; 4]> = (0..ops::OP_KINDS).map(|k| [k, 4, 0, 0x41]).collect();
    put("every-op-7z-pool", &every7, &z7);
    // A solid run: init, decode, decode on, cleanup, decode, verify.
    put(
        "solid-cleanup-verify",
        &[
            [0, 14, 1, 0],
            [3, 0, 0, 0xFE],
            [3, 0, 2, 0xFE],
            [2, 0, 0, 0],
            [3, 7, 0, 0x80],
            [5, 1, 0, 0],
        ],
        &cl,
    );
    // Errors without a model, refused parameters, then a reused arena.
    put(
        "refusals-and-reuse",
        &[
            [3, 0, 0, 0x10],
            [4, 0, 0, 0x10],
            [0, 0, 0, 0x80],
            [0, 0, 1, 0x81],
            [0, 4, 0, 0],
            [5, 1, 0, 0],
            [5, 1, 0, 0],
            [1, 0, 0, 0],
            [5, 3, 0, 0],
        ],
        &rng.bytes(1024),
    );
    // The bare model: starts, refusals, garbage, restart and verify.
    put(
        "bare-model-restarts",
        &[
            [6, 62, 0, 0],
            [8, 0, 0, 0xFF],
            [7, 0, 0, 0],
            [7, 1, 1, 1],
            [7, 2, 2, 2],
            [9, 0, 0, 1],
            [9, 3, 5, 0],
            [6, 0, 9, 0],
            [9, 2, 0, 0],
        ],
        &z7,
    );
    let storm: Vec<[u8; 4]> = (0..ops::MAX_OPS as u8)
        .map(|i| [i % ops::OP_KINDS, i.wrapping_mul(37), i, i.wrapping_mul(11)])
        .collect();
    put("op-storm-garbage", &storm, &rng.bytes(2048));
}

/// The hostile fixtures, built by [`synth::hostile_fixtures`] (which the
/// root crate's hostile suites call in memory), with their `index.txt`.
fn hostile_fixtures(files: &mut Files) {
    let dir = Path::new(HOSTILE_DIR);
    let mut index = String::from(synth::HOSTILE_INDEX_HEADER);
    for f in synth::hostile_fixtures() {
        index.push_str(&f.index_line());
        files.insert(dir.join(format!("{}.stream", f.recipe.name)), f.stream);
        files.insert(dir.join(format!("{}.payload", f.recipe.name)), f.payload);
    }
    files.insert(dir.join("index.txt"), index.into_bytes());
}

/// Every generated file, keyed by its path relative to the repository root.
pub fn all() -> Files {
    let mut files = Files::new();
    decode_7z_seeds(&mut files);
    decode_rar_seeds(&mut files);
    roundtrip_seeds(&mut files);
    structure_seeds(&mut files);
    range_coder_seeds(&mut files);
    checked_vs_unchecked_seeds(&mut files);
    model_ops_seeds(&mut files);
    regressions(&mut files);
    hostile_fixtures(&mut files);
    files
}

/// The directories [`all`] owns: anything else in them is stale.
pub fn owned_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = TARGETS.iter().map(|t| seed_dir(t)).collect();
    for (target, _, _) in REGRESSIONS {
        let dir = Path::new(REGRESSIONS_DIR).join(target);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.push(PathBuf::from(HOSTILE_DIR));
    dirs
}

/// Writes [`all`] under the repository root `root`, removing any other file
/// in the directories this module owns, and returns what it wrote.
pub fn write_all(root: &Path) -> std::io::Result<Files> {
    let files = all();
    for dir in owned_dirs() {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir)?;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if path.is_file() && !files.contains_key(rel) {
                std::fs::remove_file(&path)?;
            }
        }
    }
    for (path, data) in &files {
        std::fs::write(root.join(path), data)?;
    }
    Ok(files)
}

/// The repository root, from this crate's manifest directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the fuzz crate sits inside the repository")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_has_seeds() {
        let files = all();
        for t in TARGETS {
            let n = files.keys().filter(|p| p.starts_with(seed_dir(t))).count();
            assert!(n >= 5, "{t} has {n} seeds");
        }
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(all(), all());
    }

    #[test]
    fn every_file_is_in_an_owned_dir() {
        let dirs = owned_dirs();
        for path in all().keys() {
            assert!(
                dirs.iter().any(|d| path.parent() == Some(d.as_path())),
                "{} is outside the owned directories",
                path.display()
            );
        }
    }

    /// The regressions' bytes are the minimised inputs the first campaign
    /// found; `paths.rs` pins their root cause.
    #[test]
    fn regressions_are_kept() {
        let files = all();
        for (target, name, input) in REGRESSIONS {
            let path = Path::new(REGRESSIONS_DIR).join(target).join(name);
            assert_eq!(files.get(&path).map(Vec::as_slice), Some(*input));
        }
    }

    #[test]
    #[ignore = "writes the generated seeds and fixtures; run by hand"]
    fn regenerate_seeds() {
        write_all(&repo_root()).unwrap();
    }
}
