//! Shared scaffolding for the scenario suites (`test_integration_scenarios_*.rs`): the
//! harness's `fresh NAME` (a fixture repo, a state dir, an engine opened over it with first
//! sight done) and the accept helpers the scenarios drive.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use lastcall_engine::config::Config;
use lastcall_engine::engine::{Engine, EngineOptions};
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
