//! Building the corpora: the small committed conformance corpus under
//! `tests/fixtures/`, and the larger benchmark corpora the Go harness
//! generates on demand under `bench/fixtures/<profile>/`.
//!
//! Every stream is checked before it is written: ppmd-rust must decode it
//! back to its payload, and for each `7zz` stream ppmd-rust's encoder is run
//! at the container's own order and memory size and compared byte for byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::oracle::{self, Coder};
use crate::payload::{self, Kind};
use crate::{rar, sevenz};

/// Schema tag of `tests/fixtures/manifest.json`.
pub const CONFORMANCE_SCHEMA: &str = "ppmd-turbo-corpus/conformance/1";
/// Schema tag of `bench/fixtures/<profile>/manifest.json`.
pub const BENCH_SCHEMA: &str = "ppmd-turbo-corpus/bench/1";

const KIB: u32 = 1 << 10;
const MIB: u32 = 1 << 20;
const GIB: u32 = 1 << 30;

/// The 7-Zip binary that writes the `7zz` streams.
pub struct SevenZip {
    /// Path to `7zz`.
    pub path: PathBuf,
    /// Its banner line.
    pub banner: String,
    /// The release in the banner, e.g. `26.01`.
    pub version: String,
}

impl SevenZip {
    /// Probes `path` (or `7zz` on `PATH`).
    pub fn probe(path: Option<&Path>) -> Result<Self, String> {
        let path = path.map_or_else(|| PathBuf::from("7zz"), Path::to_path_buf);
        let output = Command::new(&path).output().map_err(|e| {
            format!(
                "{}: {e} (install 7-Zip's 7zz or pass --sevenzip)",
                path.display()
            )
        })?;
        let text = String::from_utf8_lossy(&output.stdout);
        let banner = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("7-Zip"))
            .ok_or_else(|| format!("{}: no 7-Zip banner", path.display()))?
            .to_string();
        let version = banner
            .split_whitespace()
            .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .to_string();
        Ok(Self {
            path,
            banner,
            version,
        })
    }

    /// Writes `payload` as a one-file `.7z` with `-m0=PPMd:o=<order>:mem=<mem>b`
    /// and an uncompressed header, and returns the container's bytes.
    pub fn compress(
        &self,
        scratch: &Path,
        name: &str,
        payload: &[u8],
        order: u32,
        mem: u32,
    ) -> Result<Vec<u8>, String> {
        fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let input = scratch.join(name);
        fs::write(&input, payload).map_err(|e| e.to_string())?;
        let archive = scratch.join(format!("{name}.7z"));
        let _ = fs::remove_file(&archive);
        let status = Command::new(&self.path)
            .current_dir(scratch)
            .args([
                "a", "-t7z", "-bso0", "-bsp0", "-mhc=off", "-mtm=off", "-mtc=off", "-mta=off",
            ])
            .arg(format!("-m0=PPMd:o={order}:mem={mem}b"))
            .arg(archive.file_name().unwrap())
            .arg(name)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("7zz a {name} o={order} mem={mem}: {status}"));
        }
        let data = fs::read(&archive).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_file(&input);
        Ok(data)
    }
}

/// A short label for a memory size: `2k`, `16m`, `1g`.
pub fn mem_label(mem: u32) -> String {
    if mem >= GIB && mem.is_multiple_of(GIB) {
        format!("{}g", mem / GIB)
    } else {
        payload::size_label(mem as usize)
    }
}

fn stem(kind: Kind, size: usize) -> String {
    format!("{}-{}", kind.name(), payload::size_label(size))
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

fn coder_name(coder: Coder) -> &'static str {
    match coder {
        Coder::SevenZ => "7z",
        Coder::CarryLess => "carry-less",
    }
}

/// The corruption variants the conformance suite derives from a stream. The
/// suite applies the same operations to the committed bytes, so only the
/// recipe and ppmd-rust's outcome are recorded, never a copy.
pub fn corrupt(op: &str, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    match op {
        "truncate-half" => v.truncate(data.len() / 2),
        "flip-first" => v[0] ^= 0x80,
        "flip-third" => v[data.len() / 3] ^= 0x55,
        "flip-mid" => v[data.len() / 2] ^= 0xFF,
        other => panic!("unknown corruption {other}"),
    }
    v
}

