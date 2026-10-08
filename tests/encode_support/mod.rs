//! Shared by the encoder suites: payloads, the fixture payloads recovered
//! through ppmd-rust, and decoders built from the crate's model and coders
//! (independent of any framing).

#![allow(dead_code)]

use std::io::{Read, Write};

use ppmd_turbo::model::Model;
use ppmd_turbo::rc::{CarrylessRangeDecoder, SevenZipRangeDecoder};

/// The oracle tool's invented corpus (`text`, `ramp`, `records`, `random`),
/// included by path so the tests take no dependency on the tool.
#[path = "../../tools/ppmd-oracle/src/corpus.rs"]
pub mod corpus;

/// Payloads for the parameter grids: every corpus shape at a few lengths,
/// plus the empty payload and a long run of one byte.
pub fn payloads() -> Vec<(String, Vec<u8>)> {
    let mut out = vec![("empty".to_owned(), Vec::new())];
    for (i, len) in [1usize, 2, 300, 20_000].into_iter().enumerate() {
        let seed = 0x5EED_0000 + i as u64;
        out.push((format!("text-{len}"), corpus::text(seed, len)));
        out.push((format!("ramp-{len}"), corpus::ramp(seed, len)));
        out.push((format!("records-{len}"), corpus::records(seed, len)));
        out.push((format!("random-{len}"), corpus::random(seed, len)));
    }
    out.push(("run-5000".to_owned(), vec![0x61; 5000]));
    out
}

/// A payload that fills a small arena many times over: high-order text
/// mixed with random bytes.
pub fn exhausting_payload(len: usize) -> Vec<u8> {
    let text = corpus::text(0xA11C, len / 2);
    let noise = corpus::random(0xB0B, len / 2);
    text.chunks(997)
        .zip(noise.chunks(1009))
        .flat_map(|(a, b)| a.iter().chain(b).copied())
        .collect()
}

/// ppmd-rust's 7z encoder (the reference port of 7-Zip's `Ppmd7Enc.c`).
pub fn reference_7z(data: &[u8], order: u32, mem: u32, end_marker: bool) -> Vec<u8> {
    let mut enc = ppmd_rust::Ppmd7Encoder::new(Vec::new(), order, mem).expect("legal parameters");
    enc.write_all(data).expect("into a Vec");
    enc.finish(end_marker).expect("into a Vec")
}

/// ppmd-rust's carry-less (`7a`) encoder.
pub fn reference_carryless(data: &[u8], order: u32, mem: u32, end_marker: bool) -> Vec<u8> {
    let mut enc = ppmd_rust::Ppmd7aEncoder::new(Vec::new(), order, mem).expect("legal parameters");
    enc.write_all(data).expect("into a Vec");
    enc.finish(end_marker).expect("into a Vec")
}

/// ppmd-rust's 7z decoder: `len` bytes, or to the end marker.
pub fn reference_decode_7z(stream: &[u8], order: u32, mem: u32, len: Option<usize>) -> Vec<u8> {
    let mut dec = ppmd_rust::Ppmd7Decoder::new(stream, order, mem).expect("legal parameters");
    read_reference(&mut dec, len)
}

/// ppmd-rust's carry-less decoder: `len` bytes, or to the end marker.
pub fn reference_decode_carryless(
    stream: &[u8],
    order: u32,
    mem: u32,
    len: Option<usize>,
) -> Vec<u8> {
    let mut dec = ppmd_rust::Ppmd7aDecoder::new(stream, order, mem).expect("legal parameters");
    read_reference(&mut dec, len)
}

fn read_reference(dec: &mut impl Read, len: Option<usize>) -> Vec<u8> {
    let mut out = Vec::new();
    match len {
        Some(n) => {
            out.resize(n, 0);
            dec.read_exact(&mut out).expect("reference decodes");
        }
        None => {
            dec.read_to_end(&mut out).expect("reference decodes");
        }
    }
    out
}

/// What a decode through the crate's model produced.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The decoded bytes.
    pub data: Vec<u8>,
    /// Whether decoding stopped at the end marker.
    pub end_marker: bool,
}

/// Decodes a 7z-coder stream with `Model` over `SevenZipRangeDecoder`:
/// `len` symbols, then one more symbol if `expect_marker` (which must be
/// the end marker).
pub fn turbo_decode_7z(
    stream: &[u8],
    order: u32,
    mem: u32,
    len: usize,
    expect_marker: bool,
) -> Decoded {
    let mut model = Model::new(order, mem).expect("legal parameters");
    let mut rc = SevenZipRangeDecoder::new(stream).expect("stream initializes");
    let decoded = drive(&mut model, len, expect_marker, |m| m.decode_symbol(&mut rc));
    assert!(rc.is_finished_ok(), "7z decoder did not finish with code 0");
    assert_eq!(rc.zero_bytes_past_eof(), 0, "decoder read past the stream");
    assert_eq!(
        rc.position(),
        stream.len(),
        "decoder left stream bytes unread"
    );
    decoded
}

/// Decodes a carry-less stream with `Model` over `CarrylessRangeDecoder`.
pub fn turbo_decode_carryless(
    stream: &[u8],
    order: u32,
    mem: u32,
    len: usize,
    expect_marker: bool,
) -> Decoded {
    let mut model = Model::new(order, mem).expect("legal parameters");
    let mut rc = CarrylessRangeDecoder::new(stream).expect("stream initializes");
    let decoded = drive(&mut model, len, expect_marker, |m| m.decode_symbol(&mut rc));
    assert_eq!(rc.zero_bytes_past_eof(), 0, "decoder read past the stream");
    assert!(rc.position() <= stream.len());
    decoded
}

fn drive(
    model: &mut Model,
    len: usize,
    expect_marker: bool,
    mut next: impl FnMut(&mut Model) -> ppmd_turbo::Result<Option<u8>>,
) -> Decoded {
    let mut data = Vec::with_capacity(len);
    let mut end_marker = false;
    let total = len + usize::from(expect_marker);
    for _ in 0..total {
        match next(model).expect("decodes") {
            Some(b) => data.push(b),
            None => {
                end_marker = true;
                break;
            }
        }
    }
    Decoded { data, end_marker }
}

/// Index of the first differing byte, or the shorter length when one is a
/// prefix of the other; `None` when equal.
pub fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b {
        return None;
    }
    Some(
        a.iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len())),
    )
}
