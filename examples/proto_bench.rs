//! Prototype measurement driver for model layout experiments (backlog D11).
//!
//! ```text
//! proto_bench encode PAYLOAD ORDER MEM OUT     # ppmd-rust 7z stream, no end marker
//! proto_bench decode STREAM ORDER MEM SIZE CRC # Model + SevenZipRangeDecoder, in memory
//! ```
//!
//! `decode` prints one line: in-process seconds, output CRC-32 and whether it
//! matched `CRC` (hex). It exits 1 on a mismatch, so a timing run can never
//! time wrong output.

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use ppmd_turbo::model::Model;
use ppmd_turbo::rc::SevenZipRangeDecoder;

fn arg<T: std::str::FromStr>(v: Option<String>, what: &str) -> T {
    v.and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad {what}"))
}

fn main() -> ExitCode {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().expect("command");
    match cmd.as_str() {
        "encode" => {
            let payload = std::fs::read(it.next().expect("payload")).expect("read payload");
            let order: u32 = arg(it.next(), "order");
            let mem: u32 = arg(it.next(), "mem");
            let out = it.next().expect("out");
            let mut enc = ppmd_rust::Ppmd7Encoder::new(Vec::new(), order, mem).expect("encoder");
            enc.write_all(&payload).expect("encode");
            let bytes = enc.finish(false).expect("finish");
            std::fs::write(&out, &bytes).expect("write");
            println!(
                "{out}: {} -> {} crc {:08x}",
                payload.len(),
                bytes.len(),
                crc32fast::hash(&payload)
            );
            ExitCode::SUCCESS
        }
        "decode" => {
            let stream = std::fs::read(it.next().expect("stream")).expect("read stream");
            let order: u32 = arg(it.next(), "order");
            let mem: u32 = arg(it.next(), "mem");
            let size: usize = arg(it.next(), "size");
            let want = u32::from_str_radix(&it.next().expect("crc"), 16).expect("crc hex");
            let mut out = Vec::with_capacity(size);
            let start = Instant::now();
            let mut model = Model::new(order, mem).expect("model");
            let mut rc = SevenZipRangeDecoder::new(&stream[..]).expect("coder");
            while out.len() < size {
                match model.decode_symbol(&mut rc) {
                    Ok(Some(b)) => out.push(b),
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("decode error after {} bytes: {e}", out.len());
                        return ExitCode::FAILURE;
                    }
                }
            }
            let secs = start.elapsed().as_secs_f64();
            let crc = crc32fast::hash(&out);
            println!(
                "{secs:.6} {crc:08x} {}",
                if crc == want { "ok" } else { "MISMATCH" }
            );
            if crc == want && out.len() == size {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "encode-turbo" => {
            // proto_bench encode-turbo PAYLOAD ORDER MEM REFERENCE_STREAM
            let payload = std::fs::read(it.next().expect("payload")).expect("read payload");
            let order: u32 = arg(it.next(), "order");
            let mem: u32 = arg(it.next(), "mem");
            let reference = std::fs::read(it.next().expect("reference")).expect("read reference");
            let start = Instant::now();
            let out = ppmd_turbo::ppmd7::encode_7z(&payload, order, mem, false).expect("encode");
            let secs = start.elapsed().as_secs_f64();
            let crc = crc32fast::hash(&out);
            let same = out == reference;
            println!(
                "{secs:.6} {crc:08x} {}",
                if same { "ok" } else { "MISMATCH" }
            );
            if same {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => panic!("unknown command {cmd}"),
    }
}