/// What ppmd-rust made of a stream, for the manifest.
fn outcome(result: &Result<Vec<u8>, String>) -> Value {
    match result {
        Ok(out) => json!({"ok": true, "len": out.len(), "sha256": oracle::sha256_hex(out)}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// State while building the conformance corpus.
struct Conformance<'a> {
    dir: &'a Path,
    scratch: PathBuf,
    sevenzip: &'a SevenZip,
    payloads: Vec<Value>,
    seen_payloads: Vec<String>,
    streams: Vec<Value>,
    corruptions: Vec<Value>,
}

impl Conformance<'_> {
    fn payload(&mut self, kind: Kind, size: usize) -> Vec<u8> {
        let data = payload::generate(kind, size);
        let name = payload::file_name(kind, size);
        if !self.seen_payloads.contains(&name) {
            self.seen_payloads.push(name.clone());
            self.payloads.push(json!({
                "name": name, "kind": kind.name(), "size": size,
                "seed": format!("{:016x}", payload::seed(kind, size)),
                "sha256": oracle::sha256_hex(&data), "crc32": oracle::crc32_hex(&data),
            }));
        }
        data
    }

    fn has_stream(&self, name: &str) -> bool {
        self.streams.iter().any(|s| s["name"] == name)
    }

    /// A ppmd-rust stream.
    fn rust_stream(
        &mut self,
        coder: Coder,
        kind: Kind,
        size: usize,
        order: u32,
        mem: u32,
        eos: bool,
    ) -> Result<String, String> {
        let data = self.payload(kind, size);
        let dir = if coder == Coder::SevenZ { "7z" } else { "rar" };
        let ext = if coder == Coder::SevenZ {
            "ppmd"
        } else {
            "ppmd7a"
        };
        let name = format!(
            "{}.o{order}.m{}.ppmd-rust{}.{ext}",
            stem(kind, size),
            mem_label(mem),
            if eos { ".eos" } else { "" }
        );
        if self.has_stream(&name) {
            return Ok(name);
        }
        let stream = oracle::encode(coder, &data, order, mem, eos)?;
        let len = (!eos).then_some(data.len() as u64);
        let back = oracle::decode(coder, &stream, order, mem, len)?;
        if back != data {
            return Err(format!("{name}: ppmd-rust does not decode its own stream"));
        }
        let file = format!("{dir}/{name}");
        write(&self.dir.join(&file), &stream)?;
        self.streams.push(json!({
            "name": name, "file": file, "coder": coder_name(coder),
            "payload": payload::file_name(kind, size), "payload_len": size,
            "order": order, "mem": mem, "end_marker": eos, "producer": "ppmd-rust",
            "stream_len": stream.len(), "stream_sha256": oracle::sha256_hex(&stream),
        }));
        Ok(name)
    }

    /// A `7zz` stream: written into a `.7z`, then pulled out of it.
    fn sevenzip_stream(
        &mut self,
        kind: Kind,
        size: usize,
        order: u32,
        mem: u32,
    ) -> Result<String, String> {
        let data = self.payload(kind, size);
        let input = payload::file_name(kind, size);
        let archive = self
            .sevenzip
            .compress(&self.scratch, &input, &data, order, mem)?;
        let ps = sevenz::pack_stream(&archive)
            .ok_or_else(|| format!("{input}: 7zz wrote a shape the walk does not read"))?;
        if ps.unpacked_len != size as u64 {
            return Err(format!("{input}: container says {} bytes", ps.unpacked_len));
        }
        let stream = sevenz::stream_bytes(&archive, &ps).to_vec();
        let name = format!(
            "{}.o{}.m{}.7zz.ppmd",
            stem(kind, size),
            ps.order,
            mem_label(ps.mem_size)
        );
        if self.has_stream(&name) {
            return Ok(name);
        }
        let back = oracle::decode(
            Coder::SevenZ,
            &stream,
            ps.order,
            ps.mem_size,
            Some(size as u64),
        )?;
        if back != data {
            return Err(format!(
                "{name}: ppmd-rust does not decode 7zz's stream to the payload"
            ));
        }
        let rust = oracle::encode(Coder::SevenZ, &data, ps.order, ps.mem_size, false)?;
        let file = format!("7z/{name}");
        write(&self.dir.join(&file), &stream)?;
        self.streams.push(json!({
            "name": name, "file": file, "coder": "7z",
            "payload": input, "payload_len": size,
            "order": ps.order, "mem": ps.mem_size, "end_marker": false, "producer": "7zz",
            "stream_len": stream.len(), "stream_sha256": oracle::sha256_hex(&stream),
            "requested": {"order": order, "mem": mem},
            "container": {"len": archive.len(), "sha256": oracle::sha256_hex(&archive)},
            "ppmd_rust_encoder_identical": rust == stream,
        }));
        Ok(name)
    }

    /// Records the corruption variants of one stream with ppmd-rust's outcome.
    fn corruptions_of(&mut self, base: &str, ops: &[&str]) -> Result<(), String> {
        let entry = self
            .streams
            .iter()
            .find(|s| s["name"] == base)
            .ok_or_else(|| format!("no stream {base}"))?
            .clone();
        let data =
            fs::read(self.dir.join(entry["file"].as_str().unwrap())).map_err(|e| e.to_string())?;
        let coder = if entry["coder"] == "7z" {
            Coder::SevenZ
        } else {
            Coder::CarryLess
        };
        let order = entry["order"].as_u64().unwrap() as u32;
        let mem = entry["mem"].as_u64().unwrap() as u32;
        let len = entry["payload_len"].as_u64().unwrap();
        let payload_sha = self
            .payloads
            .iter()
            .find(|p| p["name"] == entry["payload"])
            .unwrap()["sha256"]
            .clone();
        for op in ops {
            let bad = corrupt(op, &data);
            let result = oracle::decode(coder, &bad, order, mem, Some(len));
            let expect =
                if *op == "truncate-half" || (*op == "flip-first" && coder == Coder::SevenZ) {
                    if result.is_ok() {
                        return Err(format!(
                            "{base} {op}: ppmd-rust accepted it; the expectation is wrong"
                        ));
                    }
                    "error"
                } else {
                    if matches!(&result, Ok(out) if oracle::sha256_hex(out) == payload_sha) {
                        return Err(format!(
                            "{base} {op}: ppmd-rust still decodes the payload; pick another offset"
                        ));
                    }
                    "not-payload"
                };
            self.corruptions.push(json!({
                "name": format!("{base}+{op}"), "base": base, "op": op, "expect": expect,
                "ppmd_rust": outcome(&result),
            }));
        }
        Ok(())
    }
}

/// Generates the committed conformance corpus into `dir` (normally
/// `tests/fixtures`). `rar_source` is unrar-rs's `tests/fixtures/rar4`
/// directory, which holds the RARLAB-written archives the RAR members come
/// from.
pub fn conformance(dir: &Path, sevenzip: &SevenZip, rar_source: &Path) -> Result<Value, String> {
    let scratch = std::env::temp_dir().join(format!("ppmd-corpus-{}", std::process::id()));
    let mut c = Conformance {
        dir,
        scratch: scratch.clone(),
        sevenzip,
        payloads: Vec::new(),
        seen_payloads: Vec::new(),
        streams: Vec::new(),
        corruptions: Vec::new(),
    };
    let result = conformance_inner(&mut c, rar_source);
    let _ = fs::remove_dir_all(&scratch);
    let (rar_members, hostile) = result?;
    let manifest = json!({
        "schema": CONFORMANCE_SCHEMA,
        "generator": format!("tools/ppmd-corpus {}", env!("CARGO_PKG_VERSION")),
        "regenerate": "cargo run --locked --release -p ppmd-corpus -- conformance --rar-source <unrar-rs>/tests/fixtures/rar4",
        "sevenzip": {"banner": sevenzip.banner, "version": sevenzip.version},
        "ppmd_rust": "1.5.0",
        "payloads": c.payloads,
        "streams": c.streams,
        "rar_members": rar_members,
        "hostile": hostile,
        "corruptions": c.corruptions,
    });
    write(
        &dir.join("manifest.json"),
        (serde_json::to_string_pretty(&manifest).unwrap() + "\n").as_bytes(),
    )?;
    Ok(manifest)
}

fn conformance_inner(
    c: &mut Conformance<'_>,
    rar_source: &Path,
) -> Result<(Vec<Value>, Vec<Value>), String> {
    use Coder::{CarryLess, SevenZ};
    // Every kind at every small size, order 6, 16 MiB: both ppmd-rust
    // framings (no end marker, end marker) and 7zz's own stream.
    let mut small: Vec<(Kind, usize)> = vec![(Kind::Text, 0), (Kind::Text, 1)];
    for kind in Kind::ALL {
        small.push((kind, KIB as usize));
        small.push((kind, 64 * KIB as usize));
    }
    for &(kind, size) in &small {
        c.rust_stream(SevenZ, kind, size, 6, 16 * MIB, false)?;
        c.rust_stream(SevenZ, kind, size, 6, 16 * MIB, true)?;
        if size > 0 {
            c.sevenzip_stream(kind, size, 6, 16 * MIB)?;
        }
    }
    // The order sweep: 7zz stops at 32; 64 is ppmd-rust only.
    for kind in [Kind::Text, Kind::Binary] {
        for order in [2, 4, 8, 16, 32, 64] {
            c.rust_stream(SevenZ, kind, 64 * KIB as usize, order, 16 * MIB, false)?;
            if order <= 32 {
                c.sevenzip_stream(kind, 64 * KIB as usize, order, 16 * MIB)?;
            }
        }
    }
    // The memory sweep. 7-Zip refuses less than 64 KiB and shrinks the size
    // to fit the input, so the small and the huge sizes are ppmd-rust only.
    for mem in [2 * KIB, 64 * KIB, MIB, 256 * MIB, GIB] {
        c.rust_stream(SevenZ, Kind::Text, 64 * KIB as usize, 6, mem, false)?;
    }
    c.sevenzip_stream(Kind::Text, 64 * KIB as usize, 6, 64 * KIB)?;
    c.rust_stream(SevenZ, Kind::Mixed, 64 * KIB as usize, 16, 2 * KIB, false)?;
    c.rust_stream(SevenZ, Kind::Text, KIB as usize, 64, GIB, true)?;
    // One megabyte at the order and memory size 7zz keeps for it.
    c.sevenzip_stream(Kind::Repetitive, MIB as usize, 8, 16 * MIB)?;

    // Raw carry-less streams: what RAR's PPMd blocks carry after the block
    // header. RAR sizes memory in whole MiB and maps orders above 16 in steps
    // of three, so the orders are ones RAR can express.
    c.rust_stream(CarryLess, Kind::Text, 0, 6, MIB, false)?;
    c.rust_stream(CarryLess, Kind::Text, 1, 6, MIB, false)?;
    for kind in Kind::ALL {
        c.rust_stream(CarryLess, kind, KIB as usize, 6, 16 * MIB, false)?;
    }
    for order in [2, 6, 16, 64] {
        c.rust_stream(CarryLess, Kind::Text, 64 * KIB as usize, order, MIB, false)?;
    }
    for mem in [MIB, 16 * MIB, 256 * MIB] {
        c.rust_stream(CarryLess, Kind::Mixed, 64 * KIB as usize, 8, mem, false)?;
    }
    c.rust_stream(CarryLess, Kind::Text, KIB as usize, 6, MIB, true)?;

    // Corruption recipes over a spread of streams.
    let all = ["truncate-half", "flip-first", "flip-third", "flip-mid"];
    for base in [
        "text-64k.o6.m1m.7zz.ppmd",
        "binary-64k.o6.m1m.7zz.ppmd",
        "random-64k.o6.m16m.ppmd-rust.ppmd",
        "mixed-1k.o6.m16m.ppmd-rust.eos.ppmd",
        "text-64k.o6.m1m.ppmd-rust.ppmd7a",
        "mixed-64k.o8.m16m.ppmd-rust.ppmd7a",
    ] {
        c.corruptions_of(base, &all)?;
    }

    let members = rar_members(c.dir, rar_source)?;
    let hostile = hostile(c.dir, rar_source)?;
    Ok((members, hostile))
}

fn read_source(dir: &Path, name: &str) -> Result<(Vec<u8>, Value), String> {
    let data =
        fs::read(dir.join(name)).map_err(|e| format!("{}: {e}", dir.join(name).display()))?;
    let record = json!({"file": name, "len": data.len(), "sha256": oracle::sha256_hex(&data)});
    Ok((data, record))
}

/// The payload rarpar's `generate_ppmd_perf.py` writes: base64 lines of a
/// SHA-256 counter sequence, cut to `size`. Regenerated here so the RAR
/// member's bytes can be checked without unrar.
pub fn rarpar_ppmd_perf_payload(size: usize) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    const SEED: &[u8] = b"rarpar/unrar-rs deterministic RAR4 PPMd order-16 corpus v1";
    let mut out = Vec::with_capacity(size);
    let mut counter = 0u64;
    while out.len() < size {
        let mut h = Sha256::new();
        h.update(SEED);
        h.update(counter.to_le_bytes());
        let line = base64_line(&h.finalize());
        let take = line.len().min(size - out.len());
        out.extend_from_slice(&line[..take]);
        counter += 1;
    }
    out
}

