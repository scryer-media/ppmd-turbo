//! `ppmd-oracle`: compares a PPMd codec with `7zz` and `unrar`.
//!
//! ```text
//! ppmd-oracle tools
//! ppmd-oracle check-7z [--codec reference|turbo] [--orders 2,6,16,32,64]
//!                      [--mems 2048,65536,1048576,16777216] [FILE...]
//! ppmd-oracle check-rar [--codec reference|turbo] ARCHIVE...
//! ```
//!
//! `check-7z` runs three checks for every input and parameter pair, using
//! the order and memory size 7-Zip actually wrote (it lowers memory for
//! small inputs):
//!
//! 1. encoder: the codec's stream is byte-identical to the stream `7zz` wrote;
//! 2. decoder: the codec decodes the stream `7zz` wrote to the input;
//! 3. container: `7zz` extracts the codec's stream wrapped in this tool's
//!    `.7z` writer back to the input.
//!
//! 7-Zip's encoder takes orders 2..=32 and arenas of at least 64 KiB; outside
//! that window only the container check runs, with the requested parameters.
//! Without files it uses a built-in invented corpus. It exits non-zero on
//! any mismatch. `check-rar` skips cleanly when `unrar` is not on `PATH`.

use std::path::PathBuf;
use std::process::ExitCode;

use ppmd_oracle::binaries::{SevenZip, Unrar};
use ppmd_oracle::codec::{Codec, CodecError};
use ppmd_oracle::corpus::default_corpus;
use ppmd_oracle::sevenz::{read_archive, write_archive};

const USAGE: &str = "usage:
  ppmd-oracle tools
  ppmd-oracle check-7z [--codec reference|turbo] [--orders LIST] [--mems LIST] [FILE...]
  ppmd-oracle check-rar [--codec reference|turbo] ARCHIVE...";

struct Args {
    codec: Codec,
    orders: Vec<u32>,
    mems: Vec<u32>,
    files: Vec<PathBuf>,
}

fn list(s: &str) -> Result<Vec<u32>, String> {
    s.split(',')
        .map(|v| v.trim().parse().map_err(|e| format!("{v:?}: {e}")))
        .collect()
}

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        codec: Codec::Turbo,
        orders: vec![2, 6, 16, 32, 64],
        mems: vec![2048, 1 << 16, 1 << 20, 16 << 20],
        files: Vec::new(),
    };
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--codec" => {
                let v = value()?;
                args.codec = Codec::parse(&v).ok_or_else(|| format!("unknown codec {v:?}"))?;
            }
            "--orders" => args.orders = list(&value()?)?,
            "--mems" => args.mems = list(&value()?)?,
            _ if a.starts_with("--") => return Err(format!("unknown option {a}")),
            _ => args.files.push(PathBuf::from(a)),
        }
    }
    Ok(args)
}

#[derive(Default)]
struct Tally {
    passed: u64,
    failed: u64,
    unavailable: u64,
}

impl Tally {
    fn record(&mut self, what: &str, result: Result<(), CodecError>) {
        match result {
            Ok(()) => self.passed += 1,
            Err(CodecError::Unavailable(why)) => {
                if self.unavailable == 0 {
                    eprintln!("skipped: {why}");
                }
                self.unavailable += 1;
            }
            Err(CodecError::Failed(e)) => {
                eprintln!("FAIL {what}: {e}");
                self.failed += 1;
            }
        }
    }
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    let at = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    format!(
        "lengths {} and {}, first difference at byte {at}",
        a.len(),
        b.len()
    )
}

/// 7-Zip's PPMd encoder accepts orders 2..=32 and arenas that are a multiple of 4 and at least
/// 64 KiB; its decoder takes the full 2..=64 range and any legal arena.
fn sevenzip_encodes(order: u32, mem: u32) -> bool {
    (2..=32).contains(&order) && mem >= 1 << 16 && mem.is_multiple_of(4)
}

/// Wraps the codec's stream in this tool's `.7z` and has `7zz` extract it.
fn container(
    seven: &SevenZip,
    work: &std::path::Path,
    codec: Codec,
    data: &[u8],
    order: u32,
    mem: u32,
) -> Result<(), CodecError> {
    let order_byte = u8::try_from(order).map_err(|e| CodecError::Failed(e.to_string()))?;
    let ours = codec.encode_7z(data, order, mem)?;
    let wrapped = write_archive(order_byte, mem, &ours, data, "oracle.bin")
        .map_err(|e| CodecError::Failed(e.to_string()))?;
    let out = seven
        .extract(work, &wrapped)
        .map_err(|e| CodecError::Failed(e.to_string()))?;
    if out == data {
        Ok(())
    } else {
        Err(CodecError::Failed(first_difference(&out, data)))
    }
}

