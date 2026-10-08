//! Shared by the conformance suites: the fixture manifest, digests, the
//! corruption recipes and the RAR3 escape layer.
//!
//! The manifest is committed; the streams it names are not. [`read`] takes a
//! stream from `tests/fixtures/` when `cargo run --locked --release -p
//! ppmd-corpus -- fixtures` has written it, and otherwise encodes ppmd-rust
//! rows in process through the same `ppmd_corpus::fixtures` code. `7zz` and
//! RAR rows need that command (7-Zip's `7zz`, and rarpar's published
//! corpus); without it they are skipped with a note, or fail when
//! `PPMD_TURBO_REQUIRE_FIXTURES` is set, as it is in CI.

#![allow(dead_code)]

pub mod api;

use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// `tests/fixtures`.
pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// The parsed `tests/fixtures/manifest.json`.
pub fn manifest() -> Value {
    let path = fixtures().join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let manifest: Value = serde_json::from_str(&text).expect("manifest.json parses");
    assert_eq!(manifest["schema"], "ppmd-turbo-corpus/conformance/1");
    manifest
}

/// The command that writes every generated fixture.
pub const REGENERATE: &str = "cargo run --locked --release -p ppmd-corpus -- fixtures";

/// Whether a missing fixture fails the suite instead of being skipped.
pub fn required() -> bool {
    std::env::var_os(ppmd_corpus::fixtures::REQUIRE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// A fixture's bytes: the file when it is on disk; a ppmd-rust stream
/// encoded in process when it is not; `None` (with a note on stderr) for a
/// missing `7zz` or RAR row, unless [`required`].
pub fn read(entry: &Value) -> Option<Vec<u8>> {
    let file = entry["file"].as_str().expect("file");
    let path = fixtures().join(file);
    match std::fs::read(&path) {
        Ok(data) => return Some(data),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            panic!("{}: {e}", path.display())
        }
        Err(_) => {}
    }
    if entry["producer"] == "ppmd-rust" {
        return Some(
            ppmd_corpus::fixtures::ppmd_rust_stream(entry).unwrap_or_else(|e| panic!("{e}")),
        );
    }
    let needs = if entry["producer"] == "7zz" {
        "7-Zip's 7zz"
    } else {
        "the RAR source archives"
    };
    let why = format!("{file} is not generated (it needs {needs}); run `{REGENERATE}`");
    assert!(!required(), "{why}");
    eprintln!("skipped: {why}");
    None
}

/// Lower-case hex SHA-256.
pub fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An array field of the manifest.
pub fn list<'a>(manifest: &'a Value, key: &str) -> &'a [Value] {
    manifest[key].as_array().map_or(&[], Vec::as_slice)
}

/// An unsigned field, as `u64`.
pub fn u64_of(entry: &Value, key: &str) -> u64 {
    entry[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} in {entry}"))
}

/// The SHA-256 of the payload a stream decodes to.
pub fn payload_sha256(manifest: &Value, stream: &Value) -> String {
    list(manifest, "payloads")
        .iter()
        .find(|p| p["name"] == stream["payload"])
        .and_then(|p| p["sha256"].as_str())
        .unwrap_or_else(|| panic!("no payload for {}", stream["name"]))
        .to_string()
}

/// The stream a corruption recipe starts from.
pub fn stream<'a>(manifest: &'a Value, name: &Value) -> &'a Value {
    list(manifest, "streams")
        .iter()
        .find(|s| s["name"] == *name)
        .unwrap_or_else(|| panic!("no stream {name}"))
}

/// The corruption recipes, as `tools/ppmd-corpus` defines them.
pub fn corrupt(op: &str, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    match op {
        "truncate-half" => v.truncate(data.len() / 2),
        "flip-first" => v[0] ^= 0x80,
        "flip-third" => v[data.len() / 3] ^= 0x55,
        "flip-mid" => v[data.len() / 2] ^= 0xFF,
        other => panic!("unknown corruption {other}"),
    }
    v
}

/// Checks a decode of a corrupted stream against its recipe's expectation:
/// `error` must be an `Err`; `not-payload` may be an `Err` or any output but
/// the original payload. When ppmd-rust also decoded the corrupted stream
/// without an error, the two outputs must agree, as 7-Zip's would.
pub fn check_corruption(
    entry: &Value,
    payload_sha: &str,
    result: &ppmd_turbo::Result<Vec<u8>>,
) -> Result<(), String> {
    let name = entry["name"].as_str().unwrap_or("?");
    match (entry["expect"].as_str(), result) {
        (Some("error"), Ok(out)) => Err(format!(
            "{name}: decoded {} bytes, want an error",
            out.len()
        )),
        (Some("not-payload"), Ok(out)) if sha256(out) == payload_sha => Err(format!(
            "{name}: decoded the original payload from a corrupted stream"
        )),
        (_, Ok(out))
            if entry["ppmd_rust"]["ok"] == true
                && entry["ppmd_rust"]["sha256"] != sha256(out).as_str() =>
        {
            Err(format!(
                "{name}: output differs from ppmd-rust's on the same corrupted stream"
            ))
        }
        _ => Ok(()),
    }
}

/// Runs `f`, turning a panic into an `Err` that names the fixture, so one
/// bad fixture reports instead of hiding the rest.
pub fn no_panic<T>(name: &str, f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| format!("{name}: panicked"))
}

/// Fails with every collected problem at once. Nothing checked is a
/// failure only when every fixture is [`required`]: otherwise the missing
/// ones were skipped by [`read`].
pub fn report(failures: &[String], checked: usize) {
    assert!(checked > 0 || !required(), "no fixtures were checked");
    assert!(
        failures.is_empty(),
        "{} of {checked} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The RAR3 escape layer over a PPMd symbol stream: the escape byte then 1
/// is a literal escape byte, 2 ends the data, 4 is a match (three distance
/// bytes, one length byte: length + 32 at distance + 2), 5 a run (one length
/// byte: length + 4 at distance 1). 0 (switch to LZ) and 3 (RarVM filter)
/// need machinery outside PPMd and are refused. RARLAB's `unpack30.cpp` is
/// the reference for the codes; this is the same layer `tools/ppmd-corpus`
/// uses to build the fixture.
pub fn rar3_unescape(symbols: &[u8], esc: u8, limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(limit);
    let mut i = 0;
    let next = |i: &mut usize| -> Result<u8, String> {
        let s = *symbols.get(*i).ok_or("symbols ran out inside an escape")?;
        *i += 1;
        Ok(s)
    };
    while out.len() < limit && i < symbols.len() {
        let s = next(&mut i)?;
        if s != esc {
            out.push(s);
            continue;
        }
        let (length, distance) = match next(&mut i)? {
            0 => return Err("escape 0: LZ block switch".into()),
            2 => break,
            3 => return Err("escape 3: RarVM filter".into()),
            4 => {
                let d = (u32::from(next(&mut i)?) << 16)
                    | (u32::from(next(&mut i)?) << 8)
                    | u32::from(next(&mut i)?);
                (u32::from(next(&mut i)?) + 32, d as usize + 2)
            }
            5 => (u32::from(next(&mut i)?) + 4, 1),
            _ => {
                out.push(esc);
                continue;
            }
        };
        if distance > out.len() {
            return Err("match before the start of the output".into());
        }
        for _ in 0..length {
            out.push(out[out.len() - distance]);
        }
    }
    out.truncate(limit);
    Ok(out)
}
