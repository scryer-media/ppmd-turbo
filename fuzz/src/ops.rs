//! F6 `model_ops`: arbitrary operation sequences against the long-lived
//! model APIs, on hostile streams.
//!
//! Two models live for the whole input: a [`RarDecoder`] (RAR's block
//! decoder, whose model survives blocks, resets, `cleanup` and errors) and a
//! bare [`Model`]. Each operation initialises, resets, cleans up, restarts
//! or decodes through one of them, with either range coder, from any offset
//! of a pool of hostile bytes, ignoring the rule that a model is restarted
//! after a corrupt symbol. Properties:
//!
//! - nothing panics, reads out of bounds or runs unbounded, whatever the
//!   model went through before;
//! - every failure has the documented class: parameter checks are
//!   [`ErrKind::InvalidParameters`] and leave the model untouched, a symbol
//!   without a model is [`ErrKind::Corrupt`], and a decode is only ever
//!   corrupt or truncated;
//! - **a restart forgets everything**: after any history, starting the
//!   model again (`init_model` reusing its arena, `Model::start`,
//!   `Model::restart`) and decoding a stream a fresh encoder wrote gives
//!   back exactly that stream's payload.
//!
//! Input: `n_ops` (at most [`MAX_OPS`]), `n_ops` four-byte operations
//! `kind, a, b, c`, then the byte pool the decodes read from.

use std::sync::OnceLock;

use ppmd_turbo::model::Model;
use ppmd_turbo::rar::{MAX_ZERO_BYTES_PAST_EOF, RarDecoder};
use ppmd_turbo::rc::{CarrylessRangeDecoder, RangeDecoder, SevenZipRangeDecoder, SliceInput};

use crate::outcome::{ErrKind, classify};
use crate::params::{INVALID_MEMS, INVALID_ORDERS, INVALID_RAR_MEM_MB, order_from};
use crate::payload::{Kind, generate};
use crate::reference::{encode_7z, encode_carryless};

/// The most operations one input runs.
pub const MAX_OPS: usize = 48;

/// The most RAR arena one operation asks for, in MiB.
pub const MAX_MEM_MB: u32 = 4;

/// The largest bare-model arena, in bytes.
pub const MAX_MODEL_MEM: u32 = 1 << 20;

/// The operation kinds, `kind % OP_KINDS`.
pub const OP_KINDS: u8 = 10;

/// A stream a fresh encoder wrote, with its parameters and payload.
#[derive(Debug)]
pub struct Known {
    /// Model order.
    pub order: u32,
    /// Arena in MiB (RAR) or bytes (7z).
    pub mem: u32,
    /// The decoded bytes.
    pub payload: Vec<u8>,
    /// The coded stream.
    pub stream: Vec<u8>,
}

/// The carry-less (RAR block) streams, with end markers.
pub fn known_rar() -> &'static [Known] {
    static K: OnceLock<Vec<Known>> = OnceLock::new();
    K.get_or_init(|| {
        [(2u32, 1u32), (6, 1), (16, 2), (64, 1)]
            .into_iter()
            .enumerate()
            .map(|(i, (order, mem_mb))| {
                let payload = generate(Kind::ALL[i % Kind::ALL.len()], 600 + i as u64, 1500);
                let stream = encode_carryless(&payload, order, mem_mb << 20, true);
                Known {
                    order,
                    mem: mem_mb,
                    payload,
                    stream,
                }
            })
            .collect()
    })
}

/// The 7z-coder streams, without end markers (the size is known).
pub fn known_7z() -> &'static [Known] {
    static K: OnceLock<Vec<Known>> = OnceLock::new();
    K.get_or_init(|| {
        [(2u32, 2048u32), (6, 1 << 16), (16, 1 << 20), (64, 4096)]
            .into_iter()
            .enumerate()
            .map(|(i, (order, mem))| {
                let payload = generate(Kind::ALL[(i + 2) % Kind::ALL.len()], 700 + i as u64, 1500);
                let stream = encode_7z(&payload, order, mem, false);
                Known {
                    order,
                    mem,
                    payload,
                    stream,
                }
            })
            .collect()
    })
}

/// A bare-model arena from a selector: 2 KiB to [`MAX_MODEL_MEM`].
fn model_mem(sel: u8) -> u32 {
    (2048u32 << (sel % 10)).min(MAX_MODEL_MEM)
}

fn pool_at(pool: &[u8], a: u8, b: u8) -> &[u8] {
    let off = usize::from(u16::from_le_bytes([a, b])) % (pool.len() + 1);
    &pool[off..]
}

