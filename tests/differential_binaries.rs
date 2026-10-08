//! Out-of-process differential checks against `7zz` (and `unrar` when it is
//! on `PATH`).
//!
//! Every test is ignored. Run them with
//!
//! ```text
//! PPMD_TURBO_ORACLES=1 cargo test --locked --test differential_binaries -- --ignored
//! ```
//!
//! Without `PPMD_TURBO_ORACLES=1` each test returns at once, so a broad
//! `--ignored` run on a machine without the binaries stays green. With it, a
//! missing `7zz` is a failure. `unrar` is optional: RAR checks skip without
//! it. Set `PPMD_ORACLE_7ZZ` or `PPMD_ORACLE_UNRAR` to point at a binary.
//!
//! The container code is `tools/ppmd-oracle`'s, included by path so this
//! crate takes no dependency on the tool.

#[path = "../tools/ppmd-oracle/src/binaries.rs"]
#[allow(dead_code)]
mod binaries;
#[path = "../tools/ppmd-oracle/src/corpus.rs"]
#[allow(dead_code)]
mod corpus;
mod hostile_support;
#[path = "../tools/ppmd-oracle/src/sevenz.rs"]
#[allow(dead_code)]
mod sevenz;

use std::path::{Path, PathBuf};

use binaries::{SevenZip, Unrar};
use hostile_support::{Coder, fixtures_of};
use sevenz::{read_archive, write_archive};

/// ppmd-turbo's 7z encoder, no end marker (as 7-Zip writes).
fn turbo_encode_7z(data: &[u8], order: u32, mem: u32) -> Vec<u8> {
    hostile_support::api::encode_7z(data, order, mem, false).expect("encodes")
}

fn turbo_decode_7z(stream: &[u8], order: u32, mem: u32, size: u64) -> Result<Vec<u8>, String> {
    hostile_support::api::decode_7z(stream, order, mem, Some(size)).map_err(|e| e.to_string())
}

/// `Some(7zz)` when the oracles are enabled; `None` (skip) otherwise.
fn oracle() -> Option<SevenZip> {
    if std::env::var_os("PPMD_TURBO_ORACLES").is_none_or(|v| v != "1") {
        eprintln!("skipped: set PPMD_TURBO_ORACLES=1 to run the binary oracles");
        return None;
    }
    let seven = SevenZip::find().expect("PPMD_TURBO_ORACLES=1 but no 7zz (set PPMD_ORACLE_7ZZ)");
    Some(seven)
}

fn work(test: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("oracle-{test}"))
}

/// The parameter grid 7-Zip's encoder accepts (orders 2..=32, at least
/// 64 KiB), crossed with the invented corpus.
fn encodable_cases() -> Vec<(String, Vec<u8>, u32, u32)> {
    let mut out = Vec::new();
    for (name, data) in corpus::default_corpus() {
        for order in [2, 6, 16, 32] {
            for mem in [1 << 16, 1 << 20, 16 << 20] {
                out.push((name.clone(), data.clone(), order, mem));
            }
        }
    }
    out
}

/// 7-Zip writes a PPMd archive; the tool reads it, rewraps 7-Zip's own
/// stream with its writer, and 7-Zip extracts that. Validates the container
/// code with no ppmd-turbo involved.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn container_round_trips_7zz_streams() {
    let Some(seven) = oracle() else { return };
    let dir = work("container");
    for (name, data, order, mem) in encodable_cases() {
        let archive = seven
            .compress_ppmd(&dir, "talsen.bin", &data, order, mem)
            .unwrap_or_else(|e| panic!("{name} o={order} mem={mem}: {e}"));
        let entry = read_archive(&archive).expect("reads 7-Zip's archive");
        assert_eq!(u32::from(entry.order), order, "{name}");
        assert!(entry.mem <= mem, "{name}: 7-Zip only lowers the arena");
        assert_eq!(entry.unpack_size, data.len() as u64, "{name}");
        assert_eq!(entry.crc, Some(crc32fast::hash(&data)), "{name}");
        let rewrapped = write_archive(entry.order, entry.mem, &entry.stream, &data, "kith.bin")
            .expect("writes");
        let back = seven
            .extract(&dir, &rewrapped)
            .expect("7-Zip extracts ours");
        assert!(
            back == data,
            "{name} o={order} mem={mem}: rewrapped stream differs"
        );
    }
}

