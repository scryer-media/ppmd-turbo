//! `ppmd-bench`: one decode or encode per process, for the Go harness in
//! `bench/ppmd-turbo-bench` to time from outside.
//!
//! ```text
//! ppmd-bench decode-7z  --impl ppmd-turbo|ppmd-rust --in FILE [--order N --mem M [--size LEN]] [--out FILE]
//! ppmd-bench decode-rar --impl ppmd-turbo|ppmd-rust --in VOLUME [--in VOLUME ...] [--member N] [--out FILE]
//! ppmd-bench encode-7z  --impl ppmd-turbo|ppmd-rust --in FILE --order N --mem M [--end-marker] [--out FILE]
//! ppmd-bench info
//! ```
//!
//! `decode-7z` takes a `.7z` archive holding one PPMd-compressed file (the
//! stream and its parameters are read from the header) or a raw stream with
//! `--order` and `--mem`, decoded to `--size` bytes or, without it, to the
//! end marker. `decode-rar` takes a RAR 2.9-4.x volume set and decodes the
//! first (or `--member`) member, whose packed data must be one PPMd block,
//! through the RAR3 escape layer. `encode-7z` writes a raw 7z-coder stream.
//! The carry-less encoder is correctness-only and is never benchmarked, so
//! there is no `encode-rar`.
//!
//! Output goes to `--out`, or is only checksummed. On success one JSON line
//! goes to stdout: the operation, implementation, `bytes_in`, `bytes_out`,
//! `crc32` of the output, `inproc_seconds` (the codec work alone, input
//! already in memory), `peak_alloc_bytes` (the high-water mark of live heap
//! during that work) and the process's own `maxrss_bytes`, `user_seconds`
//! and `sys_seconds` where the platform reports them.
//!
//! Exit status: 0 success, 1 the input failed to decode or encode, 2 usage,
//! 3 the implementation does not provide the operation yet.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ppmd_corpus::rar::{self, Stop, Unescaper};
use ppmd_corpus::sevenz;
use ppmd_rust::{Ppmd7Decoder, Ppmd7Encoder, Ppmd7aDecoder};
use serde_json::json;

mod turbo;

const USAGE: &str = "usage:
  ppmd-bench decode-7z  --impl ppmd-turbo|ppmd-rust --in FILE [--order N --mem M [--size LEN]] [--out FILE]
  ppmd-bench decode-rar --impl ppmd-turbo|ppmd-rust --in VOLUME [--in VOLUME ...] [--member N] [--out FILE]
  ppmd-bench encode-7z  --impl ppmd-turbo|ppmd-rust --in FILE --order N --mem M [--end-marker] [--out FILE]
  ppmd-bench info";

const CHUNK: usize = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Impl {
    Turbo,
    Rust,
}

impl Impl {
    fn name(self) -> &'static str {
        match self {
            Impl::Turbo => "ppmd-turbo",
            Impl::Rust => "ppmd-rust",
        }
    }
}

struct Args {
    command: String,
    imp: Option<Impl>,
    inputs: Vec<PathBuf>,
    out: Option<PathBuf>,
    order: Option<u32>,
    mem: Option<u32>,
    size: Option<u64>,
    member: usize,
    end_marker: bool,
}

/// How a run failed, and the exit status that says so.
enum Failure {
    Usage(String),
    Codec(String),
    NotImplemented(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Codec(e)
    }
}

impl From<&str> for Failure {
    fn from(e: &str) -> Self {
        Failure::Codec(e.into())
    }
}

fn usage(e: impl Into<String>) -> Failure {
    Failure::Usage(e.into())
}

