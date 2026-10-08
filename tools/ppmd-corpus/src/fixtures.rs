//! Regenerating every generated file the repository's tests and fuzz
//! targets read. None of them is committed; this is the one entry point:
//!
//! ```text
//! cargo run --locked --release -p ppmd-corpus -- fixtures
//! ```
//!
//! It writes, from the committed recipes:
//!
//! - `tests/fixtures/{7z,rar}/`: the conformance streams named in
//!   `tests/fixtures/manifest.json`. ppmd-rust rows are encoded in process;
//!   `7zz` rows are written by 7-Zip's `7zz` (skipped when it is absent);
//!   the three RAR rows are cut from RARLAB-written and libarchive archives
//!   fetched by digest from rarpar's published test corpus (skipped when
//!   neither `--rar-source` nor the network supplies them);
//! - `fuzz/seeds/`, `fuzz/regressions/` and `tests/hostile_fixtures/`, by
//!   running the fuzz crate's `regenerate_seeds` example
//!   (`fuzz/src/seeds.rs`).
//!
//! Every file written is then checked against `tools/ppmd-corpus/fixtures.sha256`,
//! and each conformance stream against its manifest digest as well.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::corpus::{self, SevenZip};
use crate::oracle::{self, Coder};
use crate::payload;
use crate::sevenz;

/// The digest list, relative to the repository root.
pub const DIGESTS: &str = "tools/ppmd-corpus/fixtures.sha256";

/// Where the conformance manifest and streams live, relative to the root.
pub const CONFORMANCE_DIR: &str = "tests/fixtures";

/// Set to make the conformance suites fail, rather than skip, when a
/// fixture they cannot build in process (a `7zz` or RAR row) is missing.
pub const REQUIRE_ENV: &str = "PPMD_TURBO_REQUIRE_FIXTURES";

/// rarpar's published, content-addressed test corpus: objects are served by
/// BLAKE3 under this prefix. The RAR rows' source archives come from here.
pub const RAR_CORPUS_OBJECTS: &str =
    "https://rarpar.cdn.scryer-media.dev/test-corpus/objects/blake3/";

/// The source archives of the RAR rows, as rarpar's corpus ledger
/// (`test-corpus/sources.json`) names them: file name and BLAKE3. Their
/// SHA-256 digests are in the conformance manifest, and are what is checked.
pub const RAR_SOURCES: [(&str, &str); 6] = [
    (
        "rar4_ppm_oldmv.rar",
        "339ca617c544c60f2575b7a789aa730f52480e58fcf3079359524459df07a33f",
    ),
    (
        "rar4_ppm_oldmv.r00",
        "d7df83bfa3c37579d0fc8855df626214468e99d2e59126c9c55fcee41f408de1",
    ),
    (
        "rar4_ppm_oldmv.r01",
        "2c1596d784d0c463cf6f347b111c3fcdb174b12304dbcf3755c8d5d8e15e0266",
    ),
    (
        "rar4_ppm_oldmv.r02",
        "2020f7cd5d1836a7bf3d2ec29aee15cd71211a3a4d348dc478c2b7c0b49b40ed",
    ),
    (
        "test_read_format_rar_ppmd_use_after_free.rar",
        "8f3def47f026b013c7f24bb38ca4274d20c5fca733047b0f0195c8f7876a59fd",
    ),
    (
        "test_read_format_rar_ppmd_use_after_free2.rar",
        "78d35ef12a74257f04d01458797990b4f8911ff08335bfca696c282faa244e40",
    ),
];

/// What `fixtures` should do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The repository root.
    pub root: PathBuf,
    /// `7zz` to use; `7zz` on `PATH` when `None`.
    pub sevenzip: Option<PathBuf>,
    /// A directory holding the RAR source archives; fetched when `None`.
    pub rar_source: Option<PathBuf>,
    /// Fail instead of skipping when an oracle or a source is missing.
    pub require_all: bool,
    /// Only these parts (`conformance`, `seeds`); all when empty.
    pub only: Vec<String>,
    /// Rewrite the digest list from what was written instead of checking it.
    pub write_digests: bool,
}

/// What a run wrote and skipped.
#[derive(Debug, Default)]
pub struct Summary {
    /// Every file written, repository-relative, with its SHA-256.
    pub written: BTreeMap<String, String>,
    /// Why each skipped group was skipped.
    pub skipped: Vec<String>,
}

