//! The two range coders on their own, both directions.
//!
//! - Decode: arbitrary bytes as a coded stream, driven by arbitrary
//!   operations (any total, start, size or binary probability). The decoders
//!   must never panic, never loop and never read out of bounds; a zero
//!   range is a fault, not a division by zero.
//! - Round trip: the operations, clamped to what each coder guarantees to
//!   carry, are encoded and decoded back; every symbol must come back and
//!   the decoder must end exactly at the end of the stream.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ppmd_turbo::rc::{
    CarrylessRangeDecoder, CarrylessRangeEncoder, RangeDecoder, RangeEncoder,
    SevenZipRangeDecoder, SevenZipRangeEncoder,
};

/// One operation per 9 input bytes: a kind and two `u32`s.
fn ops(data: &[u8]) -> impl Iterator<Item = (u8, u32, u32)> + '_ {
    data.chunks_exact(9).map(|c| {
        let a = u32::from_le_bytes([c[1], c[2], c[3], c[4]]);
        let b = u32::from_le_bytes([c[5], c[6], c[7], c[8]]);
        (c[0], a, b)
    })
}

fn drive<D: RangeDecoder>(dec: &mut D, data: &[u8]) {
    for (kind, a, b) in ops(data) {
        match kind % 3 {
            0 => {
                let _ = dec.get_threshold(a);
            }
            1 => dec.decode(a, b),
            _ => {
                let _ = dec.decode_bit(a);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Op {
    Sym(u32, u32, u32),
    Bit(u32, u32),
}

/// Clamps arbitrary operations to valid ones: totals up to `max_total`,
/// `start + size <= total`, binary probabilities in the model's range.
fn valid_ops(data: &[u8], max_total: u32) -> Vec<Op> {
    ops(data)
        .map(|(kind, a, b)| {
            if kind & 1 == 0 {
                Op::Bit(95 + a % (16384 - 2 * 95 + 1), b & 1)
            } else {
                let total = 1 + a % max_total;
                let size = 1 + b % total;
                let start = (b >> 16) % (total - size + 1);
                Op::Sym(start, size, total)
            }
        })
        .collect()
}

fn encode<E: RangeEncoder>(enc: &mut E, ops: &[Op]) {
    for &op in ops {
        match op {
            Op::Sym(start, size, total) => enc.encode(start, size, total),
            Op::Bit(size0, bit) => enc.encode_bit(size0, bit),
        }
    }
    assert!(!enc.faulted());
}

fn check<D: RangeDecoder>(dec: &mut D, ops: &[Op]) {
    for &op in ops {
        match op {
            Op::Sym(start, size, total) => {
                let count = dec.get_threshold(total);
                assert!(count >= start && count - start < size);
                dec.decode(start, size);
            }
            Op::Bit(size0, bit) => assert_eq!(dec.decode_bit(size0), bit),
        }
    }
    assert!(!dec.faulted());
}

fuzz_target!(|data: &[u8]| {
    let Some((&split, rest)) = data.split_first() else {
        return;
    };
    let (stream, script) = rest.split_at(usize::from(split).min(rest.len()));

    if let Ok(mut dec) = SevenZipRangeDecoder::new(stream) {
        drive(&mut dec, script);
    }
    if let Ok(mut dec) = CarrylessRangeDecoder::new(stream) {
        drive(&mut dec, script);
    }

    let ops = valid_ops(script, 0xFFFF);
    let mut enc = SevenZipRangeEncoder::new(Vec::new());
    encode(&mut enc, &ops);
    let coded = enc.finish().expect("valid operations encode");
    let mut dec = SevenZipRangeDecoder::new(&coded[..]).expect("own stream initializes");
    check(&mut dec, &ops);
    assert!(dec.is_finished_ok());
    assert_eq!((dec.position(), dec.zero_bytes_past_eof()), (coded.len(), 0));

    let ops = valid_ops(script, 1 << 15);
    let mut enc = CarrylessRangeEncoder::new(Vec::new());
    encode(&mut enc, &ops);
    let coded = enc.finish().expect("valid operations encode");
    let mut dec = CarrylessRangeDecoder::new(&coded[..]).expect("own stream initializes");
    check(&mut dec, &ops);
    assert!(dec.is_finished_ok());
    assert_eq!((dec.position(), dec.zero_bytes_past_eof()), (coded.len(), 0));
});
