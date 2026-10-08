//! Conformance of the 7z framing against the committed corpus.
//!
//! Every stream in `tests/fixtures/manifest.json` with the 7z coder was
//! written by 7-Zip's `7zz` (pulled out of its `.7z` container) or by
//! ppmd-rust's encoder, with and without the end marker, and checked by the
//! generator: ppmd-rust decodes each back to its payload, and ppmd-rust's
//! encoder reproduces each `7zz` stream byte for byte. Here the crate's
//! decoder must give the payload's SHA-256 for every one, reject the
//! corrupted variants as their recipes say, and never panic.

// Decoding the whole corpus takes Miri hours; the hostile and library tests
// cover the same decoder under Miri, and the fuzz lanes under ASan.
#![cfg(not(miri))]

mod common;

use common::api::decode_7z;
use common::{check_corruption, list, no_panic, payload_sha256, read, report, sha256, u64_of};

fn sevenz_streams(manifest: &serde_json::Value) -> Vec<&serde_json::Value> {
    list(manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "7z")
        .collect()
}

/// The committed bytes are the bytes the manifest describes, and every
/// stream's parameters are ones variant H accepts. Needs no decoder.
#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn manifest_matches_the_committed_streams() {
    let manifest = common::manifest();
    let streams = sevenz_streams(&manifest);
    let mut failures = Vec::new();
    for s in &streams {
        let data = read(s);
        let name = s["name"].as_str().unwrap();
        if data.len() as u64 != u64_of(s, "stream_len") || sha256(&data) != s["stream_sha256"] {
            failures.push(format!("{name}: bytes differ from the manifest"));
        }
        let order = u64_of(s, "order") as u32;
        let mem = u64_of(s, "mem") as u32;
        if !(ppmd_turbo::PPMD7_MIN_ORDER..=ppmd_turbo::PPMD7_MAX_ORDER).contains(&order)
            || !(ppmd_turbo::PPMD7_MIN_MEM_SIZE..=ppmd_turbo::PPMD7_MAX_MEM_SIZE).contains(&mem)
        {
            failures.push(format!("{name}: order {order} / mem {mem} out of range"));
        }
        if s["producer"] == "7zz" && s["ppmd_rust_encoder_identical"] != true {
            failures.push(format!(
                "{name}: ppmd-rust's encoder did not reproduce 7zz's stream"
            ));
        }
    }
    report(&failures, streams.len());
}

#[test]
fn every_stream_decodes_to_its_payload() {
    let manifest = common::manifest();
    let streams = sevenz_streams(&manifest);
    let mut failures = Vec::new();
    for s in &streams {
        let name = s["name"].as_str().unwrap();
        let data = read(s);
        let len = (s["end_marker"] != true).then(|| u64_of(s, "payload_len"));
        let decoded = no_panic(name, || {
            decode_7z(
                &data,
                u64_of(s, "order") as u32,
                u64_of(s, "mem") as u32,
                len,
            )
        });
        match decoded {
            Err(e) => failures.push(e),
            Ok(Err(e)) => failures.push(format!("{name}: {e}")),
            Ok(Ok(out)) if out.len() as u64 != u64_of(s, "payload_len") => {
                failures.push(format!(
                    "{name}: {} bytes, want {}",
                    out.len(),
                    u64_of(s, "payload_len")
                ));
            }
            Ok(Ok(out)) if sha256(&out) != payload_sha256(&manifest, s) => {
                failures.push(format!("{name}: output differs from the payload"));
            }
            Ok(Ok(_)) => {}
        }
    }
    report(&failures, streams.len());
}

/// An end-marker stream read with its known length gives the same bytes:
/// the marker after the last symbol is never reached.
#[test]
fn end_marker_streams_also_decode_by_length() {
    let manifest = common::manifest();
    let streams: Vec<_> = sevenz_streams(&manifest)
        .into_iter()
        .filter(|s| s["end_marker"] == true)
        .collect();
    let mut failures = Vec::new();
    for s in &streams {
        let name = s["name"].as_str().unwrap();
        let data = read(s);
        let len = Some(u64_of(s, "payload_len"));
        match decode_7z(
            &data,
            u64_of(s, "order") as u32,
            u64_of(s, "mem") as u32,
            len,
        ) {
            Ok(out) if sha256(&out) == payload_sha256(&manifest, s) => {}
            Ok(_) => failures.push(format!("{name}: output differs from the payload")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    report(&failures, streams.len());
}

#[test]
fn corrupted_streams_fail_as_their_recipes_say() {
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for c in list(&manifest, "corruptions") {
        let base = common::stream(&manifest, &c["base"]);
        if base["coder"] != "7z" {
            continue;
        }
        checked += 1;
        let name = c["name"].as_str().unwrap();
        let bad = common::corrupt(c["op"].as_str().unwrap(), &read(base));
        let len = Some(u64_of(base, "payload_len"));
        match no_panic(name, || {
            decode_7z(
                &bad,
                u64_of(base, "order") as u32,
                u64_of(base, "mem") as u32,
                len,
            )
        }) {
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

/// Every prefix of a small stream: an error or, if the missing tail was
/// never needed, the payload; never invented bytes and never a panic.
#[test]
fn every_truncation_of_a_small_stream_is_clean() {
    let manifest = common::manifest();
    let s = common::stream(&manifest, &"text-1k.o6.m64k.7zz.ppmd".into());
    let data = read(s);
    let want = payload_sha256(&manifest, s);
    let mut failures = Vec::new();
    for cut in 0..data.len() {
        let name = format!("prefix {cut}");
        let len = Some(u64_of(s, "payload_len"));
        match no_panic(&name, || decode_7z(&data[..cut], 6, 1 << 16, len)) {
            Err(e) => failures.push(e),
            Ok(Ok(out)) if sha256(&out) != want => {
                failures.push(format!("{name}: decoded wrong bytes without an error"))
            }
            Ok(_) => {}
        }
    }
    report(&failures, data.len());
}

#[test]
fn out_of_range_parameters_are_rejected() {
    let manifest = common::manifest();
    let s = common::stream(&manifest, &"text-1k.o6.m64k.7zz.ppmd".into());
    let data = read(s);
    let len = Some(u64_of(s, "payload_len"));
    let min_mem = ppmd_turbo::PPMD7_MIN_MEM_SIZE;
    let max_mem = ppmd_turbo::PPMD7_MAX_MEM_SIZE;
    for (order, mem) in [
        (0, 1 << 16),
        (1, 1 << 16),
        (65, 1 << 16),
        (255, 1 << 16),
        (6, 0),
        (6, min_mem - 1),
        (6, max_mem + 1),
        (6, u32::MAX),
    ] {
        let result = no_panic("parameters", || decode_7z(&data, order, mem, len)).unwrap();
        assert!(result.is_err(), "order {order} mem {mem} was accepted");
    }
}