/// Parses `tests/fixtures/manifest.json` under `root`.
pub fn manifest(root: &Path) -> Result<Value, String> {
    let path = root.join(CONFORMANCE_DIR).join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if m["schema"] != corpus::CONFORMANCE_SCHEMA {
        return Err(format!("{}: schema {}", path.display(), m["schema"]));
    }
    Ok(m)
}

fn str_of<'a>(entry: &'a Value, key: &str) -> Result<&'a str, String> {
    entry[key]
        .as_str()
        .ok_or_else(|| format!("{key} missing in {entry}"))
}

fn u32_of(entry: &Value, key: &str) -> Result<u32, String> {
    entry[key]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("{key} missing in {entry}"))
}

fn coder_of(entry: &Value) -> Result<Coder, String> {
    match str_of(entry, "coder")? {
        "7z" => Ok(Coder::SevenZ),
        "carry-less" => Ok(Coder::CarryLess),
        other => Err(format!("unknown coder {other}")),
    }
}

fn payload_of(entry: &Value) -> Result<Vec<u8>, String> {
    let name = str_of(entry, "payload")?;
    let (kind, size) =
        payload::parse_file_name(name).ok_or_else(|| format!("payload name {name}"))?;
    Ok(payload::generate(kind, size))
}

fn check_digest(entry: &Value, data: &[u8], key: &str) -> Result<(), String> {
    let want = str_of(entry, key)?;
    let got = oracle::sha256_hex(data);
    if got != want {
        return Err(format!(
            "{}: generated {got}, the manifest records {want}",
            entry["name"]
        ));
    }
    Ok(())
}

/// The bytes of a manifest stream whose producer is ppmd-rust, encoded in
/// process and checked against the manifest digest. The conformance suites
/// call this when the file is not on disk, so `cargo test` needs no
/// generated files for these rows.
pub fn ppmd_rust_stream(entry: &Value) -> Result<Vec<u8>, String> {
    if entry["producer"] != "ppmd-rust" {
        return Err(format!("{}: not a ppmd-rust stream", entry["name"]));
    }
    let data = payload_of(entry)?;
    let stream = oracle::encode(
        coder_of(entry)?,
        &data,
        u32_of(entry, "order")?,
        u32_of(entry, "mem")?,
        entry["end_marker"] == true,
    )?;
    check_digest(entry, &stream, "stream_sha256")?;
    Ok(stream)
}

/// The bytes of a manifest stream whose producer is `7zz`: the payload is
/// archived at the requested order and memory size and the stream pulled
/// out of the container, then checked against the manifest's order, memory
/// size and digest.
pub fn sevenzip_stream(
    sevenzip: &SevenZip,
    scratch: &Path,
    entry: &Value,
) -> Result<Vec<u8>, String> {
    let data = payload_of(entry)?;
    let requested = &entry["requested"];
    let archive = sevenzip.compress(
        scratch,
        str_of(entry, "payload")?,
        &data,
        u32_of(requested, "order")?,
        u32_of(requested, "mem")?,
    )?;
    let ps = sevenz::pack_stream(&archive).ok_or_else(|| {
        format!(
            "{}: 7zz wrote a shape the walk does not read",
            entry["name"]
        )
    })?;
    if ps.order != u32_of(entry, "order")? || ps.mem_size != u32_of(entry, "mem")? {
        return Err(format!(
            "{}: {} wrote order {} mem {}, the manifest records order {} mem {}",
            entry["name"], sevenzip.banner, ps.order, ps.mem_size, entry["order"], entry["mem"]
        ));
    }
    let stream = sevenz::stream_bytes(&archive, &ps).to_vec();
    check_digest(entry, &stream, "stream_sha256")?;
    Ok(stream)
}

/// The SHA-256 the manifest records for each RAR source archive.
fn rar_source_digests(m: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut note = |rec: &Value| {
        if let (Some(f), Some(h)) = (rec["file"].as_str(), rec["sha256"].as_str()) {
            out.insert(f.to_string(), h.to_string());
        }
    };
    for member in m["rar_members"].as_array().into_iter().flatten() {
        for v in member["source"]["volumes"].as_array().into_iter().flatten() {
            note(v);
        }
    }
    for h in m["hostile"].as_array().into_iter().flatten() {
        note(&h["source"]["archive"]);
    }
    out
}

fn file_matches(path: &Path, sha256: &str) -> bool {
    fs::read(path).is_ok_and(|d| oracle::sha256_hex(&d) == sha256)
}

