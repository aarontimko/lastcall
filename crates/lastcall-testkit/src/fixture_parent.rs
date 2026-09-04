//! The golden's multi-root parent dir (kickoff deliverable 13): one directory `W` holding
//! `W/alpha` (repo A), `W/beta` (repo B) and `W/notes` (a non-git draft dir).
//!
//! [`build`] performs **first sight of all three roots before any history operation**, so
//! the golden exercises the seen-tree model rather than first sight:
//!
//! - repo A: an uncommitted edit (`f2`) and a later agent commit of `f1` (B1) — both stay
//!   pending, unannotated;
//! - repo B: a fast-forward pull of two coworker files (C2: `u1`, `u2` upstream), then an
//!   edit of `u2` on top (C6: mixed);
//! - the draft dir: one edited file (F3).
//!
//! Every path is deterministic (fixed identities and dates, see [`crate::fixture_repo`]),
//! so `status --json` over `W` is byte-stable once `W` itself is replaced by `<W>`.
//! [`late_ops`] is the `just probe-watch` half: a timed commit and edit in repo A while
//! `lastcall watch` runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lastcall_engine::config::Config;

use crate::engine::open_engine;
use crate::fixture_repo::{FixtureRepo, GitError, engine_env_for};
use crate::tmp::TempDir;

/// The draft dir's name under `W`; the config's `draft_dirs` entry.
pub const DRAFT_DIR: &str = "notes";

/// What the builder created; every path is as given (not canonicalized).
#[derive(Debug, Clone)]
pub struct Built {
    pub parent: PathBuf,
    pub alpha: PathBuf,
    pub beta: PathBuf,
    pub notes: PathBuf,
    /// The `HOME` the engine saw at first sight (under the state dir).
    pub home: PathBuf,
}

/// The engine config the binary must be given (as a file) to see the same three roots.
pub fn config() -> Config {
    Config {
        draft_dirs: vec![DRAFT_DIR.to_owned()],
        ..Config::default()
    }
}

/// The `config.toml` text for [`config`] with `parent_dirs = [parent]`.
pub fn config_toml(parent: &Path) -> String {
    format!(
        "parent_dirs = [\"{}\"]\ndraft_dirs = [\"{DRAFT_DIR}\"]\n",
        parent.display()
    )
}

/// Write [`config_toml`] to `path`.
pub fn write_config(path: &Path, parent: &Path) -> std::io::Result<()> {
    std::fs::write(path, config_toml(parent))
}

fn edit(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Build the fixture under `parent` (created if missing) with the engine's state in
/// `state_dir`. `parent` is never removed by this crate.
pub fn build(parent: &Path, state_dir: &Path) -> Result<Built, GitError> {
    let mut alpha = FixtureRepo::new_in(TempDir::adopt(parent), "alpha")?;
    let mut beta = FixtureRepo::new_in(TempDir::adopt(parent), "beta")?;
    let notes = parent.join(DRAFT_DIR);
    std::fs::create_dir_all(&notes).map_err(|e| io(&notes, &e))?;
    for i in 1..=3 {
        let p = notes.join(format!("n{i}.md"));
        edit(&p, &format!("# note {i}\n")).map_err(|e| io(&p, &e))?;
    }

    // First sight of all three roots, before any history operation.
    let home = state_dir.join("home");
    let env = engine_env_for(parent, &home, state_dir);
    let mut engine = open_engine(parent, &env, state_dir, config());
    let roots = engine.scan_all();
    assert_eq!(
        roots.len(),
        3,
        "three roots discovered under {}",
        parent.display()
    );
    for (root, _seq, result) in &roots {
        assert!(
            result.is_ok(),
            "first sight of {} failed: {result:?}",
            root.display()
        );
    }
    drop(engine);

    // (a) repo A: an uncommitted edit, then a later agent commit (B1).
    alpha.write("f2", "b\nagent edit\n");
    alpha.write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n");
    alpha.git(&["add", "f1"])?;
    alpha.commit("agent: rewrite f1 line 1")?;

    // (b) repo B: fast-forward pull of two coworker files (C2), one edited on top (C6).
    beta.coworker_push(2)?;
    beta.git(&["pull", "-q", "--ff-only"])?;
    beta.write("u2", "up2\nlocal edit\n");

    // (c) the draft dir: one edited file (F3).
    let n2 = notes.join("n2.md");
    edit(&n2, "# note 2\n\nedited\n").map_err(|e| io(&n2, &e))?;

    Ok(Built {
        parent: parent.to_path_buf(),
        alpha: alpha.path().to_path_buf(),
        beta: beta.path().to_path_buf(),
        notes,
        home,
    })
}

/// The `just probe-watch` half: against an already [`build`]-ed fixture, commit in repo A
/// at `t + 2 s` and edit a file there at `t + 4 s`. Returns the two paths touched.
pub fn late_ops(parent: &Path) -> Result<(PathBuf, PathBuf), GitError> {
    let mut alpha = FixtureRepo::open_in(TempDir::adopt(parent), "alpha");
    std::thread::sleep(Duration::from_secs(2));
    alpha.write("f3", "c\nlate commit\n");
    alpha.git(&["add", "f3"])?;
    alpha.commit("agent: late commit of f3")?;
    std::thread::sleep(Duration::from_secs(2));
    let f2 = alpha.write("f2", "b\nagent edit\nlate edit\n");
    Ok((alpha.path().join("f3"), f2))
}

fn io(path: &Path, e: &std::io::Error) -> GitError {
    GitError {
        args: vec!["<fs>".to_owned()],
        cwd: path.to_path_buf(),
        stderr: e.to_string(),
    }
}
