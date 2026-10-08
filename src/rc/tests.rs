//! Coder-level tests that need both directions: encoder-to-decoder round
//! trips, byte-identical output against straightforward reference coders
//! written from the C formulas, and truncation. Everything is seeded and
//! deterministic.

use super::*;
use crate::error::Error;

/// SplitMix64: a seeded, dependency-free generator for test data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % (u64::from(hi) - u64::from(lo) + 1)) as u32
    }
}

/// `MAX_FREQ` (`Ppmd7.h`): the largest frequency of one symbol.
const MAX_FREQ: u32 = 124;

/// The smallest and largest binary probabilities the model produces
/// (`min(BinSumm) = 95`, `Ppmd7Dec.c:148`; symmetric at the top).
const BIN_MIN: u32 = 95;
const BIN_MAX: u32 = BIN_TOTAL - BIN_MIN;

#[derive(Clone, Copy, Debug)]
enum Op {
    Sym { start: u32, size: u32, total: u32 },
    Bit { size0: u32, bit: u32 },
}

/// Random operations whose totals stay within what the coder guarantees to
/// carry: up to `0xFFFF` (7z, `SummFreq` is a `u16`) or `BOT` (carry-less,
/// whose range never drops below `BOT` after normalization). Half the
/// symbols look like the model's (sizes up to `MAX_FREQ`, totals up to 256
/// such symbols), half are uniform over the whole span.
fn ops(rng: &mut Rng, n: usize, max_total: u32) -> Vec<Op> {
    (0..n)
        .map(|_| match rng.next() % 4 {
            0 => Op::Bit {
                size0: rng.range(BIN_MIN, BIN_MAX),
                bit: (rng.next() & 1) as u32,
            },
            1 => {
                let total = rng.range(2, (256 * MAX_FREQ).min(max_total));
                let size = rng.range(1, MAX_FREQ.min(total));
                let start = rng.range(0, total - size);
                Op::Sym { start, size, total }
            }
            2 => {
                let total = rng.range(1, max_total);
                let size = rng.range(1, total);
                let start = rng.range(0, total - size);
                Op::Sym { start, size, total }
            }
            _ => {
                // Escapes: a large size at the top of the range.
                let total = rng.range(2, max_total);
                let size = rng.range(1, total.min(64));
                Op::Sym {
                    start: total - size,
                    size,
                    total,
                }
            }
        })
        .collect()
}

fn encode_ops<E: RangeEncoder>(enc: &mut E, ops: &[Op]) {
    for op in ops {
        match *op {
            Op::Sym { start, size, total } => enc.encode(start, size, total),
            Op::Bit { size0, bit } => enc.encode_bit(size0, bit),
        }
    }
    assert!(!enc.faulted());
}

fn check_decode_ops<D: RangeDecoder>(dec: &mut D, ops: &[Op]) {
    for (i, op) in ops.iter().enumerate() {
        match *op {
            Op::Sym { start, size, total } => {
                let count = dec.get_threshold(total);
                assert!(
                    (start..start + size).contains(&count),
                    "op {i}: count {count} outside [{start}, {})",
                    start + size
                );
                dec.decode(start, size);
            }
            Op::Bit { size0, bit } => assert_eq!(dec.decode_bit(size0), bit, "op {i}"),
        }
    }
    assert!(!dec.faulted());
}

const ROUND_TRIP_OPS: usize = if cfg!(miri) { 2_000 } else { 200_000 };

#[test]
fn sevenz_round_trips_random_operations() {
    for seed in 0..4 {
        let ops = ops(&mut Rng(seed), ROUND_TRIP_OPS, 0xFFFF);
        let mut enc = SevenZipRangeEncoder::new(Vec::new());
        encode_ops(&mut enc, &ops);
        let stream = enc.finish().unwrap();
        assert_eq!(stream[0], 0);

        let mut dec = SevenZipRangeDecoder::new(&stream[..]).unwrap();
        check_decode_ops(&mut dec, &ops);
        // 7z's end condition holds and every byte, and no padding, was used.
        assert!(dec.is_finished_ok());
        assert_eq!(dec.position(), stream.len());
        assert_eq!(dec.zero_bytes_past_eof(), 0);
    }
}

