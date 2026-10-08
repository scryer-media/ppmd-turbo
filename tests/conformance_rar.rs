//! Conformance of the RAR framing against the committed corpus.
//!
//! Three kinds of input, all from `tests/fixtures/manifest.json`:
//!
//! - raw carry-less streams written by ppmd-rust's `Ppmd7aEncoder`: PPMd
//!   variant H over the range coder RAR's PPMd blocks use, at orders and
//!   memory sizes RAR can express;
//! - a member of a RARLAB-written archive (unrar-rs's `rar4_ppm_oldmv` set,
//!   split over four volumes), whose packed data is one PPMd block: the
//!   symbols must match the generator's ppmd-rust decode and, through the
//!   RAR3 escape layer, the member's bytes;
//! - libarchive's hostile PPMd archives, which must only return without a
//!   panic.

mod common;

use common::api::RarDecoder;
use common::{check_corruption, list, no_panic, payload_sha256, read, report, sha256, u64_of};

const MIB: u64 = 1 << 20;

#[test]
fn rar3_unescape_codes() {
    let symbols = [b'a', b'b', 2, 1, 2, 5, 0, 2, 4, 0, 0, 0, 0, 2, 2, b'z'];
    let out = common::rar3_unescape(&symbols, 2, 1000).unwrap();
    assert_eq!(&out[..7], &[b'a', b'b', 2, 2, 2, 2, 2]);
    assert_eq!(out.len(), 7 + 32);
    assert!(common::rar3_unescape(&[2, 0], 2, 10).is_err());
}

#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn manifest_matches_the_committed_files() {
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for s in list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "carry-less")
    {
        checked += 1;
        let data = read(s);
        let name = s["name"].as_str().unwrap();
        if data.len() as u64 != u64_of(s, "stream_len") || sha256(&data) != s["stream_sha256"] {
            failures.push(format!("{name}: bytes differ from the manifest"));
        }
        if !u64_of(s, "mem").is_multiple_of(MIB) {
            failures.push(format!("{name}: RAR sizes memory in whole MiB"));
        }
    }
    for m in list(&manifest, "rar_members") {
        checked += 1;
        let data = read(m);
        if sha256(&data) != m["packed_sha256"] {
            failures.push(format!(
                "{}: packed bytes differ from the manifest",
                m["name"]
            ));
        }
        // Order code 0x0F + 1 = 16 with reset and escape byte: what rar -mc16 wrote.
        if data.first().copied() != m["header"]["flags"].as_u64().map(|f| f as u8) {
            failures.push(format!("{}: header flags differ", m["name"]));
        }
    }
    for h in list(&manifest, "hostile") {
        checked += 1;
        if read(h).len() as u64 != u64_of(h, "packed_len") {
            failures.push(format!("{}: length differs", h["name"]));
        }
    }
    report(&failures, checked);
}

#[test]
#[ignore = "awaiting decoder"]
fn raw_carry_less_streams_decode_to_their_payloads() {
    let manifest = common::manifest();
    let streams: Vec<_> = list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "carry-less")
        .collect();
    let mut failures = Vec::new();
    for s in &streams {
        let name = s["name"].as_str().unwrap();
        let data = read(s);
        let len = u64_of(s, "payload_len");
        let mut out = Vec::new();
        let result = no_panic(name, || {
            RarDecoder::new().decode_block(
                true,
                u64_of(s, "order") as usize,
                (u64_of(s, "mem") / MIB) as usize,
                &data,
                len,
                &mut out,
            )
        });
        match result {
            Err(e) => failures.push(e),
            Ok(Err(e)) => failures.push(format!("{name}: {e}")),
            Ok(Ok(consumed)) if consumed > data.len() => failures.push(format!(
                "{name}: consumed {consumed} of {} bytes",
                data.len()
            )),
            Ok(Ok(_)) if sha256(&out) != payload_sha256(&manifest, s) => {
                failures.push(format!(
                    "{name}: {} symbols, output differs from the payload",
                    out.len()
                ));
            }
            Ok(Ok(_)) => {}
        }
    }
    report(&failures, streams.len());
}

