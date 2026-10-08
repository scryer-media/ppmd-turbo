//! Locating and driving the out-of-process references: `7zz` and `unrar`.
//!
//! Standard library only, so `tests/differential_binaries.rs` includes this
//! file directly with `#[path]`. Every run is a plain child process with
//! captured output; nothing waits on a clock.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Overrides the `7zz` location.
pub const SEVEN_ZIP_ENV: &str = "PPMD_ORACLE_7ZZ";
/// Overrides the `unrar` location.
pub const UNRAR_ENV: &str = "PPMD_ORACLE_UNRAR";

const SEVEN_ZIP_DEFAULTS: &[&str] = &[
    "/opt/homebrew/bin/7zz",
    "/usr/local/bin/7zz",
    "/usr/bin/7zz",
];

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn from_env(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

fn failed(what: &str, out: &Output) -> io::Error {
    io::Error::other(format!(
        "{what} exited with {}: {}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The 7-Zip console binary.
#[derive(Debug, Clone)]
pub struct SevenZip {
    /// Where it is.
    pub path: PathBuf,
}

impl SevenZip {
    /// `$PPMD_ORACLE_7ZZ`, a usual install location, or `7zz` on `PATH`.
    pub fn find() -> Option<Self> {
        from_env(SEVEN_ZIP_ENV)
            .or_else(|| {
                SEVEN_ZIP_DEFAULTS
                    .iter()
                    .map(PathBuf::from)
                    .find(|p| p.is_file())
            })
            .or_else(|| on_path("7zz"))
            .map(|path| Self { path })
    }

    /// The first line of its banner.
    pub fn version(&self) -> io::Result<String> {
        let out = Command::new(&self.path).stdin(Stdio::null()).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    /// Compresses `data` into a single-file `.7z` with PPMd at `order` and
    /// `mem` bytes, plain header, inside `work` (created if missing). 7-Zip
    /// may lower `mem` for small inputs; read the properties back from the
    /// archive rather than assuming them.
    pub fn compress_ppmd(
        &self,
        work: &Path,
        name: &str,
        data: &[u8],
        order: u32,
        mem: u32,
    ) -> io::Result<Vec<u8>> {
        fs::create_dir_all(work)?;
        let input = work.join(name);
        let archive = work.join(format!("{name}.7z"));
        fs::write(&input, data)?;
        if archive.exists() {
            fs::remove_file(&archive)?;
        }
        let out = Command::new(&self.path)
            .current_dir(work)
            .args([
                "a", "-bd", "-y", "-mhc=off", "-mtm=off", "-mtc=off", "-mta=off",
            ])
            .arg(format!("-m0=PPMd:o={order}:mem={mem}b"))
            .arg(&archive)
            .arg(name)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(failed("7zz a", &out));
        }
        let bytes = fs::read(&archive)?;
        fs::remove_file(&archive)?;
        fs::remove_file(&input)?;
        Ok(bytes)
    }

    /// Extracts the single file of `archive` (bytes) and returns its data.
    /// A CRC or data error is an `Err` carrying 7-Zip's message.
    pub fn extract(&self, work: &Path, archive: &[u8]) -> io::Result<Vec<u8>> {
        fs::create_dir_all(work)?;
        let path = work.join("oracle-extract.7z");
        fs::write(&path, archive)?;
        let out = Command::new(&self.path)
            .args(["e", "-so", "-bd", "-y"])
            .arg(&path)
            .stdin(Stdio::null())
            .output()?;
        fs::remove_file(&path)?;
        if !out.status.success() {
            return Err(failed("7zz e", &out));
        }
        Ok(out.stdout)
    }
}

/// RARLAB's `unrar`. Only ever found on `PATH` (or `$PPMD_ORACLE_UNRAR`);
/// when absent, every RAR oracle check skips.
#[derive(Debug, Clone)]
pub struct Unrar {
    /// Where it is.
    pub path: PathBuf,
}

impl Unrar {
    /// `$PPMD_ORACLE_UNRAR` or `unrar` on `PATH`.
    pub fn find() -> Option<Self> {
        from_env(UNRAR_ENV)
            .or_else(|| on_path("unrar"))
            .map(|path| Self { path })
    }

    /// Prints every file of `archive` to stdout and returns the bytes, in
    /// archive order. A CRC or data error is an `Err`.
    pub fn print(&self, archive: &Path) -> io::Result<Vec<u8>> {
        let out = Command::new(&self.path)
            .args(["p", "-inul", "-y"])
            .arg(archive)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(failed("unrar p", &out));
        }
        Ok(out.stdout)
    }

    /// Tests `archive`; `Ok(())` when unrar reports it intact.
    pub fn test(&self, archive: &Path) -> io::Result<()> {
        let out = Command::new(&self.path)
            .args(["t", "-inul", "-y"])
            .arg(archive)
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failed("unrar t", &out))
        }
    }
}