/// Standard base64 of `data` followed by a newline.
fn base64_line(data: &[u8]) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63]);
        out.push(T[(n >> 12) as usize & 63]);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63]
        } else {
            b'='
        });
    }
    out.push(b'\n');
    out
}

/// RARLAB-written members whose data is a single PPMd block: the packed
/// bytes (block header included) are committed, with the symbol count the
/// escape layer needs and the digests of both the symbols and the member.
fn rar_members(dir: &Path, source: &Path) -> Result<Vec<Value>, String> {
    let volumes = [
        "rar4_ppm_oldmv.rar",
        "rar4_ppm_oldmv.r00",
        "rar4_ppm_oldmv.r01",
        "rar4_ppm_oldmv.r02",
    ];
    let mut datas = Vec::new();
    let mut records = Vec::new();
    for v in volumes {
        let (data, record) = read_source(source, v)?;
        datas.push(data);
        records.push(record);
    }
    let refs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
    let members = rar::members(&refs)?;
    let [member] = members.as_slice() else {
        return Err(format!("rar4_ppm_oldmv: {} members, want 1", members.len()));
    };
    let decoded = oracle::decode_rar_member(&member.packed, member.unpacked_len)?;
    let expected = rarpar_ppmd_perf_payload(member.unpacked_len as usize);
    if decoded.payload != expected {
        return Err("rar4_ppm_oldmv: decoded member differs from the regenerated payload".into());
    }
    if crc32fast::hash(&decoded.payload) != member.crc32 {
        return Err("rar4_ppm_oldmv: decoded member fails the header CRC".into());
    }
    let file = "rar/rar4-ppm-oldmv.packed";
    write(&dir.join(file), &member.packed)?;
    let h = decoded.header;
    Ok(vec![json!({
        "name": "rar4-ppm-oldmv", "file": file,
        "source": {
            "origin": "unrar-rs tests/fixtures/rar4 (github.com/scryer-media/rarpar), written by RARLAB rar 6.24 through rarpar's ppmd_perf recipe (generate_ppmd_perf.py --all)",
            "volumes": records,
        },
        "member": member.name, "method": member.method,
        "packed_len": member.packed.len(), "packed_sha256": oracle::sha256_hex(&member.packed),
        "unpacked_len": member.unpacked_len, "crc32": format!("{:08x}", member.crc32),
        "payload_sha256": oracle::sha256_hex(&decoded.payload),
        "header": {"flags": h.flags, "reset": h.reset, "order": h.order, "mem_mb": h.mem_mb, "esc": h.esc, "len": h.len},
        "symbols": decoded.symbols.len(), "symbols_sha256": oracle::sha256_hex(&decoded.symbols),
        "stop": format!("{:?}", decoded.stop),
    })])
}