#[test]
fn carryless_round_trips_random_operations() {
    for seed in 10..14 {
        let ops = ops(&mut Rng(seed), ROUND_TRIP_OPS, BOT);
        let mut enc = CarrylessRangeEncoder::new(Vec::new());
        encode_ops(&mut enc, &ops);
        let stream = enc.finish().unwrap();

        let mut dec = CarrylessRangeDecoder::new(&stream[..]).unwrap();
        check_decode_ops(&mut dec, &ops);
        assert!(dec.is_finished_ok());
        assert_eq!(dec.position(), stream.len());
        assert_eq!(dec.zero_bytes_past_eof(), 0);
    }
}

/// The same stream decodes identically from every input backing, and a
/// shared source is left right after the coder's bytes.
#[test]
fn every_input_backing_decodes_the_same() {
    let ops = ops(&mut Rng(99), ROUND_TRIP_OPS / 10, BOT);
    let mut enc = CarrylessRangeEncoder::new(WriteOutput::with_flush_size(Vec::new(), 7));
    encode_ops(&mut enc, &ops);
    let stream = enc.finish().unwrap().into_inner();

    let mut dec = CarrylessRangeDecoder::new(ReadInput::with_refill_size(&stream[..], 5)).unwrap();
    check_decode_ops(&mut dec, &ops);
    assert_eq!(dec.position(), stream.len());

    let mut with_tail = stream.clone();
    with_tail.extend_from_slice(b"next block");
    let mut source = super::input::Lent::new(&with_tail, 9);
    {
        let mut dec = CarrylessRangeDecoder::new(&mut source).unwrap();
        check_decode_ops(&mut dec, &ops);
    }
    assert_eq!(source.rest(), b"next block");

    let mut out = vec![0u8; stream.len()];
    let mut enc = SevenZipRangeEncoder::new(SliceOutput::new(&mut out));
    let ops7 = self::ops(&mut Rng(98), 100, 0xFFFF);
    encode_ops(&mut enc, &ops7);
    let written = enc.finish().unwrap().len();
    let mut dec =
        SevenZipRangeDecoder::new(ReadInput::with_refill_size(&out[..written], 3)).unwrap();
    check_decode_ops(&mut dec, &ops7);
    assert!(dec.is_finished_ok());
}

// ---------------------------------------------------------------------------
// Reference coders: the C formulas transcribed with u64 arithmetic and
// explicit masks, no wrapping tricks and no shortcuts, so a mistake in the
// optimised coders' integer handling shows as a byte difference.

const MASK32: u64 = 0xFFFF_FFFF;

/// `Ppmd7z_RangeEnc_*` (`Ppmd7Enc.c`), straight from the C.
struct Reference7z {
    low: u64,
    range: u64,
    cache: u64,
    cache_size: u64,
    out: Vec<u8>,
}

impl Reference7z {
    fn new() -> Self {
        Self {
            low: 0,
            range: MASK32,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
        }
    }

