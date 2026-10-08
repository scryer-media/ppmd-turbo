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
