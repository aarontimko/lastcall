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

/// The **opt-in** gitignored draft dir inside `W/alpha` ([`add_draft_root`]).
pub const DRAFT_SUBDIR: &str = "_drafts";
/// The file [`add_draft_root`] puts in it.
pub const DRAFT_SUBDIR_FILE: &str = "reply.md";

/// The file [`build`] adds to **`alpha` only** (Phase 8 deliverable 10, ruling P7), path
/// relative to that root.
pub const PARSE_RS: &str = "src/parse.rs";

/// [`PARSE_RS`] as it is **committed**, before the first sight of any root: the baseline the
/// review is against.
///
/// Plausible Rust with real structure — a module doc, a struct, two functions and a test
/// module — because the probe and the editing scenes want something a reviewer would
/// recognise, and because `f1` (`a1`..`a10`, one hunk at line 1) can prove nothing about a
/// line number (design review F5).
pub const PARSE_RS_BASE: &str = r##"//! A tiny line-oriented parser for the fixture's sample config text.

/// One parsed record: a key, its value, and the line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// Parse `text` into one record per `key = value` line.
///
/// Blank lines and `#` comments are skipped. A line without a `=` is not an
/// error: it is simply not a record, which keeps the parser total.
pub fn parse(text: &str) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        out.push(Record {
            key: key.trim().to_owned(),
            value: value.trim().to_owned(),
            line: i + 1,
        });
    }
    out
}

/// The value of the last record with `key`, if any: a later line wins.
pub fn lookup<'a>(records: &'a [Record], key: &str) -> Option<&'a str> {
    records
        .iter()
        .rev()
        .find(|r| r.key == key)
        .map(|r| r.value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let records = parse("# a comment\n\nname = lastcall\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "name");
        assert_eq!(records[0].line, 3);
    }
}
"##;

/// [`PARSE_RS`] after "the agent" rewrote it: **three separated places**, so the row has
/// three hunks with real leading context.
///
/// 1. the module doc at the top (one line becomes three);
/// 2. the comment-skipping condition in the middle — the hunk the editing scenes open at,
///    deliberately neither line 1 nor a context line (design review F9);
/// 3. a second test at the bottom.
///
/// The three are far enough apart that context 3 cannot merge them; [`PARSE_RS_EDIT2`] is
/// the marker a test finds to compute the middle hunk's editor line for itself.
pub const PARSE_RS_EDITED: &str = r##"//! A tiny line-oriented parser for `key = value` text.
//!
//! Comment syntax follows the sample files: `#` to the end of the line.

/// One parsed record: a key, its value, and the line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// Parse `text` into one record per `key = value` line.
///
/// Blank lines and `#` comments are skipped. A line without a `=` is not an
/// error: it is simply not a record, which keeps the parser total.
pub fn parse(text: &str) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        out.push(Record {
            key: key.trim().to_owned(),
            value: value.trim().to_owned(),
            line: i + 1,
        });
    }
    out
}

/// The value of the last record with `key`, if any: a later line wins.
pub fn lookup<'a>(records: &'a [Record], key: &str) -> Option<&'a str> {
    records
        .iter()
        .rev()
        .find(|r| r.key == key)
        .map(|r| r.value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let records = parse("# a comment\n\nname = lastcall\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "name");
        assert_eq!(records[0].line, 3);
    }

    #[test]
    fn lookup_takes_the_last_record() {
        let records = parse("a = 1\na = 2\n");
        assert_eq!(lookup(&records, "a"), Some("2"));
    }
}
"##;

/// The **added** line of the middle hunk (agent edit 2). Its one-based position in
/// [`PARSE_RS_EDITED`] is exactly `Hunk::editor_line()` for that hunk: the walk stops at the
/// hunk's first non-context line, which for a replacement lands on the new side's insert.
pub const PARSE_RS_EDIT2: &str =
    r#"        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {"#;

/// The one-based line of [`PARSE_RS_EDIT2`] in [`PARSE_RS_EDITED`] — what `shift-i` and `i`
/// on the middle hunk must open at, computed from the fixture text rather than from the
/// engine that is under test.
pub fn parse_rs_edit2_line() -> usize {
    PARSE_RS_EDITED
        .lines()
        .position(|l| l == PARSE_RS_EDIT2)
        .expect("the middle hunk's added line is in the edited text")
        + 1
}

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

