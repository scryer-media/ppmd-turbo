//! The ppmd-turbo side of the driver.
//!
//! Each operation reports itself unavailable until the crate provides it.
//! Wiring one in means calling the crate here and setting its flag, which
//! `ppmd-bench info` reports so the harness plans ppmd-turbo rows only for
//! operations that exist.

use crate::{Failure, Sink};
use ppmd_corpus::rar::PpmHeader;

/// The largest model order the crate accepts, as a link check.
pub const MAX_ORDER: u32 = ppmd_turbo::PPMD7_MAX_ORDER;
/// `decode-7z` is wired to the crate.
pub const DECODE_7Z: bool = false;
/// `decode-rar` is wired to the crate.
pub const DECODE_RAR: bool = false;
/// `encode-7z` is wired to the crate.
pub const ENCODE_7Z: bool = false;

fn missing(op: &str) -> Failure {
    Failure::NotImplemented(format!("ppmd-turbo has no {op} yet; use --impl ppmd-rust"))
}

pub fn decode_7z(
    _stream: &[u8],
    _order: u32,
    _mem: u32,
    _size: Option<u64>,
    _sink: &mut Sink,
) -> Result<(), Failure> {
    Err(missing("7z decoder"))
}

pub fn decode_rar(
    _header: &PpmHeader,
    _rc: &[u8],
    _limit: usize,
    _sink: &mut Sink,
) -> Result<u64, Failure> {
    Err(missing("RAR decoder"))
}

pub fn encode_7z(
    _data: &[u8],
    _order: u32,
    _mem: u32,
    _end_marker: bool,
    _sink: Sink,
) -> Result<Sink, Failure> {
    Err(missing("7z encoder"))
}
