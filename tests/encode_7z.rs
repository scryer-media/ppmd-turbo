//! The 7z encoder: byte-identical to 7-Zip (through the committed `7zz`
//! streams and ppmd-rust 1.5.0, a port of 7-Zip's `Ppmd7Enc.c`), and
//! decodable by both ppmd-rust and the crate's own model.

mod common;
mod encode_support;

use std::io::{Read, Write};

use encode_support::{
    exhausting_payload, first_difference, payloads, reference_7z, reference_decode_7z,
    turbo_decode_7z,
};
use ppmd_turbo::{Ppmd7Decoder, Ppmd7Encoder, decode_7z, encode_7z};

/// Encodes through the `Write` adapter, in uneven chunks.
fn encode_streaming(data: &[u8], order: u32, mem: u32, end_marker: bool) -> Vec<u8> {
    let mut enc = Ppmd7Encoder::new(Vec::new(), order, mem).expect("legal parameters");
    for chunk in data.chunks(777) {
        enc.write_all(chunk).expect("encodes");
    }
    enc.flush().expect("flushes");
    enc.finish(end_marker).expect("finishes")
}

/// Decodes `stream` through the crate's public 7z decoder, both entry
/// points: `decode_7z` and `Ppmd7Decoder` read through `std::io::Read`.
/// With a known size `Ppmd7Decoder` runs in `FinishStream` mode and must
/// consume the stream exactly; without one, the end marker must stop it.
fn real_decode_7z(stream: &[u8], order: u32, mem: u32, len: usize, end_marker: bool, what: &str) {
    let known = (!end_marker).then_some(len as u64);
    let sliced = decode_7z(stream, order, mem, known).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(sliced.len() == len, "{what}: decode_7z length");

    let mut dec = match known {
        Some(n) => {
            let mut dec = Ppmd7Decoder::with_unpacked_size(stream, order, mem, n).unwrap();
            dec.set_finish_stream(true);
            dec
        }
        None => Ppmd7Decoder::new(stream, order, mem).unwrap(),
    };
    let mut read = Vec::new();
    dec.read_to_end(&mut read)
        .unwrap_or_else(|e| panic!("{what}: Ppmd7Decoder: {e}"));
    assert!(dec.is_finished(), "{what}: Ppmd7Decoder finished");
    assert_eq!(
        dec.position(),
        stream.len(),
        "{what}: Ppmd7Decoder consumed"
    );
    assert!(read == sliced, "{what}: Ppmd7Decoder vs decode_7z");
}

/// Every 7z stream in the fixture manifest, whether `7zz` or ppmd-rust
/// wrote it, is reproduced byte for byte, with the manifest's order, memory
/// size and end-marker setting. The payloads are recovered by decoding the
/// committed stream with ppmd-rust and checked against the manifest digest.
#[test]
#[cfg_attr(miri, ignore = "reads fixture files")]
fn reproduces_every_committed_7z_stream() {
    let manifest = common::manifest();
    let mut failures = Vec::new();
    let mut checked = 0;
    for s in common::list(&manifest, "streams")
        .iter()
        .filter(|s| s["coder"] == "7z")
    {
        let name = s["name"].as_str().unwrap();
        let stream = common::read(s);
        let order = common::u64_of(s, "order") as u32;
        let mem = common::u64_of(s, "mem") as u32;
        let end_marker = s["end_marker"] == true;
        let len = common::u64_of(s, "payload_len") as usize;
        let payload = reference_decode_7z(&stream, order, mem, (!end_marker).then_some(len));
        assert_eq!(
            common::sha256(&payload),
            common::payload_sha256(&manifest, s),
            "{name}: payload"
        );
        let ours = encode_7z(&payload, order, mem, end_marker).expect("encodes");
        if let Some(at) = first_difference(&ours, &stream) {
            failures.push(format!(
                "{name} ({}): first difference at byte {at} of {}/{}",
                s["producer"],
                ours.len(),
                stream.len()
            ));
        }
        checked += 1;
    }
    common::report(&failures, checked);
}

