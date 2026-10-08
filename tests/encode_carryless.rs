//! The carry-less encoder: raw PPMd streams over Subbotin's coder, checked
//! for correctness only. Every stream round-trips through the crate's
//! carry-less decoder and through `RarPpmd`, and matches ppmd-rust's
//! `Ppmd7aEncoder` (Shkarin's coder as 7-Zip's `Ppmd7a` reads it) byte for
//! byte. No RAR framing is written or read here.

mod common;
mod encode_support;

use common::api::{RarDecoder, decode_carryless, encode_carryless};
use encode_support::{
    exhausting_payload, first_difference, payloads, reference_carryless,
    reference_decode_carryless, turbo_decode_carryless,
};
use ppmd_turbo::{CarrylessEncoder, ErrorKind, Params};

/// Every carry-less stream in the fixture manifest is reproduced byte for
/// byte from its payload (recovered through ppmd-rust and checked against
/// the manifest digest).
#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn reproduces_every_committed_carryless_stream() {
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for s in common::list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "carry-less")
    {
        let name = s["name"].as_str().unwrap();
        let Some(stream) = common::read(s) else {
            continue;
        };
        let order = common::u64_of(s, "order") as u32;
        let mem = common::u64_of(s, "mem") as u32;
        let end_marker = s["end_marker"] == true;
        let len = common::u64_of(s, "payload_len") as usize;
        let payload = reference_decode_carryless(&stream, order, mem, (!end_marker).then_some(len));
        assert_eq!(
            common::sha256(&payload),
            common::payload_sha256(&manifest, s),
            "{name}: payload"
        );
        let ours = encode_carryless(&payload, order, mem, end_marker).expect("encodes");
        if let Some(at) = first_difference(&ours, &stream) {
            failures.push(format!(
                "{name}: first difference at byte {at} of {}/{}",
                ours.len(),
                stream.len()
            ));
        }
        checked += 1;
    }
    common::report(&failures, checked);
}

/// The parameter grid against ppmd-rust's carry-less encoder.
#[test]
#[cfg_attr(miri, ignore = "large grid")]
fn byte_identical_to_ppmd_rust_across_the_grid() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, data) in payloads() {
        for order in [2u32, 4, 6, 16, 64] {
            for mem in [2048u32, 1 << 16, 1 << 20] {
                for end_marker in [false, true] {
                    let want = reference_carryless(&data, order, mem, end_marker);
                    let ours = encode_carryless(&data, order, mem, end_marker).expect("encodes");
                    if let Some(at) = first_difference(&ours, &want) {
                        failures.push(format!(
                            "{name} o={order} mem={mem} eos={end_marker}: byte {at} of {}/{}",
                            ours.len(),
                            want.len()
                        ));
                    }
                    checked += 1;
                }
            }
        }
    }
    common::report(&failures, checked);
}

/// Round trips through the crate's model over `CarrylessRangeDecoder` and
/// through `CarrylessDecoder`.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn round_trips_through_the_carryless_decoder() {
    for (name, data) in payloads() {
        for (order, mem) in [(2u32, 1u32 << 20), (6, 1 << 16), (16, 2048), (64, 1 << 20)] {
            for end_marker in [false, true] {
                let stream = encode_carryless(&data, order, mem, end_marker).unwrap();
                let back = turbo_decode_carryless(&stream, order, mem, data.len(), end_marker);
                let what = format!("{name} o={order} mem={mem} eos={end_marker}");
                assert!(back.data == data, "{what}");
                assert_eq!(back.end_marker, end_marker, "{what}");
                let len = (!end_marker).then_some(data.len() as u64);
                let stepped = decode_carryless(&stream, order, mem, len).unwrap();
                assert!(stepped == data, "{what}: CarrylessDecoder");
            }
        }
    }
}