/// libarchive's PPMd regression archives, imported into unrar-rs: hostile
/// inputs whose headers lie about their sizes. The packed data of the first
/// file block, cut at the end of the archive, is committed; the suite's only
/// requirement is that decoding it returns, Ok or Err, without a panic.
fn hostile(dir: &Path, source: &Path) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for (name, archive) in [
        (
            "libarchive-ppmd-use-after-free",
            "test_read_format_rar_ppmd_use_after_free.rar",
        ),
        (
            "libarchive-ppmd-use-after-free2",
            "test_read_format_rar_ppmd_use_after_free2.rar",
        ),
    ] {
        let (data, record) = read_source(source, archive)?;
        let (offset, packed, unp) = rar::first_file_data_lenient(&data)
            .ok_or_else(|| format!("{archive}: no file block"))?;
        let file = format!("rar/{name}.packed");
        write(&dir.join(&file), packed)?;
        let header = rar::ppm_header(packed).map(|h| {
            json!({"flags": h.flags, "reset": h.reset, "order": h.order, "mem_mb": h.mem_mb, "esc": h.esc, "len": h.len})
        });
        out.push(json!({
            "name": name, "file": file,
            "source": {"origin": "libarchive test suite (BSD-2-Clause), via unrar-rs tests/fixtures/rar4", "archive": record, "packed_offset": offset},
            "claimed_unpacked_len": unp, "packed_len": packed.len(), "header": header,
        }));
    }
    Ok(out)
}

