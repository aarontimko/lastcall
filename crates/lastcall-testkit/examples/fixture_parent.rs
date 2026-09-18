//! `fixture_parent --parent <W> --state-dir <S> [--late-ops]`: build the golden's
//! three-root parent dir (`just probe-status`), or — with `--late-ops` against an already
//! built one — commit in repo A at t+2 s and edit there at t+4 s (`just probe-watch`).
//!
//! Prints `parent=<W>`, `state_dir=<S>` and `config=<S>/config.toml` (written here, with
//! `parent_dirs = [W]`) so a shell recipe can point the release binary at them.

use std::path::PathBuf;
use std::process::ExitCode;

use lastcall_testkit::fixture_parent;

fn usage() -> ExitCode {
    eprintln!("usage: fixture_parent --parent <dir> --state-dir <dir> [--late-ops]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut parent: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut late = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--parent") => parent = args.next().map(PathBuf::from),
            Some("--state-dir") => state_dir = args.next().map(PathBuf::from),
            Some("--late-ops") => late = true,
            _ => return usage(),
        }
    }
    let (Some(parent), Some(state_dir)) = (parent, state_dir) else {
        return usage();
    };
    if late {
        return match fixture_parent::late_ops(&parent) {
            Ok((committed, edited)) => {
                println!("late-ops: committed {}", committed.display());
                println!("late-ops: edited {}", edited.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("fixture_parent: {e}");
                ExitCode::from(1)
            }
        };
    }
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        eprintln!("fixture_parent: create {}: {e}", state_dir.display());
        return ExitCode::from(1);
    }
    let built = match fixture_parent::build(&parent, &state_dir, &state_dir.join("home")) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fixture_parent: {e}");
            return ExitCode::from(1);
        }
    };
    let config = state_dir.join("config.toml");
    if let Err(e) = fixture_parent::write_config(&config, &built.parent) {
        eprintln!("fixture_parent: write {}: {e}", config.display());
        return ExitCode::from(1);
    }
    println!("parent={}", built.parent.display());
    println!("state_dir={}", state_dir.display());
    println!("config={}", config.display());
    println!("home={}", built.home.display());
    println!(
        "roots: {} {} {}",
        built.alpha.display(),
        built.beta.display(),
        built.notes.display()
    );
    ExitCode::SUCCESS
}
