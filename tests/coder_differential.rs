//! Differential tests of the range coders against ppmd-rust 1.5.0.
//!
//! ppmd-rust's coders are `pub(crate)`, so they can only be reached through
//! its full `Ppmd7Encoder` / `Ppmd7aEncoder`, and those need a context model
//! on this side too. Until the model lands every test here is
//! `#[ignore = "awaiting model"]`; the coders are proven on their own by the
//! hand-computed vectors, round trips, reference-coder comparisons and
//! truncation tests in `src/rc/`.
//!
//! The functions in [`api`] mirror the corpus harness's shim
//! (`tests/common/api.rs`): a raw 7z `PPMD` stream with its order and memory
//! size, decoded to `unpacked_len` bytes. When the model lands, fill in the
//! intended bodies (or switch to `common::api`) and drop the `ignore`s.

use std::io::Write;

mod api {
    use ppmd_turbo::Result;

    /// Decodes a raw 7z `PPMD` stream (7z coder): exactly `unpacked_len`
    /// bytes when given, otherwise up to the end marker.
    ///
    /// Intended body: as `tests/common/api.rs::decode_7z`, through
    /// `ppmd_turbo::ppmd7::Ppmd7Decoder::new(stream, order, mem_size)`.
    pub fn decode_7z(
        stream: &[u8],
        order: u32,
        mem_size: u32,
        unpacked_len: Option<u64>,
    ) -> Result<Vec<u8>> {
        let _ = (stream, order, mem_size, unpacked_len);
        todo!("awaiting ppmd_turbo::ppmd7::Ppmd7Decoder")
    }

    /// Encodes `data` as a raw 7z `PPMD` stream without an end marker, as
    /// 7-Zip does.
    ///
    /// Intended body: `ppmd_turbo::ppmd7::Ppmd7Encoder::new(Vec::new(), order,
    /// mem_size)?`, `write_all(data)`, `finish(false)`.
    pub fn encode_7z(data: &[u8], order: u32, mem_size: u32) -> Result<Vec<u8>> {
        let _ = (data, order, mem_size);
        todo!("awaiting ppmd_turbo::ppmd7::Ppmd7Encoder")
    }

    /// Decodes a raw variant H stream coded with the carry-less coder
    /// (7-Zip's `Ppmd7a`, Shkarin's `.pmd`).
    ///
    /// Intended body: the 7a decoder over
    /// `ppmd_turbo::rc::CarrylessRangeDecoder::new_7a(stream)`.
    pub fn decode_7a(
        stream: &[u8],
        order: u32,
        mem_size: u32,
        unpacked_len: Option<u64>,
    ) -> Result<Vec<u8>> {
        let _ = (stream, order, mem_size, unpacked_len);
        todo!("awaiting the 7a decoder")
    }

    /// Encodes `data` with the carry-less coder, without an end marker.
    ///
    /// Intended body: the 7a encoder over
    /// `ppmd_turbo::rc::CarrylessRangeEncoder::new(Vec::new())`.
    pub fn encode_7a(data: &[u8], order: u32, mem_size: u32) -> Result<Vec<u8>> {
        let _ = (data, order, mem_size);
        todo!("awaiting the 7a encoder")
    }
}

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
#[ignore = "awaiting model"]
fn sevenz_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, seed as u64);
        let stream = ppmd_rust_7z(&data, order, mem);
        let got = api::decode_7z(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
#[ignore = "awaiting model"]
fn sevenz_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 100 + seed as u64);
        let want = ppmd_rust_7z(&data, order, mem);
        let got = api::encode_7z(&data, order, mem).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}

#[test]
#[ignore = "awaiting model"]
fn carryless_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 200 + seed as u64);
        let stream = ppmd_rust_7a(&data, order, mem);
        let got = api::decode_7a(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
#[ignore = "awaiting model"]
fn carryless_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 300 + seed as u64);
        let want = ppmd_rust_7a(&data, order, mem);
        let got = api::encode_7a(&data, order, mem).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}