    fn shift_low(&mut self) {
        if (self.low & MASK32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let mut temp = self.cache;
            loop {
                self.out.push(((temp + (self.low >> 32)) & 0xFF) as u8);
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low & MASK32) >> 24) & 0xFF;
        }
        self.cache_size += 1;
        self.low = ((self.low & MASK32) << 8) & MASK32;
    }

    fn norm_step(&mut self) -> bool {
        if self.range < (1 << 24) {
            self.range = (self.range << 8) & MASK32;
            self.shift_low();
            return true;
        }
        false
    }

    fn encode(&mut self, start: u32, size: u32, total: u32) {
        self.range /= u64::from(total);
        self.low += (u64::from(start) * self.range) & MASK32;
        self.range = (self.range * u64::from(size)) & MASK32;
        // RC_NORM: two conditional steps.
        if self.norm_step() {
            self.norm_step();
        }
    }

    fn encode_bit(&mut self, size0: u32, bit: u32) {
        let bound = (self.range >> 14) * u64::from(size0);
        if bit == 0 {
            self.range = bound;
            self.norm_step(); // RC_NORM_1
        } else {
            self.low += bound;
            self.range -= bound;
            if self.norm_step() {
                self.norm_step();
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

/// Subbotin's carry-less encoder, straight from the formulas.
struct ReferenceCarryless {
    low: u64,
    range: u64,
    out: Vec<u8>,
}

impl ReferenceCarryless {
    fn new() -> Self {
        Self {
            low: 0,
            range: MASK32,
            out: Vec::new(),
        }
    }

    fn normalize(&mut self) {
        loop {
            let top_settled = (self.low ^ ((self.low + self.range) & MASK32)) >= (1 << 24);
            if top_settled {
                if self.range >= (1 << 15) {
                    break;
                }
                // range = -low & (BOT - 1)
                self.range = ((1u64 << 32) - self.low) % (1 << 15);
            }
            self.out.push((self.low >> 24) as u8);
            self.range = (self.range << 8) & MASK32;
            self.low = (self.low << 8) & MASK32;
        }
    }

    fn encode(&mut self, start: u32, size: u32, total: u32) {
        self.range /= u64::from(total);
        self.low = (self.low + u64::from(start) * self.range) & MASK32;
        self.range *= u64::from(size);
        self.normalize();
    }

    fn encode_bit(&mut self, size0: u32, bit: u32) {
        if bit == 0 {
            self.encode(0, size0, 1 << 14);
        } else {
            self.encode(size0, (1 << 14) - size0, 1 << 14);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.out.push((self.low >> 24) as u8);
            self.low = (self.low << 8) & MASK32;
        }
        self.out
    }
}

// ---------------------------------------------------------------------------
// A tiny adaptive order-0 model (not PPMd): before each byte, a binary
// "same as the previous byte" decision with an adaptive probability in the
// model's BinSumm range; on a miss, the byte from a frequency table that
// grows by 24 per hit and halves past a limit.

struct Order0 {
    freq: [u32; 256],
    total: u32,
    prob: u32,
    prev: u8,
    limit: u32,
}

impl Order0 {
    fn new(limit: u32) -> Self {
        Self {
            freq: [1; 256],
            total: 256,
            prob: BIN_TOTAL / 2,
            prev: 0,
            limit,
        }
    }

    fn update_bit(&mut self, bit: u32) {
        if bit == 0 {
            self.prob += (BIN_TOTAL - self.prob) >> 5;
        } else {
            self.prob -= self.prob >> 5;
        }
        self.prob = self.prob.clamp(BIN_MIN, BIN_MAX);
    }

    fn update_sym(&mut self, sym: u8) {
        self.freq[usize::from(sym)] += 24;
        self.total += 24;
        if self.total > self.limit {
            self.total = 0;
            for f in &mut self.freq {
                *f = f.div_ceil(2);
                self.total += *f;
            }
        }
        self.prev = sym;
    }

    fn start_of(&self, sym: u8) -> u32 {
        self.freq[..usize::from(sym)].iter().sum()
    }

    /// Calls `bit(size0, bit)` and `sym(start, size, total)` for one byte.
    fn code(
        &mut self,
        byte: u8,
        mut bit: impl FnMut(u32, u32),
        mut sym: impl FnMut(u32, u32, u32),
    ) {
        let b = u32::from(byte != self.prev);
        bit(self.prob, b);
        self.update_bit(b);
        if b == 1 {
            sym(
                self.start_of(byte),
                self.freq[usize::from(byte)],
                self.total,
            );
        }
        self.update_sym(byte);
    }

    /// Decodes one byte; `Err(())` when the coder reports a count no symbol
    /// owns or faults, which is how a model sees a corrupt stream.
    fn decode<D: RangeDecoder>(&mut self, dec: &mut D) -> core::result::Result<u8, ()> {
        let b = dec.decode_bit(self.prob);
        self.update_bit(b);
        let byte = if b == 0 {
            self.prev
        } else {
            let count = dec.get_threshold(self.total);
            if count >= self.total {
                return Err(());
            }
            let mut start = 0;
            let mut sym = 0usize;
            while start + self.freq[sym] <= count {
                start += self.freq[sym];
                sym += 1;
            }
            dec.decode(start, self.freq[sym]);
            sym as u8
        };
        if dec.faulted() {
            return Err(());
        }
        self.update_sym(byte);
        Ok(byte)
    }
}

/// Text-like bytes with runs, so both the binary and the symbol paths and
/// both normalization schedules are exercised.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let alphabet = b"etaoin shrdlu cmfwyp vbgkqjxz ETAOIN.,\n0123456789";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let r = rng.next();
        let byte = if r.is_multiple_of(7) {
            (r >> 8) as u8
        } else {
            alphabet[(r >> 16) as usize % alphabet.len()]
        };
        let run = if r.is_multiple_of(11) {
            1 + (r >> 40) % 40
        } else {
            1
        };
        for _ in 0..run {
            out.push(byte);
        }
    }
    out.truncate(len);
    out
}

/// The model's limits: 7z totals up to `0xFFFF`, carry-less up to `BOT`.
const LIMIT_7Z: u32 = 0xFF00;
const LIMIT_CARRYLESS: u32 = BOT - 300;

fn encode_7z(data: &[u8]) -> Vec<u8> {
    let mut model = Order0::new(LIMIT_7Z);
    let mut enc = SevenZipRangeEncoder::new(Vec::new());
    for &byte in data {
        let e = core::cell::RefCell::new(&mut enc);
        model.code(
            byte,
            |s0, b| e.borrow_mut().encode_bit(s0, b),
            |s, n, t| e.borrow_mut().encode(s, n, t),
        );
    }
    enc.finish().unwrap()
}

fn encode_carryless(data: &[u8]) -> Vec<u8> {
    let mut model = Order0::new(LIMIT_CARRYLESS);
    let mut enc = CarrylessRangeEncoder::new(Vec::new());
    for &byte in data {
        let e = core::cell::RefCell::new(&mut enc);
        model.code(
            byte,
            |s0, b| e.borrow_mut().encode_bit(s0, b),
            |s, n, t| e.borrow_mut().encode(s, n, t),
        );
    }
    enc.finish().unwrap()
}

const SAMPLE_LEN: usize = if cfg!(miri) { 2_000 } else { 100_000 };

#[test]
fn order0_streams_are_byte_identical_to_the_reference_coders() {
    for seed in 0..3 {
        let data = sample(SAMPLE_LEN, seed);

        let mut model = Order0::new(LIMIT_7Z);
        let reference = core::cell::RefCell::new(Reference7z::new());
        for &byte in &data {
            model.code(
                byte,
                |s0, b| reference.borrow_mut().encode_bit(s0, b),
                |s, n, t| reference.borrow_mut().encode(s, n, t),
            );
        }
        let want = reference.into_inner().finish();
        let got = encode_7z(&data);
        assert_eq!(got, want, "7z coder, seed {seed}");
        assert!(got.len() < data.len() / 2, "the model compresses");

        let mut model = Order0::new(LIMIT_CARRYLESS);
        let reference = core::cell::RefCell::new(ReferenceCarryless::new());
        for &byte in &data {
            model.code(
                byte,
                |s0, b| reference.borrow_mut().encode_bit(s0, b),
                |s, n, t| reference.borrow_mut().encode(s, n, t),
            );
        }
        let want = reference.into_inner().finish();
        let got = encode_carryless(&data);
        assert_eq!(got, want, "carry-less coder, seed {seed}");
    }
}

#[test]
fn order0_streams_decode_back() {
    let data = sample(SAMPLE_LEN, 7);

    let stream = encode_7z(&data);
    let mut model = Order0::new(LIMIT_7Z);
    let mut dec = SevenZipRangeDecoder::new(&stream[..]).unwrap();
    let decoded: Vec<u8> = (0..data.len())
        .map(|_| model.decode(&mut dec).unwrap())
        .collect();
    assert_eq!(decoded, data);
    assert!(dec.is_finished_ok());
    assert_eq!(dec.position(), stream.len());

    let stream = encode_carryless(&data);
    let mut model = Order0::new(LIMIT_CARRYLESS);
    let mut dec =
        CarrylessRangeDecoder::new(ReadInput::with_refill_size(&stream[..], 1000)).unwrap();
    let decoded: Vec<u8> = (0..data.len())
        .map(|_| model.decode(&mut dec).unwrap())
        .collect();
    assert_eq!(decoded, data);
    assert!(dec.is_finished_ok());
    assert_eq!(dec.position(), stream.len());
}

// ---------------------------------------------------------------------------
// Truncation: a prefix of a valid stream either fails to initialize with
// `Truncated` or decodes exactly as the prefix followed by zero bytes does,
// and reports as padding precisely the bytes it took past the prefix.

struct Outcome {
    result: core::result::Result<Vec<u8>, usize>,
    position: usize,
    zeros: u32,
}

fn run<D: RangeDecoder>(dec: &mut D, limit: u32, n: usize) -> core::result::Result<Vec<u8>, usize> {
    let mut model = Order0::new(limit);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        match model.decode(dec) {
            Ok(byte) => out.push(byte),
            Err(()) => return Err(i),
        }
    }
    Ok(out)
}