/// ppmd-turbo's 7z encoder writes the same bytes as 7-Zip.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn turbo_encoder_is_byte_identical_to_7zz() {
    let Some(seven) = oracle() else { return };
    let dir = work("encoder");
    for (name, data, order, mem) in encodable_cases() {
        let entry = read_archive(
            &seven
                .compress_ppmd(&dir, "vexa.bin", &data, order, mem)
                .expect("7-Zip compresses"),
        )
        .expect("reads");
        let ours = turbo_encode_7z(&data, u32::from(entry.order), entry.mem);
        if let Some(at) = ours.iter().zip(&entry.stream).position(|(a, b)| a != b) {
            panic!(
                "{name} o={} mem={}: first difference at byte {at} of {}/{}",
                entry.order,
                entry.mem,
                ours.len(),
                entry.stream.len()
            );
        }
        assert_eq!(ours.len(), entry.stream.len(), "{name}: stream length");
    }
}

/// ppmd-turbo decodes every stream 7-Zip writes.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn turbo_decoder_reads_7zz_streams() {
    let Some(seven) = oracle() else { return };
    let dir = work("decoder");
    for (name, data, order, mem) in encodable_cases() {
        let entry = read_archive(
            &seven
                .compress_ppmd(&dir, "osk.bin", &data, order, mem)
                .expect("7-Zip compresses"),
        )
        .expect("reads");
        let out = turbo_decode_7z(
            &entry.stream,
            u32::from(entry.order),
            entry.mem,
            entry.unpack_size,
        )
        .unwrap_or_else(|e| panic!("{name} o={order} mem={mem}: {e}"));
        assert!(out == data, "{name} o={order} mem={mem}: output differs");
    }
}

/// 7-Zip extracts ppmd-turbo's streams at every order, including the
/// orders above 32 and the arenas below 64 KiB its own encoder refuses.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn sevenzip_reads_turbo_streams_at_every_order() {
    let Some(seven) = oracle() else { return };
    let dir = work("reverse");
    for (name, data) in corpus::default_corpus() {
        for order in [2u8, 3, 7, 16, 33, 48, 64] {
            for mem in [2048u32, 1 << 16, 1 << 20] {
                let stream = turbo_encode_7z(&data, u32::from(order), mem);
                let archive =
                    write_archive(order, mem, &stream, &data, "dunmere.bin").expect("writes");
                let back = seven
                    .extract(&dir, &archive)
                    .unwrap_or_else(|e| panic!("{name} o={order} mem={mem}: {e}"));
                assert!(
                    back == data,
                    "{name} o={order} mem={mem}: 7-Zip read back other data"
                );
            }
        }
    }
}

/// Damaged streams: whenever 7-Zip extracts one cleanly (its CRC matches),
/// ppmd-turbo decodes it to the same bytes. Truncations and the first 32
/// byte flips of each 7z hostile fixture.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn damaged_streams_7zz_accepts_turbo_accepts() {
    let Some(seven) = oracle() else { return };
    let dir = work("damaged");
    for fx in fixtures_of(Coder::SevenZ) {
        if fx.payload.is_empty() {
            continue;
        }
        let order = u8::try_from(fx.order).expect("order fits a byte");
        let mut variants = Vec::new();
        for i in 0..fx.stream.len().min(32) {
            let mut s = fx.stream.clone();
            s[i] ^= 0x01 << (i % 8);
            variants.push((format!("flip {i}"), s));
        }
        for keep in [
            0,
            1,
            4,
            5,
            fx.stream.len() / 2,
            fx.stream.len().saturating_sub(1),
        ] {
            variants.push((format!("keep {keep}"), fx.stream[..keep].to_vec()));
        }
        for (what, stream) in variants {
            let archive =
                write_archive(order, fx.mem, &stream, &fx.payload, "ivet.bin").expect("writes");
            let Ok(back) = seven.extract(&dir, &archive) else {
                continue;
            };
            let ours = turbo_decode_7z(&stream, fx.order, fx.mem, fx.payload.len() as u64);
            assert!(
                ours.as_ref() == Ok(&back),
                "{} {what}: 7-Zip extracted {} bytes, ppmd-turbo gave {:?}",
                fx.name,
                back.len(),
                ours.map(|v| v.len())
            );
        }
    }
}

/// `unrar` tests every RAR fixture the conformance suite ships. Skips when
/// `unrar` is not on `PATH`. When the RAR API lands, compare `unrar p` with
/// ppmd-turbo's extraction of each archive here.
#[test]
#[ignore = "binary oracle: PPMD_TURBO_ORACLES=1, --ignored"]
fn unrar_accepts_the_rar_fixtures() {
    if oracle().is_none() {
        return;
    }
    let Some(unrar) = Unrar::find() else {
        eprintln!("skipped: unrar is not on PATH");
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let mut stack = vec![root];
    let mut archives = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("rar")) {
                archives.push(p);
            }
        }
    }
    archives.sort();
    for a in &archives {
        unrar
            .test(a)
            .unwrap_or_else(|e| panic!("{}: {e}", a.display()));
    }
    eprintln!("unrar tested {} archives", archives.len());
}