/// Decodes up to `n` symbols; returns how many came out. A decode is only
/// ever corrupt (or, without a model, corrupt by definition).
fn drive<R: RangeDecoder>(
    mut step: impl FnMut(&mut R) -> ppmd_turbo::Result<Option<u8>>,
    rc: &mut R,
    zero_bytes: impl Fn(&R) -> u32,
    n: usize,
) -> usize {
    let mut decoded = 0;
    for _ in 0..n {
        match step(rc).map_err(|e| classify(&e)) {
            Ok(Some(_)) => decoded += 1,
            Ok(None) => {}
            Err(kind) => {
                assert_eq!(kind, ErrKind::Corrupt, "a symbol decode failed as {kind:?}");
                break;
            }
        }
        if zero_bytes(rc) > MAX_ZERO_BYTES_PAST_EOF {
            break;
        }
    }
    decoded
}

/// Decodes `n` symbols through the RAR decoder with either coder.
fn rar_symbols(rar: &mut RarDecoder, data: &[u8], sevenz: bool, n: usize) {
    let had_model = rar.has_model();
    if sevenz {
        let Ok(mut rc) = SevenZipRangeDecoder::<SliceInput<'_>>::new(data) else {
            return;
        };
        let decoded = drive(
            |rc| rar.decode_symbol(rc),
            &mut rc,
            |rc| rc.zero_bytes_past_eof(),
            n,
        );
        assert!(had_model || decoded == 0, "symbols without a model");
    } else {
        let Ok(mut rc) = CarrylessRangeDecoder::<SliceInput<'_>>::new(data) else {
            assert!(data.len() < 4, "a {}-byte coder start failed", data.len());
            return;
        };
        let decoded = drive(
            |rc| rar.decode_symbol(rc),
            &mut rc,
            |rc| rc.zero_bytes_past_eof(),
            n,
        );
        assert!(had_model || decoded == 0, "symbols without a model");
    }
}

/// Decodes `n` symbols through the bare model with either coder.
fn model_symbols(model: &mut Model, data: &[u8], sevenz: bool, n: usize) {
    if sevenz {
        if let Ok(mut rc) = SevenZipRangeDecoder::<SliceInput<'_>>::new(data) {
            drive(
                |rc| model.decode_symbol(rc),
                &mut rc,
                |rc| rc.zero_bytes_past_eof(),
                n,
            );
        }
    } else if let Ok(mut rc) = CarrylessRangeDecoder::<SliceInput<'_>>::new(data) {
        drive(
            |rc| model.decode_symbol(rc),
            &mut rc,
            |rc| rc.zero_bytes_past_eof(),
            n,
        );
    }
}

/// `known` through the RAR decoder after a reset block: its payload exactly.
fn verify_rar(rar: &mut RarDecoder, known: &Known, what: &str) {
    let mut out = Vec::new();
    let r = rar.decode_block(
        true,
        known.order,
        known.mem,
        &known.stream,
        known.payload.len() as u64 + 1,
        &mut out,
    );
    let consumed = r.unwrap_or_else(|e| panic!("{what}: a fresh stream failed: {e:?}"));
    assert!(consumed <= known.stream.len());
    assert!(
        out == known.payload,
        "{what}: {} bytes out of {}, first difference at {:?}",
        out.len(),
        known.payload.len(),
        out.iter().zip(&known.payload).position(|(x, y)| x != y)
    );
}

/// `known` through the bare model from its current state: its payload
/// exactly.
fn verify_model(model: &mut Model, known: &Known, what: &str) {
    let mut rc = SevenZipRangeDecoder::<SliceInput<'_>>::new(&known.stream[..])
        .expect("a fresh stream starts");
    for (i, &want) in known.payload.iter().enumerate() {
        match model.decode_symbol(&mut rc) {
            Ok(Some(got)) => assert_eq!(got, want, "{what}: symbol {i} differs"),
            other => panic!("{what}: symbol {i} was {other:?}"),
        }
    }
    assert!(!rc.faulted(), "{what}");
}