fn parse_number(flag: &str, v: &str) -> Result<u64, Failure> {
    let (digits, scale) = match v.as_bytes().last() {
        Some(b'k' | b'K') => (&v[..v.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&v[..v.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&v[..v.len() - 1], 1 << 30),
        _ => (v, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| usage(format!("{flag}: not a size: {v}")))
}

fn parse() -> Result<Args, Failure> {
    let mut it = std::env::args().skip(1);
    let command = it.next().ok_or_else(|| usage("missing command"))?;
    let mut a = Args {
        command,
        imp: None,
        inputs: Vec::new(),
        out: None,
        order: None,
        mem: None,
        size: None,
        member: 0,
        end_marker: false,
    };
    while let Some(flag) = it.next() {
        if flag == "--end-marker" {
            a.end_marker = true;
            continue;
        }
        if flag == "-h" || flag == "--help" {
            return Err(usage(""));
        }
        let v = it
            .next()
            .ok_or_else(|| usage(format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--impl" => {
                a.imp = Some(match v.as_str() {
                    "ppmd-turbo" => Impl::Turbo,
                    "ppmd-rust" => Impl::Rust,
                    other => return Err(usage(format!("unknown implementation {other}"))),
                })
            }
            "--in" => a.inputs.push(v.into()),
            "--out" => a.out = Some(v.into()),
            "--order" => {
                a.order = Some(
                    u32::try_from(parse_number(&flag, &v)?)
                        .map_err(|_| usage("--order out of range"))?,
                )
            }
            "--mem" => {
                a.mem = Some(
                    u32::try_from(parse_number(&flag, &v)?)
                        .map_err(|_| usage("--mem out of range"))?,
                )
            }
            "--size" => a.size = Some(parse_number(&flag, &v)?),
            "--member" => {
                a.member = usize::try_from(parse_number(&flag, &v)?)
                    .map_err(|_| usage("--member out of range"))?
            }
            other => return Err(usage(format!("unknown flag {other}"))),
        }
    }
    Ok(a)
}

/// Where decoded or encoded bytes go: counted and checksummed, and written
/// to `--out` when given.
struct Sink {
    file: Option<BufWriter<File>>,
    crc: crc32fast::Hasher,
    len: u64,
}

impl Sink {
    fn new(out: Option<&PathBuf>) -> Result<Self, String> {
        let file = match out {
            Some(p) => Some(BufWriter::with_capacity(
                CHUNK,
                File::create(p).map_err(|e| format!("{}: {e}", p.display()))?,
            )),
            None => None,
        };
        Ok(Self {
            file,
            crc: crc32fast::Hasher::new(),
            len: 0,
        })
    }

    fn finish(self) -> Result<(u64, u32), String> {
        if let Some(mut f) = self.file {
            f.flush().map_err(|e| e.to_string())?;
        }
        Ok((self.len, self.crc.finalize()))
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(f) = &mut self.file {
            f.write_all(buf)?;
        }
        self.crc.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_file(p: &PathBuf) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn one_input(a: &Args) -> Result<&PathBuf, Failure> {
    match a.inputs.as_slice() {
        [p] => Ok(p),
        [] => Err(usage("--in is required")),
        _ => Err(usage("exactly one --in")),
    }
}

/// What a shot measured, before the process figures are added.
struct Shot {
    bytes_in: u64,
    bytes_out: u64,
    crc32: u32,
    seconds: f64,
    peak_alloc: u64,
    extra: serde_json::Value,
}

/// Copies at most `limit` bytes (all, if `None`) from `r` to `sink`.
fn pump(r: &mut dyn Read, sink: &mut Sink, limit: Option<u64>) -> Result<(), String> {
    let mut buf = vec![0u8; CHUNK];
    let mut left = limit;
    loop {
        let want = match left {
            Some(0) => return Ok(()),
            Some(n) => n.min(CHUNK as u64) as usize,
            None => CHUNK,
        };
        let got = r.read(&mut buf[..want]).map_err(|e| e.to_string())?;
        if got == 0 {
            return match left {
                Some(n) => Err(format!("the stream ended {n} bytes short")),
                None => Ok(()),
            };
        }
        sink.write_all(&buf[..got]).map_err(|e| e.to_string())?;
        if let Some(n) = &mut left {
            *n -= got as u64;
        }
    }
}

fn decode_7z(a: &Args, imp: Impl) -> Result<Shot, Failure> {
    let path = one_input(a)?;
    let data = read_file(path)?;
    let (stream, order, mem, size, container) = match sevenz::pack_stream(&data) {
        Some(ps) => (
            sevenz::stream_bytes(&data, &ps),
            ps.order,
            ps.mem_size,
            Some(ps.unpacked_len),
            true,
        ),
        None if data.starts_with(b"7z\xBC\xAF\x27\x1C") => {
            return Err(Failure::Codec(
                "not a single-file PPMd 7z archive with a plain header".into(),
            ));
        }
        None => (
            &data[..],
            a.order.ok_or_else(|| usage("a raw stream needs --order"))?,
            a.mem.ok_or_else(|| usage("a raw stream needs --mem"))?,
            a.size,
            false,
        ),
    };
    let mut sink = Sink::new(a.out.as_ref())?;
    let base = alloc_watch_reset();
    let start = Instant::now();
    match imp {
        Impl::Rust => {
            let mut dec =
                Ppmd7Decoder::new(stream, order, mem).map_err(|e| format!("ppmd-rust: {e:?}"))?;
            pump(&mut dec, &mut sink, size)?;
        }
        Impl::Turbo => turbo::decode_7z(stream, order, mem, size, &mut sink)?,
    }
    let seconds = start.elapsed().as_secs_f64();
    let peak_alloc = alloc_watch_peak(base);
    let (bytes_out, crc32) = sink.finish()?;
    Ok(Shot {
        bytes_in: stream.len() as u64,
        bytes_out,
        crc32,
        seconds,
        peak_alloc,
        extra: json!({"order": order, "mem": mem, "container": container}),
    })
}

fn decode_rar(a: &Args, imp: Impl) -> Result<Shot, Failure> {
    if a.inputs.is_empty() {
        return Err(usage("--in is required"));
    }
    let volumes = a
        .inputs
        .iter()
        .map(read_file)
        .collect::<Result<Vec<_>, _>>()?;
    let views: Vec<&[u8]> = volumes.iter().map(Vec::as_slice).collect();
    let mut members = rar::members(&views)?;
    if a.member >= members.len() {
        return Err(Failure::Codec(format!(
            "the archive has {} members",
            members.len()
        )));
    }
    let member = members.swap_remove(a.member);
    drop(volumes);
    if member.solid {
        return Err(Failure::Codec(
            "a solid continuation member needs the members before it".into(),
        ));
    }
    let header = rar::ppm_header(&member.packed)
        .ok_or("the member's packed data does not start with a PPMd block")?;
    if !header.reset {
        return Err(Failure::Codec(
            "the member's first PPMd block does not reset the model".into(),
        ));
    }
    let mem = header
        .mem_mb
        .checked_mul(1 << 20)
        .ok_or("memory size overflows")?;
    let rc = &member.packed[header.len..];
    let limit = usize::try_from(member.unpacked_len).map_err(|e| e.to_string())?;
    let esc = header.esc.unwrap_or(rar::DEFAULT_ESC);
    let mut sink = Sink::new(a.out.as_ref())?;
    let base = alloc_watch_reset();
    let start = Instant::now();
    let symbols = match imp {
        Impl::Rust => {
            let mut dec = Ppmd7aDecoder::new(rc, header.order, mem)
                .map_err(|e| format!("ppmd-rust: {e:?}"))?;
            let mut layer = Unescaper::new(esc, limit);
            let mut one = [0u8; 1];
            if limit > 0 {
                loop {
                    if dec.read(&mut one).map_err(|e| e.to_string())? == 0 {
                        return Err(Failure::Codec(format!(
                            "the PPMd stream ended after {} symbols",
                            layer.symbols
                        )));
                    }
                    match layer.push(one[0]) {
                        Ok(Some(Stop::Full | Stop::EndOfFile)) => break,
                        Ok(None) => {}
                        Err(e) => {
                            return Err(Failure::Codec(format!(
                                "escape layer: {e:?} after {} symbols",
                                layer.symbols
                            )));
                        }
                    }
                }
            }
            sink.write_all(&layer.out).map_err(|e| e.to_string())?;
            layer.symbols
        }
        Impl::Turbo => turbo::decode_rar(&header, rc, limit, &mut sink)?,
    };
    let seconds = start.elapsed().as_secs_f64();
    let peak_alloc = alloc_watch_peak(base);
    let (bytes_out, crc32) = sink.finish()?;
    if bytes_out != member.unpacked_len {
        return Err(Failure::Codec(format!(
            "{bytes_out} bytes, the header says {}",
            member.unpacked_len
        )));
    }
    if crc32 != member.crc32 {
        return Err(Failure::Codec(format!(
            "CRC-32 {crc32:08x}, the header says {:08x}",
            member.crc32
        )));
    }
    Ok(Shot {
        bytes_in: member.packed.len() as u64,
        bytes_out,
        crc32,
        seconds,
        peak_alloc,
        extra: json!({"member": member.name, "order": header.order, "mem": mem, "symbols": symbols}),
    })
}

fn encode_7z(a: &Args, imp: Impl) -> Result<Shot, Failure> {
    let path = one_input(a)?;
    let order = a.order.ok_or_else(|| usage("--order is required"))?;
    let mem = a.mem.ok_or_else(|| usage("--mem is required"))?;
    let data = read_file(path)?;
    let sink = Sink::new(a.out.as_ref())?;
    let base = alloc_watch_reset();
    let start = Instant::now();
    let sink = match imp {
        Impl::Rust => {
            let mut enc =
                Ppmd7Encoder::new(sink, order, mem).map_err(|e| format!("ppmd-rust: {e:?}"))?;
            for chunk in data.chunks(CHUNK) {
                enc.write_all(chunk).map_err(|e| e.to_string())?;
            }
            enc.finish(a.end_marker).map_err(|e| e.to_string())?
        }
        Impl::Turbo => turbo::encode_7z(&data, order, mem, a.end_marker, sink)?,
    };
    let seconds = start.elapsed().as_secs_f64();
    let peak_alloc = alloc_watch_peak(base);
    let (bytes_out, crc32) = sink.finish()?;
    Ok(Shot {
        bytes_in: data.len() as u64,
        bytes_out,
        crc32,
        seconds,
        peak_alloc,
        extra: json!({"order": order, "mem": mem, "end_marker": a.end_marker}),
    })
}

fn info() -> serde_json::Value {
    let ops = |imp: Impl| match imp {
        Impl::Rust => json!({"decode-7z": true, "decode-rar": true, "encode-7z": true}),
        Impl::Turbo => json!({
            "decode-7z": turbo::DECODE_7Z,
            "decode-rar": turbo::DECODE_RAR,
            "encode-7z": turbo::ENCODE_7Z,
        }),
    };
    json!({
        "tool": "ppmd-bench",
        "version": env!("CARGO_PKG_VERSION"),
        "ppmd_turbo_max_order": turbo::MAX_ORDER,
        "impls": {"ppmd-turbo": ops(Impl::Turbo), "ppmd-rust": ops(Impl::Rust)},
    })
}

fn run(a: &Args) -> Result<serde_json::Value, Failure> {
    if a.command == "info" {
        return Ok(info());
    }
    let imp = a.imp.ok_or_else(|| usage("--impl is required"))?;
    let shot = match a.command.as_str() {
        "decode-7z" => decode_7z(a, imp)?,
        "decode-rar" => decode_rar(a, imp)?,
        "encode-7z" => encode_7z(a, imp)?,
        other => return Err(usage(format!("unknown command {other}"))),
    };
    let usage = process_usage();
    Ok(json!({
        "op": a.command,
        "impl": imp.name(),
        "bytes_in": shot.bytes_in,
        "bytes_out": shot.bytes_out,
        "crc32": format!("{:08x}", shot.crc32),
        "inproc_seconds": shot.seconds,
        "peak_alloc_bytes": shot.peak_alloc,
        "maxrss_bytes": usage.map(|u| u.0),
        "user_seconds": usage.map(|u| u.1),
        "sys_seconds": usage.map(|u| u.2),
        "params": shot.extra,
    }))
}

fn main() -> ExitCode {
    let result = parse().and_then(|a| run(&a));
    match result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(e)) => {
            if !e.is_empty() {
                eprintln!("ppmd-bench: {e}");
            }
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Codec(e)) => {
            eprintln!("ppmd-bench: {e}");
            ExitCode::from(1)
        }
        Err(Failure::NotImplemented(e)) => {
            eprintln!("ppmd-bench: not implemented: {e}");
            ExitCode::from(3)
        }
    }
}

/// The process's own peak RSS in bytes and user/system CPU seconds.
#[cfg(unix)]
fn process_usage() -> Option<(u64, f64, f64)> {
    // SAFETY: getrusage writes one plain struct that we zero-initialise.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return None;
        }
        ru
    };
    // macOS reports ru_maxrss in bytes, Linux and the BSDs in KiB.
    let scale = if cfg!(target_vendor = "apple") {
        1
    } else {
        1024
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    Some((
        (ru.ru_maxrss as u64).saturating_mul(scale),
        secs(ru.ru_utime),
        secs(ru.ru_stime),
    ))
}

#[cfg(not(unix))]
fn process_usage() -> Option<(u64, f64, f64)> {
    None
}

// Allocation high-water mark, as in lzma-turbo's lzma-bench.

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Wraps the system allocator and tracks live bytes. The counters are
/// relaxed: a high-water mark for a report, not synchronisation.
struct Counting;

#[global_allocator]
static ALLOC: Counting = Counting;

fn note_alloc(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            note_alloc(l.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(p, l) };
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                note_alloc(new - l.size());
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

/// Starts a measurement: the live-bytes baseline, with the peak pulled down
/// to it.
fn alloc_watch_reset() -> usize {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    base
}

fn alloc_watch_peak(base: usize) -> u64 {
    PEAK.load(Ordering::Relaxed).saturating_sub(base) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_take_binary_suffixes() {
        assert_eq!(parse_number("--mem", "16m").ok(), Some(16 << 20));
        assert_eq!(parse_number("--mem", "64k").ok(), Some(64 << 10));
        assert_eq!(parse_number("--mem", "1g").ok(), Some(1 << 30));
        assert_eq!(parse_number("--size", "1000").ok(), Some(1000));
        assert!(parse_number("--size", "m").is_err());
        assert!(parse_number("--size", "-1").is_err());
    }

    #[test]
    fn pump_stops_at_the_limit_and_reports_short_streams() {
        let mut sink = Sink::new(None).unwrap();
        pump(&mut &[7u8; 100][..], &mut sink, Some(40)).unwrap();
        assert_eq!(sink.finish().unwrap().0, 40);
        let mut sink = Sink::new(None).unwrap();
        assert!(pump(&mut &[7u8; 10][..], &mut sink, Some(40)).is_err());
    }
}
