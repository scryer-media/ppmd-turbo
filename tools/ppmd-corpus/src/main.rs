//! `ppmd-corpus`: generates ppmd-turbo's corpora.
//!
//! ```text
//! ppmd-corpus fixtures [--root .] [--sevenzip 7zz] [--rar-source DIR] [--require-all] [--only conformance,seeds] [--write-digests]
//! ppmd-corpus conformance [--dir tests/fixtures] [--sevenzip 7zz] --rar-source <unrar-rs>/tests/fixtures/rar4
//! ppmd-corpus bench --profile quick|full --dir bench/fixtures/<profile> [--sevenzip 7zz] [--only SUBSTR,...]
//! ppmd-corpus payload --name fixture-text-1m.bin --out FILE
//! ```
//!
//! Exit status: 0 success, 1 a generation or check failed, 2 usage.

use std::path::PathBuf;
use std::process::ExitCode;

use ppmd_corpus::{corpus, fixtures, payload};

const USAGE: &str = "usage:
  ppmd-corpus fixtures [--root .] [--sevenzip 7zz] [--rar-source DIR] [--require-all] [--only conformance,seeds] [--write-digests]
  ppmd-corpus conformance [--dir tests/fixtures] [--sevenzip 7zz] --rar-source DIR
  ppmd-corpus bench --profile quick|full [--dir bench/fixtures/<profile>] [--sevenzip 7zz] [--only SUBSTR,...]
  ppmd-corpus payload --name fixture-<kind>-<size>.bin --out FILE";

struct Args {
    command: String,
    dir: Option<PathBuf>,
    sevenzip: Option<PathBuf>,
    rar_source: Option<PathBuf>,
    profile: Option<String>,
    only: Vec<String>,
    name: Option<String>,
    out: Option<PathBuf>,
    root: Option<PathBuf>,
    require_all: bool,
    write_digests: bool,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let command = it.next().ok_or("missing command")?;
    let mut args = Args {
        command,
        dir: None,
        sevenzip: None,
        rar_source: None,
        profile: None,
        only: Vec::new(),
        name: None,
        out: None,
        root: None,
        require_all: false,
        write_digests: false,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--dir" => args.dir = Some(value()?.into()),
            "--sevenzip" => args.sevenzip = Some(value()?.into()),
            "--rar-source" => args.rar_source = Some(value()?.into()),
            "--profile" => args.profile = Some(value()?),
            "--only" => {
                args.only = value()?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--name" => args.name = Some(value()?),
            "--out" => args.out = Some(value()?.into()),
            "--root" => args.root = Some(value()?.into()),
            "--require-all" => args.require_all = true,
            "--write-digests" => args.write_digests = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("ppmd-corpus: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match args.command.as_str() {
        "fixtures" => (|| -> Result<(), String> {
            let root = match args.root.clone() {
                Some(r) => r,
                None => std::env::current_dir().map_err(|e| e.to_string())?,
            };
            let root = root
                .canonicalize()
                .map_err(|e| format!("{}: {e}", root.display()))?;
            let summary = fixtures::run(&fixtures::Options {
                root: root.clone(),
                sevenzip: args.sevenzip.clone(),
                rar_source: args.rar_source.clone(),
                require_all: args.require_all,
                only: args.only.clone(),
                write_digests: args.write_digests,
            })?;
            println!(
                "{}: {} fixtures written and checked against {}{}",
                root.display(),
                summary.written.len(),
                fixtures::DIGESTS,
                if summary.skipped.is_empty() {
                    String::new()
                } else {
                    format!("; skipped: {}", summary.skipped.join("; "))
                }
            );
            Ok(())
        })(),
        "conformance" => (|| -> Result<(), String> {
            let rar = args
                .rar_source
                .as_deref()
                .ok_or("--rar-source is required")?;
            let sevenzip = corpus::SevenZip::probe(args.sevenzip.as_deref())?;
            let dir = args
                .dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("tests/fixtures"));
            let m = corpus::conformance(&dir, &sevenzip, rar)?;
            println!(
                "{}: {} payloads, {} streams, {} RAR members, {} hostile, {} corruptions",
                dir.join("manifest.json").display(),
                m["payloads"].as_array().map_or(0, Vec::len),
                m["streams"].as_array().map_or(0, Vec::len),
                m["rar_members"].as_array().map_or(0, Vec::len),
                m["hostile"].as_array().map_or(0, Vec::len),
                m["corruptions"].as_array().map_or(0, Vec::len),
            );
            Ok(())
        })(),
        "bench" => (|| -> Result<(), String> {
            let profile = args.profile.as_deref().ok_or("--profile is required")?;
            let sevenzip = corpus::SevenZip::probe(args.sevenzip.as_deref())?;
            let dir = args
                .dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("bench/fixtures").join(profile));
            let m = corpus::bench(&dir, profile, &sevenzip, &args.only)?;
            println!(
                "{}: {} archives, {} raw streams, {} sources",
                dir.join("manifest.json").display(),
                m["archives"].as_array().map_or(0, Vec::len),
                m["raw"].as_array().map_or(0, Vec::len),
                m["sources"].as_array().map_or(0, Vec::len),
            );
            Ok(())
        })(),
        "payload" => (|| -> Result<(), String> {
            let name = args.name.as_deref().ok_or("--name is required")?;
            let out = args.out.as_deref().ok_or("--out is required")?;
            let (kind, size) =
                payload::parse_file_name(name).ok_or("name is not fixture-<kind>-<size>.bin")?;
            std::fs::write(out, payload::generate(kind, size)).map_err(|e| e.to_string())
        })(),
        _ => Err(format!("unknown command {}", args.command)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ppmd-corpus: {e}");
            ExitCode::from(1)
        }
    }
}
