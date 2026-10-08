//! The 7z encoder against `7zz` itself, out of process.
//!
//! Ignored by default. Run with
//!
//! ```text
//! PPMD_TURBO_ORACLES=1 cargo test --locked --test encode_oracle_7zz -- --ignored
//! ```
//!
//! Without `PPMD_TURBO_ORACLES=1` each test returns at once; with it, a
//! missing `7zz` is a failure (`PPMD_ORACLE_7ZZ` points at a binary). The
//! container code is `tools/ppmd-oracle`'s, included by path.

mod encode_support;

/// The crate's API (`tests/common/api.rs`).
#[path = "common/api.rs"]
mod api;

#[path = "../tools/ppmd-oracle/src/binaries.rs"]
#[allow(dead_code)]
mod binaries;
#[path = "../tools/ppmd-oracle/src/sevenz.rs"]
#[allow(dead_code)]
mod sevenz;

use std::path::{Path, PathBuf};

use api::encode_7z;
use binaries::SevenZip;
use encode_support::{corpus, first_difference};
use sevenz::{read_archive, write_archive};

fn oracle() -> Option<SevenZip> {
    if std::env::var_os("PPMD_TURBO_ORACLES").is_none_or(|v| v != "1") {
        eprintln!("skipped: set PPMD_TURBO_ORACLES=1 to run the binary oracles");
        return None;
    }
    let seven = SevenZip::find().expect("PPMD_TURBO_ORACLES=1 but no 7zz (set PPMD_ORACLE_7ZZ)");
    eprintln!("oracle: {}", seven.version().unwrap_or_default());
    Some(seven)
}

fn work(test: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("encode-oracle-{test}"))
}

/// 7-Zip compresses each corpus payload at the orders and arenas its
/// encoder accepts; the stream inside its archive and ppmd-turbo's stream
/// for the same properties are the same bytes.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn byte_identical_to_7zz() {
    let Some(seven) = oracle() else { return };
    let dir = work("identical");
    let mut checked = 0;
    for (name, data) in corpus::default_corpus() {
        for order in [2u32, 3, 6, 8, 16, 32] {
            for mem in [1u32 << 16, 1 << 20, 16 << 20] {
                let archive = seven
                    .compress_ppmd(&dir, "vexa.bin", &data, order, mem)
                    .unwrap_or_else(|e| panic!("{name} o={order} mem={mem}: {e}"));
                let entry = read_archive(&archive).expect("reads 7-Zip's archive");
                let ours =
                    encode_7z(&data, u32::from(entry.order), entry.mem, false).expect("encodes");
                assert_eq!(
                    first_difference(&ours, &entry.stream),
                    None,
                    "{name} o={} mem={}: {} vs 7zz {} bytes",
                    entry.order,
                    entry.mem,
                    ours.len(),
                    entry.stream.len()
                );
                checked += 1;
            }
        }
    }
    eprintln!("{checked} streams byte-identical to 7zz");
}

/// 7-Zip extracts ppmd-turbo's streams, including orders above 32 and
/// arenas below 64 KiB that its own encoder refuses, and payloads that
/// exhaust a small arena many times.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn sevenzip_extracts_turbo_streams() {
    let Some(seven) = oracle() else { return };
    let dir = work("extract");
    let mut cases = corpus::default_corpus();
    cases.push((
        "exhausting-300000".to_owned(),
        encode_support::exhausting_payload(300_000),
    ));
    for (name, data) in cases {
        for order in [2u8, 7, 33, 64] {
            for mem in [2048u32, 1 << 16, 1 << 20] {
                let stream = encode_7z(&data, u32::from(order), mem, false).expect("encodes");
                let archive =
                    write_archive(order, mem, &stream, &data, "dunmere.bin").expect("writes");
                let back = seven
                    .extract(&dir, &archive)
                    .unwrap_or_else(|e| panic!("{name} o={order} mem={mem}: {e}"));
                assert!(
                    back == data,
                    "{name} o={order} mem={mem}: 7-Zip read other data"
                );
            }
        }
    }
}