/// The parameter grid against ppmd-rust: orders 2 through 64, arenas from
/// the 2 KiB minimum (which restarts the model constantly) to 16 MiB, with
/// and without the end marker, through both entry points.
#[test]
#[cfg_attr(miri, ignore = "large grid; Miri runs `small_inputs_under_miri`")]
fn byte_identical_to_ppmd_rust_across_the_grid() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, data) in payloads() {
        for order in [2u32, 3, 6, 16, 33, 64] {
            for mem in [2048u32, 1 << 16, 1 << 20, 16 << 20] {
                for end_marker in [false, true] {
                    let want = reference_7z(&data, order, mem, end_marker);
                    let ours = encode_7z(&data, order, mem, end_marker).expect("encodes");
                    if let Some(at) = first_difference(&ours, &want) {
                        failures.push(format!(
                            "{name} o={order} mem={mem} eos={end_marker}: \
                             byte {at} of {}/{}",
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

/// The `Write` adapter, fed in chunks and flushed midway, writes exactly
/// what the slice entry writes.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn the_write_adapter_matches_the_slice_entry() {
    for (name, data) in payloads() {
        for (order, mem) in [(6u32, 1u32 << 16), (64, 2048)] {
            for end_marker in [false, true] {
                assert_eq!(
                    encode_streaming(&data, order, mem, end_marker),
                    encode_7z(&data, order, mem, end_marker).unwrap(),
                    "{name} o={order} mem={mem} eos={end_marker}"
                );
            }
        }
    }
}

/// Round trips through ppmd-rust's decoder, through the crate's public
/// `Ppmd7Decoder` and `decode_7z`, and through the crate's model over the
/// 7z range decoder, which must also finish with a zero code and read the
/// stream exactly to its end.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn round_trips_through_both_decoders() {
    for (name, data) in payloads() {
        for (order, mem) in [(2u32, 1u32 << 20), (6, 1 << 16), (16, 2048), (64, 1 << 20)] {
            for end_marker in [false, true] {
                let stream = encode_7z(&data, order, mem, end_marker).unwrap();
                let what = format!("{name} o={order} mem={mem} eos={end_marker}");
                let len = (!end_marker).then_some(data.len());
                assert!(
                    reference_decode_7z(&stream, order, mem, len) == data,
                    "{what}: ppmd-rust"
                );
                let ours = turbo_decode_7z(&stream, order, mem, data.len(), end_marker);
                assert!(ours.data == data, "{what}: ppmd-turbo's model");
                assert_eq!(ours.end_marker, end_marker, "{what}: end marker");
                real_decode_7z(&stream, order, mem, data.len(), end_marker, &what);
                assert!(
                    decode_7z(&stream, order, mem, len.map(|n| n as u64)).unwrap() == data,
                    "{what}: decode_7z"
                );
            }
        }
    }
}

/// A small arena at a high order restarts the model hundreds of times; the
/// stream stays byte-identical to ppmd-rust's and round-trips.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn arena_exhaustion_restarts_identically() {
    let data = exhausting_payload(200_000);
    for (order, mem) in [(64u32, 2048u32), (32, 4096), (16, 1 << 14), (8, 1 << 16)] {
        let ours = encode_7z(&data, order, mem, true).unwrap();
        let want = reference_7z(&data, order, mem, true);
        assert_eq!(first_difference(&ours, &want), None, "o={order} mem={mem}");
        let back = turbo_decode_7z(&ours, order, mem, data.len(), true);
        assert!(back.data == data && back.end_marker, "o={order} mem={mem}");
        let what = format!("o={order} mem={mem}");
        real_decode_7z(&ours, order, mem, data.len(), true, &what);
        assert!(
            decode_7z(&ours, order, mem, None).unwrap() == data,
            "{what}"
        );
    }
}

/// One encoder can follow another on the same writer: the end marker ends
/// each stream, and each decodes on its own.
#[test]
#[cfg_attr(miri, ignore = "slow under Miri")]
fn back_to_back_streams_on_one_writer() {
    let a = encode_support::corpus::text(1, 3000);
    let b = encode_support::corpus::records(2, 3000);
    let mut enc = Ppmd7Encoder::new(Vec::new(), 6, 1 << 16).unwrap();
    enc.write_all(&a).unwrap();
    let first = enc.finish(true).unwrap();
    let split = first.len();
    let mut enc = Ppmd7Encoder::new(first, 6, 1 << 16).unwrap();
    enc.write_all(&b).unwrap();
    let both = enc.finish(true).unwrap();
    assert_eq!(&both[..split], encode_7z(&a, 6, 1 << 16, true).unwrap());
    assert_eq!(&both[split..], encode_7z(&b, 6, 1 << 16, true).unwrap());
    // The first decoder stops at its end marker, exactly at the split.
    let mut dec = Ppmd7Decoder::new(&both[..], 6, 1 << 16).unwrap();
    let mut back = Vec::new();
    dec.read_to_end(&mut back).unwrap();
    assert!(back == a && dec.position() == split);
    assert!(decode_7z(&both[split..], 6, 1 << 16, None).unwrap() == b);
}

/// Parameters outside variant H's range are refused before anything is
/// written.
#[test]
fn rejects_out_of_range_parameters() {
    for (order, mem) in [(1u32, 1u32 << 16), (65, 1 << 16), (6, 2047), (6, u32::MAX)] {
        assert!(matches!(
            Ppmd7Encoder::new(Vec::new(), order, mem),
            Err(ppmd_turbo::Error::InvalidParameters)
        ));
        assert!(matches!(
            encode_7z(b"x", order, mem, false),
            Err(ppmd_turbo::Error::InvalidParameters)
        ));
    }
}

/// A writer that fails surfaces its error from `flush` or `finish`.
#[test]
fn a_failing_writer_is_reported() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("refused"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut enc = Ppmd7Encoder::new(Broken, 6, 1 << 16).unwrap();
    enc.write_all(b"some bytes to code").unwrap();
    assert!(matches!(enc.finish(true), Err(ppmd_turbo::Error::Io(_))));
}

/// Short inputs at small arenas: the shape Miri can afford.
#[test]
fn small_inputs_under_miri() {
    let data = encode_support::corpus::text(7, 120);
    for (order, mem) in [(2u32, 2048u32), (6, 2048), (64, 4096)] {
        for end_marker in [false, true] {
            let stream = encode_7z(&data, order, mem, end_marker).unwrap();
            let back = turbo_decode_7z(&stream, order, mem, data.len(), end_marker);
            assert!(back.data == data, "o={order} mem={mem}");
            assert_eq!(back.end_marker, end_marker);
            real_decode_7z(&stream, order, mem, data.len(), end_marker, "miri");
        }
    }
}