#[test]
#[ignore = "awaiting decoder"]
fn rarlab_members_decode_to_their_bytes() {
    let manifest = common::manifest();
    let members = list(&manifest, "rar_members");
    let mut failures = Vec::new();
    for m in members {
        let name = m["name"].as_str().unwrap();
        let packed = read(m);
        let h = &m["header"];
        let rc = &packed[u64_of(h, "len") as usize..];
        let mut symbols = Vec::new();
        let result = no_panic(name, || {
            RarDecoder::new().decode_block(
                h["reset"] == true,
                u64_of(h, "order") as usize,
                u64_of(h, "mem_mb") as usize,
                rc,
                u64_of(m, "symbols"),
                &mut symbols,
            )
        });
        match result {
            Err(e) => failures.push(e),
            Ok(Err(e)) => failures.push(format!("{name}: {e}")),
            Ok(Ok(_)) if sha256(&symbols) != m["symbols_sha256"] => {
                failures.push(format!("{name}: symbols differ"))
            }
            Ok(Ok(_)) => {
                let esc = h["esc"].as_u64().map_or(2, |e| e as u8);
                match common::rar3_unescape(&symbols, esc, u64_of(m, "unpacked_len") as usize) {
                    Ok(bytes) if sha256(&bytes) == m["payload_sha256"] => {}
                    Ok(_) => failures.push(format!("{name}: member bytes differ")),
                    Err(e) => failures.push(format!("{name}: {e}")),
                }
            }
        }
    }
    report(&failures, members.len());
}

#[test]
#[ignore = "awaiting decoder"]
fn hostile_archives_return_without_panicking() {
    let manifest = common::manifest();
    let hostile = list(&manifest, "hostile");
    let mut failures = Vec::new();
    for h in hostile {
        let name = h["name"].as_str().unwrap();
        let packed = read(h);
        let header = &h["header"];
        let (skip, order, mem_mb) = if header.is_object() {
            (
                u64_of(header, "len") as usize,
                u64_of(header, "order") as usize,
                u64_of(header, "mem_mb") as usize,
            )
        } else {
            (0, 6, 1)
        };
        let limit = u64_of(h, "claimed_unpacked_len").min(MIB);
        let mut out = Vec::new();
        if let Err(e) = no_panic(name, || {
            RarDecoder::new().decode_block(true, order, mem_mb, &packed[skip..], limit, &mut out)
        }) {
            failures.push(e);
        }
    }
    report(&failures, hostile.len());
}

#[test]
#[ignore = "awaiting decoder"]
fn corrupted_streams_fail_as_their_recipes_say() {
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for c in list(&manifest, "corruptions") {
        let base = common::stream(&manifest, &c["base"]);
        if base["coder"] != "carry-less" {
            continue;
        }
        checked += 1;
        let name = c["name"].as_str().unwrap();
        let bad = common::corrupt(c["op"].as_str().unwrap(), &read(base));
        let result = no_panic(name, || {
            let mut out = Vec::new();
            RarDecoder::new()
                .decode_block(
                    true,
                    u64_of(base, "order") as usize,
                    (u64_of(base, "mem") / MIB) as usize,
                    &bad,
                    u64_of(base, "payload_len"),
                    &mut out,
                )
                .map(|_| out)
        });
        match result {
            Err(e) => failures.push(e),
            Ok(result) => {
                if let Err(e) = check_corruption(c, &payload_sha256(&manifest, base), &result) {
                    failures.push(e);
                }
            }
        }
    }
    report(&failures, checked);
}

/// The RAR member truncated anywhere: an error, never a panic.
#[test]
#[ignore = "awaiting decoder"]
fn truncated_member_is_an_error() {
    let manifest = common::manifest();
    let m = &list(&manifest, "rar_members")[0];
    let packed = read(m);
    let h = &m["header"];
    let rc = &packed[u64_of(h, "len") as usize..];
    let mut failures = Vec::new();
    let cuts = [0, 1, 3, 4, rc.len() / 4, rc.len() / 2, rc.len() * 3 / 4];
    for cut in cuts {
        let name = format!("cut at {cut}");
        let mut out = Vec::new();
        match no_panic(&name, || {
            RarDecoder::new().decode_block(
                true,
                16,
                u64_of(h, "mem_mb") as usize,
                &rc[..cut],
                u64_of(m, "symbols"),
                &mut out,
            )
        }) {
            Err(e) => failures.push(e),
            Ok(Ok(_)) => failures.push(format!(
                "{name}: decoded {} symbols without an error",
                out.len()
            )),
            Ok(Err(_)) => {}
        }
    }
    report(&failures, cuts.len());
}

/// A block that does not reset, with no model yet, is an error (as in
/// unrar-rs's `test_ppmd_block_without_init`).
#[test]
#[ignore = "awaiting decoder"]
fn continuation_without_a_model_is_an_error() {
    let mut out = Vec::new();
    let result = RarDecoder::new().decode_block(false, 0, 0, &[0; 8], 10, &mut out);
    assert!(result.is_err());
}
