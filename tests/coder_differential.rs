//! Differential tests of the range coders against ppmd-rust 1.5.0.
//!
//! ppmd-rust's coders are `pub(crate)`, so they are reached through its full
//! `Ppmd7Encoder` / `Ppmd7aEncoder`: ppmd-rust writes a stream, and the
//! crate's model over the matching coder must decode it. The encoder
//! comparisons wait for the crate's encoders and stay
//! `#[ignore = "awaiting encoder"]`.

use std::io::Write;

#[path = "common/api.rs"]
mod api;

/// Text-like bytes with runs, seeded (SplitMix64), invented content only.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let words: [&[u8]; 8] = [
        b"lorem ", b"ipsum ", b"quartz ", b"vellum ", b"\n", b"0x7F ", b"zephyr ", b"ink ",
    ];
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let r = next();
        if r % 13 == 0 {
            out.extend(std::iter::repeat_n((r >> 8) as u8, (r >> 32) as usize % 50));
        } else {
            out.extend_from_slice(words[(r >> 16) as usize % words.len()]);
        }
    }
    out.truncate(len);
    out
}

/// Orders and memory sizes spanning variant H's limits, with a memory size
/// small enough to force restarts.
const PARAMS: [(u32, u32); 5] = [
    (2, 1 << 11),
    (2, 1 << 16),
    (6, 1 << 20),
    (16, 1 << 24),
    (64, 1 << 24),
];

fn ppmd_rust_7z(data: &[u8], order: u32, mem_size: u32) -> Vec<u8> {
    let mut enc = ppmd_rust::Ppmd7Encoder::new(Vec::new(), order, mem_size).unwrap();
    enc.write_all(data).unwrap();
    enc.finish(false).unwrap()
}

fn ppmd_rust_7a(data: &[u8], order: u32, mem_size: u32) -> Vec<u8> {
    let mut enc = ppmd_rust::Ppmd7aEncoder::new(Vec::new(), order, mem_size).unwrap();
    enc.write_all(data).unwrap();
    enc.finish(false).unwrap()
}

#[test]
fn sevenz_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, seed as u64);
        let stream = ppmd_rust_7z(&data, order, mem);
        let got = api::decode_7z(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
fn sevenz_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 100 + seed as u64);
        let want = ppmd_rust_7z(&data, order, mem);
        let got = api::encode_7z(&data, order, mem, false).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}

#[test]
fn carryless_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 200 + seed as u64);
        let stream = ppmd_rust_7a(&data, order, mem);
        let got = api::decode_carryless(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
fn carryless_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 300 + seed as u64);
        let want = ppmd_rust_7a(&data, order, mem);
        let got = api::encode_carryless(&data, order, mem, false).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}