/// A directory holding the six RAR source archives, each checked against
/// the manifest: `given` when set, else a cache under `target/` filled from
/// rarpar's published corpus with `curl`.
fn rar_sources(root: &Path, given: Option<&Path>, m: &Value) -> Result<PathBuf, String> {
    let digests = rar_source_digests(m);
    if digests.len() != RAR_SOURCES.len() {
        return Err(format!(
            "the manifest names {} RAR sources, the tool knows {}",
            digests.len(),
            RAR_SOURCES.len()
        ));
    }
    let dir = match given {
        Some(d) => d.to_path_buf(),
        None => root.join("target").join("ppmd-corpus").join("rar-sources"),
    };
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, blake3) in RAR_SOURCES {
        let want = digests
            .get(name)
            .ok_or_else(|| format!("the manifest has no digest for {name}"))?;
        let path = dir.join(name);
        if file_matches(&path, want) {
            continue;
        }
        if given.is_some() {
            return Err(format!(
                "{}: missing or not the archive the manifest records",
                path.display()
            ));
        }
        let part = dir.join(format!("{name}.part"));
        let url = format!("{RAR_CORPUS_OBJECTS}{blake3}");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&part)
            .arg(&url)
            .status()
            .map_err(|e| format!("curl: {e}"))?;
        if !status.success() {
            let _ = fs::remove_file(&part);
            return Err(format!("curl {url}: {status}"));
        }
        if !file_matches(&part, want) {
            let _ = fs::remove_file(&part);
            return Err(format!("{url}: not the archive the manifest records"));
        }
        fs::rename(&part, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(dir)
}

fn write(root: &Path, rel: &str, data: &[u8], summary: &mut Summary) -> Result<(), String> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
    summary
        .written
        .insert(rel.to_string(), oracle::sha256_hex(data));
    Ok(())
}

fn skip(opts: &Options, summary: &mut Summary, why: String) -> Result<(), String> {
    if opts.require_all {
        return Err(why);
    }
    eprintln!("ppmd-corpus: skipped {why}");
    summary.skipped.push(why);
    Ok(())
}