/// Benchmark profiles: which payloads, orders and memory sizes.
pub struct BenchSpec {
    /// 7zz containers: (kind, size, order, requested mem).
    pub containers: Vec<(Kind, usize, u32, u32)>,
    /// ppmd-rust raw 7z streams for rows 7zz cannot write: (kind, size, order, mem).
    pub raw: Vec<(Kind, usize, u32, u32)>,
    /// Payloads written out for encode rows.
    pub sources: Vec<(Kind, usize)>,
}

/// The corpus for a bench profile (`quick` or `full`; `fleet` runs on `full`).
pub fn bench_spec(profile: &str) -> Result<BenchSpec, String> {
    let m = MIB as usize;
    match profile {
        "quick" => {
            let mut containers: Vec<_> =
                Kind::ALL.into_iter().map(|k| (k, m, 6, 16 * MIB)).collect();
            for order in [2, 16, 32] {
                containers.push((Kind::Text, m, order, 16 * MIB));
            }
            containers.push((Kind::Text, m, 6, MIB));
            Ok(BenchSpec {
                containers,
                raw: vec![(Kind::Text, m, 8, GIB)],
                sources: vec![(Kind::Text, m)],
            })
        }
        "full" => {
            let big = 16 * m;
            let mut containers = Vec::new();
            for order in [2, 4, 6, 8, 16, 32] {
                for mem in [MIB, 16 * MIB, 256 * MIB] {
                    containers.push((Kind::Text, big, order, mem));
                }
            }
            for kind in [Kind::Binary, Kind::Mixed, Kind::Repetitive, Kind::Random] {
                for order in [6, 16] {
                    for mem in [16 * MIB, 256 * MIB] {
                        containers.push((kind, big, order, mem));
                    }
                }
            }
            containers.push((Kind::Text, m, 6, 16 * MIB));
            Ok(BenchSpec {
                containers,
                raw: vec![(Kind::Text, big, 8, GIB)],
                sources: vec![(Kind::Text, big), (Kind::Mixed, big)],
            })
        }
        other => Err(format!(
            "unknown bench profile {other:?} (want quick or full)"
        )),
    }
}