fn decode_7z(stream: &[u8], n: usize) -> Option<Outcome> {
    let mut dec = match SevenZipRangeDecoder::new(stream) {
        Ok(dec) => dec,
        Err(Error::Truncated) => {
            assert!(stream.len() < 5);
            return None;
        }
        // A bad first byte or code: only garbage gets here.
        Err(Error::CorruptStream { .. }) => return None,
        Err(e) => panic!("unexpected {e}"),
    };
    let result = run(&mut dec, LIMIT_7Z, n);
    Some(Outcome {
        result,
        position: dec.position(),
        zeros: dec.zero_bytes_past_eof(),
    })
}

fn decode_carryless(stream: &[u8], n: usize) -> Option<Outcome> {
    let mut dec = match CarrylessRangeDecoder::new(stream) {
        Ok(dec) => dec,
        Err(Error::Truncated) => {
            assert!(stream.len() < 4);
            return None;
        }
        Err(e) => panic!("unexpected {e}"),
    };
    let result = run(&mut dec, LIMIT_CARRYLESS, n);
    Some(Outcome {
        result,
        position: dec.position(),
        zeros: dec.zero_bytes_past_eof(),
    })
}

fn check_prefixes(stream: &[u8], n: usize, decode: fn(&[u8], usize) -> Option<Outcome>) {
    let full = decode(stream, n).unwrap();
    assert!(full.result.is_ok());
    assert_eq!(full.zeros, 0);
    for len in 0..=stream.len() {
        let prefix = &stream[..len];
        let Some(got) = decode(prefix, n) else {
            continue;
        };
        // Enough zeros that the padded decode never runs dry itself: no
        // operation takes more than four bytes, and a symbol is two
        // operations.
        let mut padded = prefix.to_vec();
        padded.resize(stream.len() + 8 * n + 64, 0);
        let want = decode(&padded, n).unwrap();
        assert_eq!(want.zeros, 0, "prefix {len}");
        assert_eq!(got.result, want.result, "prefix {len}");
        assert_eq!(got.position, want.position.min(len), "prefix {len}");
        assert_eq!(
            got.zeros as usize,
            want.position.saturating_sub(len),
            "prefix {len}"
        );
    }
}