fn conformance(opts: &Options, summary: &mut Summary) -> Result<(), String> {
    let root = &opts.root;
    let m = manifest(root)?;
    let streams = m["streams"].as_array().cloned().unwrap_or_default();
    let sevenzip = match SevenZip::probe(opts.sevenzip.as_deref()) {
        Ok(s) => Some(s),
        Err(e) => {
            let n = streams.iter().filter(|s| s["producer"] == "7zz").count();
            skip(opts, summary, format!("{n} 7zz streams: {e}"))?;
            None
        }
    };
    let scratch = std::env::temp_dir().join(format!("ppmd-corpus-fixtures-{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        for s in &streams {
            let data = match str_of(s, "producer")? {
                "ppmd-rust" => ppmd_rust_stream(s)?,
                "7zz" => match &sevenzip {
                    Some(sz) => sevenzip_stream(sz, &scratch, s)?,
                    None => continue,
                },
                other => return Err(format!("{}: unknown producer {other}", s["name"])),
            };
            write(
                root,
                &format!("{CONFORMANCE_DIR}/{}", str_of(s, "file")?),
                &data,
                summary,
            )?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&scratch);
    result?;

    let rar_rows = m["rar_members"].as_array().map_or(0, Vec::len)
        + m["hostile"].as_array().map_or(0, Vec::len);
    let source = match rar_sources(root, opts.rar_source.as_deref(), &m) {
        Ok(dir) => dir,
        Err(e) => return skip(opts, summary, format!("{rar_rows} RAR rows: {e}")),
    };
    let dir = root.join(CONFORMANCE_DIR);
    let members = corpus::rar_members(&dir, &source)?;
    let hostile = corpus::hostile(&dir, &source)?;
    for (made, key, sha_key) in [
        (&members, "rar_members", "packed_sha256"),
        (&hostile, "hostile", "packed_sha256"),
    ] {
        for rec in made {
            let file = str_of(rec, "file")?;
            let entry = m[key]
                .as_array()
                .into_iter()
                .flatten()
                .find(|e| e["file"] == file)
                .ok_or_else(|| format!("{file}: not in the manifest"))?;
            let data = fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
            check_digest(entry, &data, sha_key)?;
            summary.written.insert(
                format!("{CONFORMANCE_DIR}/{file}"),
                oracle::sha256_hex(&data),
            );
        }
    }
    Ok(())
}

/// Every file under `dir` (repository-relative), recursively.
fn files_under(root: &Path, dir: &str, out: &mut Vec<String>) {
    let mut stack = vec![root.join(dir)];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

fn seeds(opts: &Options, summary: &mut Summary) -> Result<(), String> {
    let root = &opts.root;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args([
            "run",
            "--locked",
            "--quiet",
            "--manifest-path",
            "fuzz/Cargo.toml",
            "--example",
            "regenerate_seeds",
            "--",
        ])
        .arg(root)
        .status()
        .map_err(|e| format!("cargo run --example regenerate_seeds: {e}"))?;
    if !status.success() {
        return Err(format!("cargo run --example regenerate_seeds: {status}"));
    }
    let mut files = Vec::new();
    for dir in ["fuzz/seeds", "fuzz/regressions", "tests/hostile_fixtures"] {
        files_under(root, dir, &mut files);
    }
    for rel in files {
        let data = fs::read(root.join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
        summary.written.insert(rel, oracle::sha256_hex(&data));
    }
    Ok(())
}

/// Parses the digest list: `<sha256>  <path>` lines.
pub fn read_digests(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join(DIGESTS);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for line in text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (sha, rel) = line
            .split_once("  ")
            .ok_or_else(|| format!("{DIGESTS}: bad line {line:?}"))?;
        out.insert(rel.to_string(), sha.to_string());
    }
    Ok(out)
}

fn digest_text(written: &BTreeMap<String, String>) -> String {
    let mut text = String::from(
        "# SHA-256 of every generated fixture, written by `ppmd-corpus fixtures --write-digests`.\n# `ppmd-corpus fixtures` checks each file it writes against this list.\n",
    );
    for (rel, sha) in written {
        text.push_str(&format!("{sha}  {rel}\n"));
    }
    text
}

fn wants(opts: &Options, part: &str) -> bool {
    opts.only.is_empty() || opts.only.iter().any(|o| o == part)
}

/// Regenerates the fixtures and checks them against the digest list.
pub fn run(opts: &Options) -> Result<Summary, String> {
    let mut summary = Summary::default();
    if wants(opts, "conformance") {
        conformance(opts, &mut summary)?;
    }
    if wants(opts, "seeds") {
        seeds(opts, &mut summary)?;
    }
    if opts.write_digests {
        if !opts.only.is_empty() || !summary.skipped.is_empty() {
            return Err("--write-digests needs a full run with every oracle present".into());
        }
        let path = opts.root.join(DIGESTS);
        fs::write(&path, digest_text(&summary.written))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(summary);
    }
    let listed = read_digests(&opts.root)?;
    let mut problems = Vec::new();
    for (rel, sha) in &summary.written {
        match listed.get(rel) {
            None => problems.push(format!("{rel}: not in {DIGESTS}")),
            Some(want) if want != sha => {
                problems.push(format!("{rel}: generated {sha}, {DIGESTS} records {want}"))
            }
            Some(_) => {}
        }
    }
    let in_scope = |rel: &str| {
        let conformance = rel.starts_with(&format!("{CONFORMANCE_DIR}/"));
        if conformance {
            wants(opts, "conformance")
        } else {
            wants(opts, "seeds")
        }
    };
    let missing: Vec<&String> = listed
        .keys()
        .filter(|rel| in_scope(rel) && !summary.written.contains_key(*rel))
        .collect();
    if summary.skipped.is_empty() {
        problems.extend(
            missing
                .iter()
                .map(|rel| format!("{rel}: listed but not generated")),
        );
    }
    if !problems.is_empty() {
        return Err(format!(
            "{} fixture problems:\n{}",
            problems.len(),
            problems.join("\n")
        ));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn every_ppmd_rust_stream_regenerates() {
        let m = manifest(&root()).unwrap();
        let mut n = 0;
        for s in m["streams"].as_array().unwrap() {
            if s["producer"] == "ppmd-rust" {
                ppmd_rust_stream(s).unwrap();
                n += 1;
            }
        }
        assert!(n > 0);
    }

    #[test]
    fn the_digest_list_covers_the_manifest() {
        let m = manifest(&root()).unwrap();
        let listed = read_digests(&root()).unwrap();
        for key in ["streams", "rar_members", "hostile"] {
            for e in m[key].as_array().unwrap() {
                let rel = format!("{CONFORMANCE_DIR}/{}", e["file"].as_str().unwrap());
                assert!(listed.contains_key(&rel), "{rel}");
            }
        }
    }

    #[test]
    fn rar_sources_match_the_manifest() {
        let m = manifest(&root()).unwrap();
        let digests = rar_source_digests(&m);
        for (name, _) in RAR_SOURCES {
            assert!(digests.contains_key(name), "{name}");
        }
    }
}