/// Generates a bench corpus into `dir`: `.7z` containers written by 7zz, the
/// raw ppmd-rust streams, the encode sources, and `manifest.json`. Every
/// stream is decoded by ppmd-rust and checked against its payload first.
pub fn bench(
    dir: &Path,
    profile: &str,
    sevenzip: &SevenZip,
    only: &[String],
) -> Result<Value, String> {
    let spec = bench_spec(profile)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let scratch = dir.join(".scratch");
    let keep = |name: &str| only.is_empty() || only.iter().any(|o| name.contains(o.as_str()));
    let mut payloads: Vec<Value> = Vec::new();
    let note_payload = |kind: Kind, size: usize, data: &[u8], payloads: &mut Vec<Value>| {
        let name = payload::file_name(kind, size);
        if !payloads.iter().any(|p| p["name"] == name) {
            payloads.push(json!({"name": name, "kind": kind.name(), "size": size,
                "sha256": oracle::sha256_hex(data), "crc32": oracle::crc32_hex(data)}));
        }
    };
    let mut archives = Vec::new();
    for &(kind, size, order, mem) in &spec.containers {
        let label = format!("{}.o{order}.m{}", stem(kind, size), mem_label(mem));
        if !keep(&label) {
            continue;
        }
        let data = payload::generate(kind, size);
        note_payload(kind, size, &data, &mut payloads);
        let container =
            sevenzip.compress(&scratch, &payload::file_name(kind, size), &data, order, mem)?;
        let ps = sevenz::pack_stream(&container)
            .ok_or_else(|| format!("{label}: unexpected 7z shape"))?;
        let stream = sevenz::stream_bytes(&container, &ps);
        let back = oracle::decode(
            Coder::SevenZ,
            stream,
            ps.order,
            ps.mem_size,
            Some(ps.unpacked_len),
        )?;
        if back != data {
            return Err(format!(
                "{label}: ppmd-rust does not decode 7zz's stream to the payload"
            ));
        }
        let file = format!("{label}.7z");
        write(&dir.join(&file), &container)?;
        eprintln!(
            "{file}: {} -> {} bytes, order {} mem {}",
            size,
            ps.packed_len,
            ps.order,
            mem_label(ps.mem_size)
        );
        archives.push(json!({
            "name": label, "file": file, "payload": payload::file_name(kind, size), "kind": kind.name(),
            "order": ps.order, "mem": ps.mem_size, "requested_mem": mem,
            "offset": ps.offset, "packed_len": ps.packed_len, "unpacked_len": ps.unpacked_len,
            "container_len": container.len(), "payload_crc32": oracle::crc32_hex(&data),
        }));
    }
    let mut raw = Vec::new();
    for &(kind, size, order, mem) in &spec.raw {
        let label = format!(
            "{}.o{order}.m{}.ppmd-rust",
            stem(kind, size),
            mem_label(mem)
        );
        if !keep(&label) {
            continue;
        }
        let data = payload::generate(kind, size);
        note_payload(kind, size, &data, &mut payloads);
        let stream = oracle::encode(Coder::SevenZ, &data, order, mem, false)?;
        if oracle::decode(Coder::SevenZ, &stream, order, mem, Some(size as u64))? != data {
            return Err(format!("{label}: ppmd-rust round trip failed"));
        }
        let file = format!("{label}.ppmd");
        write(&dir.join(&file), &stream)?;
        raw.push(json!({
            "name": label, "file": file, "payload": payload::file_name(kind, size), "kind": kind.name(),
            "order": order, "mem": mem, "packed_len": stream.len(), "unpacked_len": size,
            "payload_crc32": oracle::crc32_hex(&data),
        }));
    }
    let mut sources = Vec::new();
    for &(kind, size) in &spec.sources {
        let name = payload::file_name(kind, size);
        if !keep(&name) {
            continue;
        }
        let data = payload::generate(kind, size);
        note_payload(kind, size, &data, &mut payloads);
        write(&dir.join(&name), &data)?;
        sources.push(
            json!({"name": name, "file": name, "kind": kind.name(), "size": size,
            "crc32": oracle::crc32_hex(&data)}),
        );
    }
    let _ = fs::remove_dir_all(&scratch);
    let manifest = json!({
        "schema": BENCH_SCHEMA, "profile": profile,
        "generator": format!("tools/ppmd-corpus {}", env!("CARGO_PKG_VERSION")),
        "sevenzip": {"banner": sevenzip.banner, "version": sevenzip.version, "path": sevenzip.path},
        "payloads": payloads, "archives": archives, "raw": raw, "sources": sources,
    });
    write(
        &dir.join("manifest.json"),
        (serde_json::to_string_pretty(&manifest).unwrap() + "\n").as_bytes(),
    )?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_python() {
        // base64.b64encode(b"\x00\x01\x02\x03") == b"AAECAw=="
        assert_eq!(base64_line(&[0, 1, 2, 3]), b"AAECAw==\n");
        assert_eq!(base64_line(b"Man"), b"TWFu\n");
    }

    #[test]
    fn mem_labels() {
        assert_eq!(mem_label(2048), "2k");
        assert_eq!(mem_label(16 << 20), "16m");
        assert_eq!(mem_label(1 << 30), "1g");
    }

    #[test]
    fn bench_profiles() {
        assert!(bench_spec("quick").is_ok() && bench_spec("full").is_ok());
        assert!(bench_spec("fleet").is_err());
    }
}
