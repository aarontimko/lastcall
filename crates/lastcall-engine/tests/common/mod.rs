//! Shared scaffolding for the scenario suites (`test_integration_scenarios_*.rs`): the
//! harness's `fresh NAME` (a fixture repo, a state dir, an engine opened over it with first
//! sight done) and the accept helpers the scenarios drive.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use lastcall_engine::config::Config;
use lastcall_engine::engine::{Engine, EngineOptions, RestoreRequest, Restored};
use lastcall_engine::ops::{NoFault, Outcome, Rendered};
use lastcall_engine::scan::{Pile, Row};
use lastcall_testkit::engine::{open_engine, open_engine_with};
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;

/// `fresh NAME`: fixture repo + state dir + engine (first sight done at open).
pub struct Fresh {
    pub repo: FixtureRepo,
    pub state: TempDir,
    pub engine: Engine,
    pub root: PathBuf,
    config: Config,
    options: EngineOptions,
    parent: PathBuf,
}

impl Fresh {
    /// The engine's parent dir is the repo itself ("parent inside a repo → that repo").
    pub fn new(name: &str) -> Self {
        Self::with_config(name, Config::default())
    }

    pub fn with_config(name: &str, config: Config) -> Self {
        Self::with(name, config, EngineOptions::default(), false)
    }

    /// `parent_dirs = [the fixture's temp dir]`: the repo is a child root, so linked
    /// worktrees and draft dirs beside it are discovered too.
    pub fn with_parent_dir(name: &str, config: Config) -> Self {
        Self::with(name, config, EngineOptions::default(), true)
    }

    pub fn with(name: &str, config: Config, options: EngineOptions, parent_is_dir: bool) -> Self {
        let repo = FixtureRepo::new(name).expect("fixture repo");
        Self::over(repo, config, options, parent_is_dir)
    }

    /// Open over an existing fixture (after history setup that must precede first sight).
    pub fn over(
        repo: FixtureRepo,
        config: Config,
        options: EngineOptions,
        parent_is_dir: bool,
    ) -> Self {
        let state = TempDir::new("lc-scn-state");
        let parent = if parent_is_dir {
            repo.parent_dir().to_path_buf()
        } else {
            repo.path().to_path_buf()
        };
        let env = repo.engine_env(state.path());
        let engine = open_engine_with(&parent, &env, state.path(), config.clone(), options.clone());
        let root = engine
            .roots()
            .iter()
            .find(|r| r.path == std::fs::canonicalize(repo.path()).unwrap())
            .map(|r| r.path.clone())
            .expect("the fixture repo is a root");
        Self {
            repo,
            state,
            engine,
            root,
            config,
            options,
            parent,
        }
    }

    /// `lc_restart`: drop the engine and reopen it over the same state dir.
    pub fn restart(&mut self) {
        let env = self.repo.engine_env(self.state.path());
        self.engine = open_engine_with(
            &self.parent,
            &env,
            self.state.path(),
            self.config.clone(),
            self.options.clone(),
        );
    }

    /// `lc_restart` with a different configuration: what a user editing the config file
    /// and starting the tool again does. There is no reload seam, so this is the only way
    /// a scope change reaches an open root, and `restart` on its own reuses the stored
    /// configuration (`over` would build a new state dir and lose the record).
    pub fn restart_with(&mut self, config: Config) {
        self.config = config;
        self.restart();
    }

    pub fn scan(&mut self) -> Pile {
        self.engine.scan(&self.root).expect("scan")
    }

    pub fn row(&mut self, path: &str) -> Row {
        let pile = self.scan();
        pile.row(path.as_bytes())
            .unwrap_or_else(|| panic!("{path} is not pending: {:?}", pile_string(&pile)))
            .clone()
    }

    pub fn accept_file(&mut self, path: &str) -> Outcome {
        let rendered = Rendered::of(&self.row(path));
        self.engine
            .ops(&self.root)
            .unwrap()
            .accept_file(&rendered, &NoFault)
            .expect("accept_file")
    }

    pub fn accept_all(&mut self) -> Outcome {
        let pile = self.scan();
        self.accept_all_snapshot(&pile)
    }

    pub fn accept_all_snapshot(&mut self, snapshot: &Pile) -> Outcome {
        self.engine
            .ops(&self.root)
            .unwrap()
            .accept_all(snapshot, &NoFault)
            .expect("accept_all")
    }

    pub fn accept_hunk(&mut self, path: &str, index: usize) -> Outcome {
        let row = self.row(path);
        let rendered = Rendered::of(&row);
        self.engine
            .ops(&self.root)
            .unwrap()
            .accept_hunk(&rendered, &row.hunks, index, &NoFault)
            .expect("accept_hunk")
    }

    /// Restore the whole file (or, on a deletion row, put the file back) through
    /// [`Engine::restore`], so the scenarios exercise the same critical section the TUI
    /// will: the op and the rescan together.
    pub fn restore_file(&mut self, path: &str) -> Restored {
        let rendered = Rendered::of(&self.row(path));
        self.engine
            .restore(&self.root, RestoreRequest::File(rendered))
            .expect("restore file")
    }

