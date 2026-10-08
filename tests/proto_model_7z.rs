//! Prototype gate for model layout experiments (backlog D11): drives
//! [`ppmd_turbo::model::Model`] directly through
//! [`ppmd_turbo::rc::SevenZipRangeDecoder`] over every 7z stream in the
//! conformance corpus and checks the payload's SHA-256. The 7z framing is not
//! wired yet; this is the narrowest loop that proves a model change stays
//! bit-exact on the 7z corpus.

mod common;

use common::{list, payload_sha256, read, report, sha256, u64_of};
use ppmd_turbo::model::Model;
use ppmd_turbo::rc::SevenZipRangeDecoder;

fn decode(stream: &[u8], order: u32, mem: u32, len: Option<u64>) -> Result<Vec<u8>, String> {
    let mut model = Model::new(order, mem).map_err(|e| e.to_string())?;
    let mut rc = SevenZipRangeDecoder::new(stream).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        if len.is_some_and(|n| out.len() as u64 == n) {
            break;
        }
        match model.decode_symbol(&mut rc).map_err(|e| e.to_string())? {
            Some(b) => out.push(b),
            None => break,
        }
    }
    Ok(out)
}

#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn model_decodes_every_7z_stream_to_its_payload() {
    let manifest = common::manifest();
    let streams: Vec<_> = list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "7z")
        .collect();
    let mut failures = Vec::new();
    for s in &streams {
        let name = s["name"].as_str().unwrap();
        let len = (s["end_marker"] != true).then(|| u64_of(s, "payload_len"));
        match decode(
            &read(s),
            u64_of(s, "order") as u32,
            u64_of(s, "mem") as u32,
            len,
        ) {
            Ok(out) if sha256(&out) == payload_sha256(&manifest, s) => {}
            Ok(out) => failures.push(format!("{name}: {} bytes, wrong digest", out.len())),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    report(&failures, streams.len());
}

/// The RAR side of the gate, through the crate's own `rar::RarDecoder` (the
/// conformance suite's shim is still a placeholder): every raw carry-less
/// stream decodes to its payload, and the RARLAB member to its symbols and,
/// through the RAR3 escape layer, its bytes.
#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn rar_decoder_decodes_every_carry_less_stream_and_member() {
    use ppmd_turbo::rar::RarDecoder;
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for s in list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "carry-less")
    {
        checked += 1;
        let name = s["name"].as_str().unwrap();
        let mut out = Vec::new();
        let result = RarDecoder::new().decode_block(
            true,
            u64_of(s, "order") as u32,
            (u64_of(s, "mem") >> 20) as u32,
            &read(s),
            u64_of(s, "payload_len"),
            &mut out,
        );
        match result {
            Ok(_) if sha256(&out) == payload_sha256(&manifest, s) => {}
            Ok(_) => failures.push(format!("{name}: output differs")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    for m in list(&manifest, "rar_members") {
        checked += 1;
        let name = m["name"].as_str().unwrap();
        let packed = read(m);
        let h = &m["header"];
        let mut symbols = Vec::new();
        let result = RarDecoder::new().decode_block(
            h["reset"] == true,
            u64_of(h, "order") as u32,
            u64_of(h, "mem_mb") as u32,
            &packed[u64_of(h, "len") as usize..],
            u64_of(m, "symbols"),
            &mut symbols,
        );
        if let Err(e) = result {
            failures.push(format!("{name}: {e}"));
            continue;
        }
        if sha256(&symbols) != m["symbols_sha256"] {
            failures.push(format!("{name}: symbols differ"));
            continue;
        }
        let esc = h["esc"].as_u64().map_or(2, |e| e as u8);
        match common::rar3_unescape(&symbols, esc, u64_of(m, "unpacked_len") as usize) {
            Ok(bytes) if sha256(&bytes) == m["payload_sha256"] => {}
            Ok(_) => failures.push(format!("{name}: member bytes differ")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    report(&failures, checked);
}