#[test]
fn every_prefix_decodes_like_the_zero_padded_stream() {
    let n = if cfg!(miri) { 60 } else { 600 };
    let data = sample(n, 3);
    check_prefixes(&encode_7z(&data), n, decode_7z);
    check_prefixes(&encode_carryless(&data), n, decode_carryless);
}

/// Arbitrary bytes, decoded against an arbitrary model, never panic and
/// never loop: the decode either finishes or reports a corrupt count.
#[test]
fn garbage_never_panics() {
    let mut rng = Rng(1234);
    let rounds = if cfg!(miri) { 20 } else { 2_000 };
    for _ in 0..rounds {
        let len = rng.range(0, 64) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if rng.next() & 1 == 0 && !bytes.is_empty() {
            bytes[0] = 0;
        }
        let _ = decode_7z(&bytes, 200);
        let _ = decode_carryless(&bytes, 200);
        // Raw operations with hostile totals and sizes.
        if let Ok(mut d) = CarrylessRangeDecoder::new(&bytes[..]) {
            for _ in 0..50 {
                let total = rng.range(0, u32::MAX);
                let _ = d.get_threshold(total);
                d.decode(rng.range(0, u32::MAX), rng.range(0, 3));
                let _ = d.decode_bit(rng.range(0, u32::MAX));
            }
        }
        if let Ok(mut d) = SevenZipRangeDecoder::new(&bytes[..]) {
            for _ in 0..50 {
                let total = rng.range(0, u32::MAX);
                let _ = d.get_threshold(total);
                d.decode(rng.range(0, u32::MAX), rng.range(0, 3));
                let _ = d.decode_bit(rng.range(0, u32::MAX));
            }
        }
    }
}
