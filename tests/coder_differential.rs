//! Differential tests of the range coders against ppmd-rust 1.5.0.
//!
//! ppmd-rust's coders are `pub(crate)`, so they are reached through its full
//! `Ppmd7Encoder` / `Ppmd7aEncoder`: ppmd-rust writes a stream, and the
//! crate's model over the matching coder must decode it. The encoder
//! comparisons wait for the crate's encoders and stay
//! `#[ignore = "awaiting encoder"]`.

use std::io::Write;

mod api {
    use ppmd_turbo::model::Model;
    use ppmd_turbo::rc::CarrylessRangeDecoder;
    use ppmd_turbo::{Error, Result};

    /// Decodes a raw 7z `PPMD` stream (7z coder): exactly `unpacked_len`
    /// bytes when given, otherwise up to the end marker.
    pub fn decode_7z(
        stream: &[u8],
        order: u32,
        mem_size: u32,
        unpacked_len: Option<u64>,
    ) -> Result<Vec<u8>> {
        ppmd_turbo::decode_7z(stream, order, mem_size, unpacked_len)
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
    /// (7-Zip's `Ppmd7a`, Shkarin's `.pmd`): the model over
    /// `CarrylessRangeDecoder::new_7a`. Exactly `unpacked_len` symbols when
    /// given, otherwise up to the end marker; reading past the input is an
    /// error, as in 7-Zip.
    pub fn decode_7a(
        stream: &[u8],
        order: u32,
        mem_size: u32,
        unpacked_len: Option<u64>,
    ) -> Result<Vec<u8>> {
        let mut model = Model::new(order, mem_size)?;
        let mut rc = CarrylessRangeDecoder::new_7a(stream)?;
        let mut out = Vec::new();
        while unpacked_len.is_none_or(|n| (out.len() as u64) < n) {
            let symbol = model.decode_symbol(&mut rc)?;
            if rc.zero_bytes_past_eof() != 0 {
                return Err(Error::Truncated);
            }
            match symbol {
                Some(byte) => out.push(byte),
                None if unpacked_len.is_none() => break,
                None => {
                    return Err(Error::CorruptStream {
                        detail: "end marker",
                    });
                }
            }
        }
        Ok(out)
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
fn sevenz_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, seed as u64);
        let stream = ppmd_rust_7z(&data, order, mem);
        let got = api::decode_7z(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
#[ignore = "awaiting encoder"]
fn sevenz_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 100 + seed as u64);
        let want = ppmd_rust_7z(&data, order, mem);
        let got = api::encode_7z(&data, order, mem).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}

#[test]
fn carryless_streams_from_ppmd_rust_decode() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 200 + seed as u64);
        let stream = ppmd_rust_7a(&data, order, mem);
        let got = api::decode_7a(&stream, order, mem, Some(data.len() as u64)).unwrap();
        assert!(got == data, "order {order}, mem {mem}");
    }
}

#[test]
#[ignore = "awaiting encoder"]
fn carryless_encoding_is_byte_identical_to_ppmd_rust() {
    for (seed, (order, mem)) in PARAMS.into_iter().enumerate() {
        let data = sample(200_000, 300 + seed as u64);
        let want = ppmd_rust_7a(&data, order, mem);
        let got = api::encode_7a(&data, order, mem).unwrap();
        assert!(got == want, "order {order}, mem {mem}");
    }
}