/// Runs one F6 input.
pub fn run(data: &[u8]) {
    let Some((&n_ops, rest)) = data.split_first() else {
        return;
    };
    let n_ops = usize::from(n_ops).min(MAX_OPS).min(rest.len() / 4);
    let (ops, pool) = rest.split_at(n_ops * 4);

    let mut rar = RarDecoder::new();
    let mut model = Model::new(6, 1 << 16).expect("legal parameters");
    let mut out = Vec::new();

    for (i, op) in ops.chunks_exact(4).enumerate() {
        let (kind, a, b, c) = (op[0] % OP_KINDS, op[1], op[2], op[3]);
        let what = format!("op {i} ({kind} {a} {b} {c})");
        match kind {
            // RAR's reset header, legal or not.
            0 => {
                let before = rar.mem_size();
                if c & 0x80 != 0 {
                    let order = INVALID_ORDERS[usize::from(a & 3)];
                    let mem_mb = INVALID_RAR_MEM_MB[usize::from(b) % INVALID_RAR_MEM_MB.len()];
                    let (order, mem_mb) = if c & 1 != 0 { (6, mem_mb) } else { (order, 1) };
                    let r = rar.init_model(order, mem_mb).map_err(|e| classify(&e));
                    assert_eq!(r, Err(ErrKind::InvalidParameters), "{what}");
                    assert_eq!(
                        rar.mem_size(),
                        before,
                        "{what}: a refused init changed the model"
                    );
                } else {
                    let mem_mb = 1 + u32::from(b) % MAX_MEM_MB;
                    rar.init_model(order_from(a), mem_mb)
                        .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                    assert_eq!(rar.mem_size(), Some(mem_mb << 20), "{what}");
                }
            }
            1 => {
                rar.reset();
                assert!(!rar.has_model());
            }
            2 => {
                rar.cleanup();
                assert_eq!(rar.mem_size(), Some(1 << 20), "{what}");
            }
            3 => rar_symbols(
                &mut rar,
                pool_at(pool, a, b),
                c & 1 != 0,
                usize::from(c) * 4,
            ),
            4 => {
                out.clear();
                let rc = pool_at(pool, a, b);
                let remaining = u64::from(c) * 16;
                let had_model = rar.has_model();
                match rar.decode_block(
                    c & 1 != 0,
                    order_from(a),
                    1 + u32::from(b) % MAX_MEM_MB,
                    rc,
                    remaining,
                    &mut out,
                ) {
                    Ok(consumed) => {
                        assert!(consumed <= rc.len(), "{what}: consumed {consumed}");
                        assert!(out.len() as u64 <= remaining, "{what}");
                        assert!(
                            had_model || c & 1 != 0 || rc.is_empty(),
                            "{what}: a block decoded with no model"
                        );
                    }
                    Err(e) => {
                        let kind = classify(&e);
                        assert!(
                            matches!(kind, ErrKind::Corrupt | ErrKind::Truncated),
                            "{what}: {kind:?}"
                        );
                    }
                }
            }
            // Dirty the RAR model, re-init it (reusing the arena when the
            // size matches), and a fresh stream must decode exactly.
            5 => {
                let known = &known_rar()[usize::from(a) % known_rar().len()];
                rar_symbols(&mut rar, pool_at(pool, b, c), c & 1 != 0, 256);
                verify_rar(&mut rar, known, &what);
            }
            6 => {
                let r = model.start(order_from(a), model_mem(b));
                r.unwrap_or_else(|e| panic!("{what}: {e:?}"));
                assert_eq!(model.mem_size(), model_mem(b), "{what}");
            }
            7 => {
                let order = INVALID_ORDERS[usize::from(a & 3)];
                let mem = INVALID_MEMS[usize::from(b & 3)];
                let (order, mem) = match c % 3 {
                    0 => (order, 1 << 16),
                    1 => (6, mem),
                    _ => (order, mem),
                };
                let before = (model.order(), model.mem_size());
                let r = model.start(order, mem).map_err(|e| classify(&e));
                assert_eq!(r, Err(ErrKind::InvalidParameters), "{what}");
                assert_eq!((model.order(), model.mem_size()), before, "{what}");
                assert_eq!(
                    Model::new(order, mem).map_err(|e| classify(&e)).err(),
                    Some(ErrKind::InvalidParameters),
                    "{what}"
                );
            }
            8 => model_symbols(
                &mut model,
                pool_at(pool, a, b),
                c & 1 != 0,
                usize::from(c) * 4,
            ),
            // Start the bare model with a known stream's parameters, dirty
            // it, and `restart` must bring it back to a fresh model.
            _ => {
                let known = &known_7z()[usize::from(a) % known_7z().len()];
                model
                    .start(known.order, known.mem)
                    .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                model_symbols(&mut model, pool_at(pool, b, c), c & 1 != 0, 256);
                model.restart();
                verify_model(&mut model, known, &what);
            }
        }
    }
}

/// Encodes operations and a pool as an F6 input, for the seeds.
pub fn seed(ops: &[[u8; 4]], pool: &[u8]) -> Vec<u8> {
    assert!(ops.len() <= MAX_OPS);
    let mut v = vec![ops.len() as u8];
    for op in ops {
        v.extend_from_slice(op);
    }
    v.extend_from_slice(pool);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplitMix64;

    #[test]
    fn known_streams_verify_on_fresh_models() {
        for known in known_rar() {
            verify_rar(&mut RarDecoder::new(), known, "fresh RAR");
        }
        for known in known_7z() {
            let mut model = Model::new(known.order, known.mem).unwrap();
            verify_model(&mut model, known, "fresh model");
        }
    }

    #[test]
    fn every_operation_runs() {
        let mut rng = SplitMix64::new(0xF6);
        let pool = rng.bytes(4096);
        for kind in 0..OP_KINDS {
            for c in [0u8, 1, 0x81, 0xFF] {
                let ops = [[0, 4, 0, 0], [kind, 7, 3, c], [kind, 200, 9, c]];
                run(&seed(&ops, &pool));
            }
        }
    }
}