    /// Restore one hunk of `path`.
    pub fn restore_hunk(&mut self, path: &str, index: usize) -> Restored {
        let row = self.row(path);
        self.engine
            .restore(
                &self.root,
                RestoreRequest::Hunk {
                    rendered: Rendered::of(&row),
                    hunks: row.hunks.clone(),
                    index,
                },
            )
            .expect("restore hunk")
    }

    /// The bytes on disk at `rel`, read without following a symlink at the leaf.
    pub fn bytes_at(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.repo.path().join(rel)).expect("read the working file")
    }

    /// `inspect_head` and return the notice (panics when HEAD did not move).
    pub fn head_notice(&mut self) -> Option<String> {
        self.engine
            .inspect_head(&self.root)
            .expect("inspect_head")
            .expect("HEAD moved")
            .notice
    }

    pub fn ledger(&self) -> &lastcall_engine::ledger::Ledger {
        &self.engine.root(&self.root).unwrap().ledger
    }

    pub fn store(&self) -> &lastcall_engine::store::Store {
        &self.engine.root(&self.root).unwrap().store
    }

    pub fn head(&self) -> String {
        self.repo.head().unwrap()
    }

    pub fn tree_of_head(&self) -> String {
        self.repo
            .git(&["rev-parse", "HEAD^{tree}"])
            .unwrap()
            .trim()
            .to_owned()
    }
}

pub fn pile_string(pile: &Pile) -> String {
    lastcall_testkit::engine::pile_string(pile)
}

pub fn open_plain(
    parent: &Path,
    env: &lastcall_engine::env::Env,
    state: &Path,
    config: Config,
) -> Engine {
    open_engine(parent, env, state, config)
}

// ---------------------------------------------------------------------------------------
// Plumbing: commits and branches built without a checkout.
// ---------------------------------------------------------------------------------------

/// One git plumbing command against `cwd` with an index file of its own, a fixed identity,
/// a fixed date and an optional stdin.
///
/// [`FixtureRepo::git`] removes `GIT_INDEX_FILE` and offers no stdin, so a commit built
/// without touching the working tree needs its own runner.
fn plumb(cwd: &Path, index: &Path, date: &str, args: &[&str], stdin: Option<&[u8]>) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_INDEX_FILE", index)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Me")
        .env("GIT_AUTHOR_EMAIL", "me@example.com")
        .env("GIT_COMMITTER_NAME", "Me")
        .env("GIT_COMMITTER_EMAIL", "me@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("git output");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A commit built from `base`'s tree with `sets` written over it (`None` removes the path),
/// carrying `parents`, dated `after` seconds past the current HEAD, with
/// `refs/heads/<name>` moved to it. Returns the new commit.
///
/// Nothing is checked out and the repository's own index is never touched, so lastcall
/// never sees the branch while this runs and the branch gets no record of its own.
pub fn plumb_commit(
    repo: &FixtureRepo,
    name: &str,
    base: &str,
    parents: &[&str],
    sets: &[(&str, Option<&str>)],
    message: &str,
    after: u64,
) -> String {
    let cwd = repo.path();
    let head_at: u64 = repo
        .git(&["log", "-1", "--format=%ct", "HEAD"])
        .expect("HEAD date")
        .trim()
        .parse()
        .expect("a unix timestamp");
    let date = format!("{} +0000", head_at + after);
    let index = repo.parent_dir().join(format!("plumb-{name}.index"));
    let _ = std::fs::remove_file(&index);
    plumb(cwd, &index, &date, &["read-tree", base], None);
    for (path, contents) in sets {
        match contents {
            Some(c) => {
                let blob = plumb(
                    cwd,
                    &index,
                    &date,
                    &["hash-object", "-w", "--stdin"],
                    Some(c.as_bytes()),
                );
                plumb(
                    cwd,
                    &index,
                    &date,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{blob},{path}"),
                    ],
                    None,
                );
            }
            None => {
                plumb(
                    cwd,
                    &index,
                    &date,
                    &["update-index", "--force-remove", path],
                    None,
                );
            }
        }
    }
    let tree = plumb(cwd, &index, &date, &["write-tree"], None);
    let mut args: Vec<String> = vec![
        "commit-tree".to_owned(),
        tree,
        "-m".to_owned(),
        message.to_owned(),
    ];
    for p in parents {
        args.push("-p".to_owned());
        args.push((*p).to_owned());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let commit = plumb(cwd, &index, &date, &argv, None);
    plumb(
        cwd,
        &index,
        &date,
        &["update-ref", &format!("refs/heads/{name}"), &commit],
        None,
    );
    let _ = std::fs::remove_file(&index);
    commit
}

/// [`plumb_commit`] for the common case: a branch cut from `base` with one commit on top.
pub fn plumb_branch(
    repo: &FixtureRepo,
    name: &str,
    base: &str,
    sets: &[(&str, Option<&str>)],
) -> String {
    plumb_commit(
        repo,
        name,
        base,
        &[base],
        sets,
        &format!("{name} built without a checkout"),
        1,
    )
}