/// Round trips through RAR's decoder: `decode` (stopping at the end
/// marker, or at the symbol count) and `next_symbol`. RAR declares the
/// arena in whole MiB.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn round_trips_through_the_rar_decoder() {
    for (name, data) in payloads() {
        for (order, mem_mb) in [(2u32, 1u32), (6, 1), (16, 4), (64, 1)] {
            let mem = mem_mb << 20;
            let what = format!("{name} o={order} mem={mem_mb}M");

            let marked = encode_carryless(&data, order, mem, true).unwrap();
            let mut rar = RarDecoder::new();
            let mut out = Vec::new();
            let used = rar
                .decode_block(true, order, mem_mb, &marked, u64::MAX, &mut out)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(out == data, "{what}: decode_block to the end marker");
            assert!(used <= marked.len(), "{what}");

            let unmarked = encode_carryless(&data, order, mem, false).unwrap();
            let mut rar = RarDecoder::new();
            let mut out = Vec::new();
            rar.decode_block(true, order, mem_mb, &unmarked, data.len() as u64, &mut out)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(out == data, "{what}: decode_block by count");

            let mut rar = RarDecoder::new();
            let params = Params::rar(order, mem_mb).unwrap();
            let out = rar.decode_symbols(Some(params), &marked).unwrap();
            assert!(out == data, "{what}: next_symbol");
        }
    }
}

/// Small arenas at high orders restart the model over and over; the
/// stream stays identical to ppmd-rust's and round-trips, and a 1 MiB
/// arena exhausts through RAR's decoder too.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn arena_exhaustion_round_trips() {
    let data = exhausting_payload(200_000);
    for (order, mem) in [(64u32, 2048u32), (32, 4096), (16, 1 << 14)] {
        let ours = encode_carryless(&data, order, mem, true).unwrap();
        assert_eq!(
            first_difference(&ours, &reference_carryless(&data, order, mem, true)),
            None,
            "o={order} mem={mem}"
        );
        let back = turbo_decode_carryless(&ours, order, mem, data.len(), true);
        assert!(back.data == data && back.end_marker, "o={order} mem={mem}");
    }

    let big = exhausting_payload(600_000);
    let stream = encode_carryless(&big, 64, 1 << 20, true).unwrap();
    let mut rar = RarDecoder::new();
    let mut out = Vec::new();
    rar.decode_block(true, 64, 1, &stream, u64::MAX, &mut out)
        .unwrap();
    assert!(out == big, "RAR decoder after arena exhaustion");
}

/// One call into a buffer that fits writes what the uneven pieces write.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn one_call_matches_uneven_pieces() {
    for (name, data) in payloads() {
        for end_marker in [false, true] {
            let mut enc = CarrylessEncoder::new(Params::new(8, 1 << 20).unwrap()).unwrap();
            let mut out = vec![0u8; data.len() * 2 + 64];
            let step = enc.encode(&data, &mut out).unwrap();
            assert_eq!(step.consumed, data.len());
            let fin = enc.finish(&mut out[step.produced..], end_marker).unwrap();
            assert!(fin.done);
            out.truncate(step.produced + fin.produced);
            assert_eq!(
                out,
                encode_carryless(&data, 8, 1 << 20, end_marker).unwrap(),
                "{name} eos={end_marker}"
            );
        }
    }
}

/// Parameters outside variant H's range are refused.
#[test]
fn rejects_out_of_range_parameters() {
    for (order, mem) in [(1u32, 1u32 << 20), (65, 1 << 20), (6, 2047)] {
        assert_eq!(
            Params::new(order, mem).unwrap_err().kind,
            ErrorKind::InvalidParameters
        );
        assert_eq!(
            encode_carryless(b"x", order, mem, false).unwrap_err().kind,
            ErrorKind::InvalidParameters
        );
    }
}

/// Short inputs at small arenas: the shape Miri can afford.
#[test]
fn small_inputs_under_miri() {
    let data = encode_support::corpus::records(9, 120);
    for (order, mem) in [(2u32, 2048u32), (64, 4096)] {
        for end_marker in [false, true] {
            let stream = encode_carryless(&data, order, mem, end_marker).unwrap();
            let back = turbo_decode_carryless(&stream, order, mem, data.len(), end_marker);
            assert!(back.data == data, "o={order} mem={mem}");
            assert_eq!(back.end_marker, end_marker);
        }
    }
}
