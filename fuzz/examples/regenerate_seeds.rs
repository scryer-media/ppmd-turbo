//! Writes the fuzz seed corpora, the fuzz regressions and the hostile-test
//! fixtures (see `ppmd_turbo_fuzz::seeds`) under the repository root, or
//! under the directory given as the only argument.

use std::path::PathBuf;
use std::process::ExitCode;

use ppmd_turbo_fuzz::seeds;

fn main() -> ExitCode {
    let root = std::env::args_os()
        .nth(1)
        .map_or_else(seeds::repo_root, PathBuf::from);
    match seeds::write_all(&root) {
        Ok(files) => {
            println!("{}: {} files", root.display(), files.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("regenerate_seeds: {e}");
            ExitCode::FAILURE
        }
    }
}