/// [`config`] plus the opt-in `_drafts` entry — the config a scene that called
/// [`add_draft_root`] must use, in-process and in its `config.toml` alike.
pub fn draft_config() -> Config {
    Config {
        draft_dirs: vec![DRAFT_DIR.to_owned(), DRAFT_SUBDIR.to_owned()],
        ..Config::default()
    }
}

/// The `config.toml` text for [`draft_config`] with `parent_dirs = [parent]`.
pub fn draft_config_toml(parent: &Path) -> String {
    format!(
        "parent_dirs = [\"{}\"]\ndraft_dirs = [\"{DRAFT_DIR}\", \"{DRAFT_SUBDIR}\"]\n",
        parent.display()
    )
}

/// Write [`draft_config_toml`] to `path`.
pub fn write_draft_config(path: &Path, parent: &Path) -> std::io::Result<()> {
    std::fs::write(path, draft_config_toml(parent))
}

/// The Phase 6 draft-root scenes' **opt-in fourth root** (kickoff deliverable 1): a
/// gitignored `_drafts/` inside `W/alpha`, holding `reply.md` at `baseline`, first-sighted
/// (all four roots, `draft_initial = seen`) so a later edit is the pending delta rather
/// than the baseline. Returns the draft root's canonical path.
///
/// This is deliberately **not** part of [`build`] and never will be. The status golden,
/// every three-root `.snap` (`3 repos` in the header), every PTY scene (`scanning 3 roots…`)
/// and `test_integration_herdr_worktree.rs` assert three roots; a fourth root in the shared
/// fixture breaks all of them at once. A scene that wants one calls this, and must then use
/// [`draft_config`] / [`draft_config_toml`] everywhere it opens an engine — including the
/// `config.toml` the binary reads, or the child would discover only three roots.
///
/// `alpha`'s own pile grows by the `.gitignore` this writes (a file added after alpha's
/// first sight is pending, like any other): that is the shape of a real gitignored draft
/// dir, and a caller that cares can accept it.
pub fn add_draft_root(
    built: &Built,
    state_dir: &Path,
    baseline: &str,
) -> Result<PathBuf, GitError> {
    let gitignore = built.alpha.join(".gitignore");
    let mut text = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{DRAFT_SUBDIR}/\n"));
    edit(&gitignore, &text).map_err(|e| io(&gitignore, &e))?;

    let drafts = built.alpha.join(DRAFT_SUBDIR);
    std::fs::create_dir_all(&drafts).map_err(|e| io(&drafts, &e))?;
    let reply = drafts.join(DRAFT_SUBDIR_FILE);
    edit(&reply, baseline).map_err(|e| io(&reply, &e))?;

    // First sight of the new root, at `baseline`, before anything edits it.
    let env = engine_env_for(&built.parent, &built.home, state_dir);
    let mut engine = open_engine(&built.parent, &env, state_dir, draft_config());
    let roots = engine.scan_all();
    assert_eq!(
        roots.len(),
        4,
        "four roots discovered under {} once `_drafts` is configured",
        built.parent.display()
    );
    for (root, _seq, result) in &roots {
        assert!(
            result.is_ok(),
            "first sight of {} failed: {result:?}",
            root.display()
        );
    }
    drop(engine);
    std::fs::canonicalize(&drafts).map_err(|e| io(&drafts, &e))
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

    // The richer file (deliverable 10, ruling P7) goes in **here**, not in
    // `FixtureRepo::new_in`, which seeds *every* repo including beta and the coworker clone
    // (design review F7) — and it is committed **before** the first sight below, so the
    // review sees the agent's three edits as the delta and not the whole file.
    let src = alpha.path().join("src");
    std::fs::create_dir_all(&src).map_err(|e| io(&src, &e))?;
    let parse_rs = alpha.path().join(PARSE_RS);
    edit(&parse_rs, PARSE_RS_BASE).map_err(|e| io(&parse_rs, &e))?;
    alpha.git(&["add", PARSE_RS])?;
    alpha.commit("add the config parser")?;

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

    // (a) repo A: an uncommitted edit, then a later agent commit (B1). `src/parse.rs` gets
    // the agent's three separated edits and is **not** committed: three hunks with leading
    // context, which is what the editing and copy scenes need.
    edit(&parse_rs, PARSE_RS_EDITED).map_err(|e| io(&parse_rs, &e))?;
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