fn check_7z(args: &Args) -> Result<Tally, String> {
    let seven = SevenZip::find().ok_or("7zz not found (set PPMD_ORACLE_7ZZ)")?;
    eprintln!(
        "7zz: {} ({})",
        seven.path.display(),
        seven.version().unwrap_or_default()
    );
    let work = std::env::temp_dir().join(format!("ppmd-oracle-{}", std::process::id()));
    let inputs = if args.files.is_empty() {
        default_corpus()
    } else {
        let mut v = Vec::new();
        for f in &args.files {
            let data = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
            v.push((f.display().to_string(), data));
        }
        v
    };
    let mut tally = Tally::default();
    for (name, data) in &inputs {
        if data.is_empty() {
            eprintln!("skip {name}: empty (7z stores no PPMd folder)");
            continue;
        }
        for &order in &args.orders {
            for &mem in &args.mems {
                let label = format!("{name} o={order} mem={mem}");
                if !sevenzip_encodes(order, mem) {
                    // Outside 7-Zip's encoder range: only 7-Zip's decoder
                    // can judge, through the container check.
                    tally.record(&format!("container {label}"), {
                        container(&seven, &work, args.codec, data, order, mem)
                    });
                    continue;
                }
                let archive = seven
                    .compress_ppmd(&work, "oracle-input.bin", data, order, mem)
                    .map_err(|e| format!("{label}: {e}"))?;
                let entry = read_archive(&archive).map_err(|e| format!("{label}: {e}"))?;
                let (o, m) = (u32::from(entry.order), entry.mem);
                let label = format!("{name} o={o} mem={m} (asked o={order} mem={mem})");

                tally.record(&format!("encoder {label}"), {
                    args.codec.encode_7z(data, o, m).and_then(|ours| {
                        if ours == entry.stream {
                            Ok(())
                        } else {
                            Err(CodecError::Failed(first_difference(&ours, &entry.stream)))
                        }
                    })
                });

                tally.record(&format!("decoder {label}"), {
                    args.codec
                        .decode_7z(&entry.stream, o, m, entry.unpack_size)
                        .and_then(|out| {
                            if &out == data {
                                Ok(())
                            } else {
                                Err(CodecError::Failed(first_difference(&out, data)))
                            }
                        })
                });

                tally.record(&format!("container {label}"), {
                    container(&seven, &work, args.codec, data, o, m)
                });
            }
        }
    }
    let _ = std::fs::remove_dir(&work);
    Ok(tally)
}

fn check_rar(args: &Args) -> Result<Tally, String> {
    let mut tally = Tally::default();
    let Some(unrar) = Unrar::find() else {
        eprintln!("unrar not on PATH (or PPMD_ORACLE_UNRAR); RAR checks skipped");
        return Ok(tally);
    };
    if args.files.is_empty() {
        return Err("check-rar needs at least one archive".into());
    }
    for archive in &args.files {
        let label = archive.display().to_string();
        match unrar.test(archive) {
            Ok(()) => tally.passed += 1,
            Err(e) => {
                eprintln!("FAIL unrar t {label}: {e}");
                tally.failed += 1;
                continue;
            }
        }
        let printed = match unrar.print(archive) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("FAIL unrar p {label}: {e}");
                tally.failed += 1;
                continue;
            }
        };
        let members = match std::fs::read(archive)
            .map_err(|e| e.to_string())
            .and_then(|data| ppmd_corpus::rar::members(&[&data]))
        {
            Ok(m) => m,
            Err(e) => {
                tally.record(
                    &format!("parse {label}"),
                    Err(CodecError::Failed(format!("{e} (one volume per archive)"))),
                );
                continue;
            }
        };
        // `unrar p` prints the members back to back, in archive order.
        let mut offset = 0usize;
        for member in &members {
            let what = format!("decoder {label}:{}", member.name);
            let len = member.unpacked_len as usize;
            let want = printed.get(offset..offset + len);
            offset += len;
            if member.solid || member.method == 0x30 {
                tally.record(
                    &what,
                    Err(CodecError::Unavailable(
                        "a stored or solid-continuation member",
                    )),
                );
                continue;
            }
            let result = args
                .codec
                .decode_rar_member(&member.packed, member.unpacked_len)
                .and_then(|ours| match want {
                    Some(want) if want == ours => Ok(()),
                    Some(want) => Err(CodecError::Failed(first_difference(want, &ours))),
                    None => Err(CodecError::Failed("unrar printed fewer bytes".into())),
                });
            tally.record(&what, result);
        }
    }
    Ok(tally)
}

fn main() -> ExitCode {
    let mut it = std::env::args().skip(1);
    let command = it.next().unwrap_or_default();
    let args = match parse(it) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match command.as_str() {
        "tools" => {
            match SevenZip::find() {
                Some(s) => println!(
                    "7zz: {} ({})",
                    s.path.display(),
                    s.version().unwrap_or_default()
                ),
                None => println!("7zz: not found"),
            }
            match Unrar::find() {
                Some(u) => println!("unrar: {}", u.path.display()),
                None => println!("unrar: not found"),
            }
            return ExitCode::SUCCESS;
        }
        "check-7z" => check_7z(&args),
        "check-rar" => check_rar(&args),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(t) => {
            println!(
                "{} codec: {} passed, {} failed, {} unavailable",
                args.codec, t.passed, t.failed, t.unavailable
            );
            if t.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
