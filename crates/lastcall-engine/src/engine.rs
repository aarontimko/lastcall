//! The engine: roots, ledgers, scans, head inspection and ops in one place (kickoff
//! deliverable 12). No terminal code, no process environment: everything comes through the
//! injected [`Env`] and the loaded config.
//!
//! Opening a root performs **first sight** (docs/spec/00-spec.md §6.2) when it has no
//! ledger: a git root's seen tree is `HEAD^{tree}` (`null` before the first commit) and
//! `seen_at` records HEAD; a draft root follows `draft_initial`. An unreadable ledger is
//! moved aside and the root opens with a `null` seen tree — over-show, never hide. A seen
//! tree that no longer resolves in the store (the user ran `git gc`) is treated as `null`
//! with a notice.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::config::{Config, DraftInitial, Loaded, Resolved};
use crate::env::Env;
use crate::git::{self, GitError, Oid, RepoGit};
use crate::headstate::{self, HeadState, TransitionFacts};
use crate::index::{IndexError, PrivateIndex};
use crate::ledger::{
    self, Clock, Ledger, LedgerError, LedgerLock, LoadResult, SeenAt, SystemClock, TreeEntries,
};
use crate::ops::{Ops, OpsError};
use crate::paths::{Layout, ParentId, ParentMeta, RepoPaths, RootId};
use crate::roots::{self, Badge, DiscoverInputs, Discovery, RootsChanged};
use crate::scan::{self, Pile, ScanError, ScanInputs};
use crate::store::{RootKind, Store, StoreError};
use crate::upstream::{self, Classifier};

/// The oldest git the engine accepts (`--path-format=absolute`, `ls-files --others -z`
/// semantics we rely on).
pub const MIN_GIT: (u32, u32) = git::MIN_GIT_VERSION;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git {found} is too old; lastcall needs {}.{} or newer", MIN_GIT.0, MIN_GIT.1)]
    GitTooOld { found: String },
    #[error("git is not available: {0}")]
    NoGit(String),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Ops(#[from] OpsError),
    #[error("no such root: {}", .0.display())]
    NoSuchRoot(PathBuf),
}

fn io_err(path: &Path, source: std::io::Error) -> EngineError {
    EngineError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Injectable knobs.
#[derive(Clone)]
pub struct EngineOptions {
    /// Compaction runs after a ledger write when more overrides than this carry a blob.
    pub compaction_threshold: usize,
    pub clock: Arc<dyn Clock + Send + Sync>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            compaction_threshold: 500,
            clock: Arc::new(SystemClock),
        }
    }
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineOptions")
            .field("compaction_threshold", &self.compaction_threshold)
            .finish_non_exhaustive()
    }
}

/// Everything the engine holds for one root.
pub struct RootState {
    pub path: PathBuf,
    pub kind: RootKind,
    pub parent: PathBuf,
    pub badge: Option<Badge>,
    pub paths: RepoPaths,
    pub store: Store,
    pub index: PrivateIndex,
    pub repo: Option<RepoGit>,
    pub ledger: Ledger,
    /// `ls-tree -r` of the effective seen tree.
    pub tree: TreeEntries,
    pub head: HeadState,
    pub classifier: Classifier,
    pub case_insensitive: bool,
    /// Root-relative draft roots inside this git root (their untracked files are theirs).
    pub excluded_dirs: Vec<Vec<u8>>,
    /// Notices from open (kept until the root is reopened).
    pub notices: Vec<String>,
    pub last_pile: Option<Pile>,
    pub nested_repos: Vec<Vec<u8>>,
    pub user_email: Option<String>,
}

impl RootState {
    pub fn seen_head(&self) -> Option<&Oid> {
        self.ledger.seen_at.head_commit.as_ref()
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// What a head inspection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadChange {
    pub root: PathBuf,
    pub from: Option<Oid>,
    pub to: Option<Oid>,
    pub branch: Option<String>,
    pub notice: Option<String>,
    /// The pile from the scan that followed.
    pub pile: Pile,
}

pub struct Engine {
    env: Env,
    config: Config,
    resolved: Resolved,
    layout: Layout,
    options: EngineOptions,
    roots: BTreeMap<PathBuf, RootState>,
    discovery: Discovery,
    collapsed: GlobSet,
    ignore: GlobSet,
    /// Engine-level notices (config resolution, discovery).
    notices: Vec<String>,
    pending_nested: Vec<(PathBuf, PathBuf)>,
    git_version: String,
}

impl Engine {
    /// Open the engine: check git, prepare the state dir, discover roots, open ledgers.
    pub fn open(
        loaded: &Loaded,
        resolved: &Resolved,
        env: &Env,
        options: EngineOptions,
    ) -> Result<Engine, EngineError> {
        let git_version = check_git(env)?;
        let layout = Layout::new(&loaded.state_dir);
        std::fs::create_dir_all(layout.roots_dir()).map_err(|e| io_err(&layout.roots_dir(), e))?;
        let collapsed = build_globs(&loaded.config.collapsed_globs);
        let ignore = build_globs(&loaded.config.ignore_globs);
        let mut engine = Engine {
            env: env.clone(),
            config: loaded.config.clone(),
            resolved: resolved.clone(),
            layout,
            options,
            roots: BTreeMap::new(),
            discovery: Discovery::default(),
            collapsed,
            ignore,
            notices: resolved.notices.clone(),
            pending_nested: Vec::new(),
            git_version,
        };
        engine.rescan()?;
        Ok(engine)
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn options(&self) -> &EngineOptions {
        &self.options
    }

    pub fn git_version(&self) -> &str {
        &self.git_version
    }

    pub fn ignore_globs(&self) -> &GlobSet {
        &self.ignore
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Roots sorted by path bytes.
    pub fn roots(&self) -> Vec<&RootState> {
        let mut v: Vec<&RootState> = self.roots.values().collect();
        v.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
        v
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots().into_iter().map(|r| r.path.clone()).collect()
    }

    pub fn root(&self, path: &Path) -> Option<&RootState> {
        self.roots.get(path)
    }

    pub fn root_mut(&mut self, path: &Path) -> Option<&mut RootState> {
        self.roots.get_mut(path)
    }

    /// Resolve a user-supplied path (any directory inside a root) to the root's key.
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        let canon = std::fs::canonicalize(path).ok()?;
        self.roots
            .keys()
            .filter(|r| canon.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())
            .cloned()
    }

    /// Re-run discovery; open new roots, drop removed ones (ledgers kept on disk).
    pub fn rescan(&mut self) -> Result<RootsChanged, EngineError> {
        let mut nested = std::mem::take(&mut self.pending_nested);
        for root in self.roots.values() {
            for n in &root.nested_repos {
                nested.push((
                    root.path.clone(),
                    root.path.join(std::ffi::OsStr::from_bytes(n)),
                ));
            }
        }
        let next = roots::discover(&DiscoverInputs {
            env: &self.env,
            parent_dirs: &self.resolved.parent_dirs,
            draft_dirs: &self.config.draft_dirs,
            nested: &nested,
        });
        let changed = roots::diff(&self.discovery, &next);
        for n in &next.notices {
            if !self.notices.contains(n) {
                self.notices.push(n.clone());
            }
        }
        for removed in &changed.removed {
            self.roots.remove(removed);
        }
        self.discovery = next;
        let to_open: Vec<roots::DiscoveredRoot> = self
            .discovery
            .roots
            .iter()
            .filter(|r| !self.roots.contains_key(&r.path))
            .cloned()
            .collect();
        for d in to_open {
            match self.open_root(&d) {
                Ok(state) => {
                    self.roots.insert(d.path.clone(), state);
                }
                Err(e) => self
                    .notices
                    .push(format!("{}: cannot open: {e}", d.path.display())),
            }
        }
        // Badges and excluded draft dirs can change with the root set.
        let discovery = self.discovery.clone();
        for root in self.roots.values_mut() {
            if let Some(d) = discovery.get(&root.path) {
                root.badge = d.badge.clone();
            }
            root.excluded_dirs = if root.kind == RootKind::Git {
                discovery.draft_dirs_inside(&root.path)
            } else {
                Vec::new()
            };
        }
        Ok(changed)
    }

    fn open_root(&self, d: &roots::DiscoveredRoot) -> Result<RootState, EngineError> {
        let parent_id = ParentId::of(&d.parent);
        let root_id = RootId::of(&d.path);
        let paths = self.layout.repo_paths(&parent_id, &root_id);
        std::fs::create_dir_all(&paths.repo_dir).map_err(|e| io_err(&paths.repo_dir, e))?;
        let meta = self.layout.meta_path(&parent_id);
        if !meta.exists() {
            let m = ParentMeta {
                schema_version: ledger::SCHEMA_VERSION.to_string(),
                parent: d.parent.to_string_lossy().into_owned(),
                created_at: self.options.clock.now_iso8601(),
            };
            let text = serde_json::to_string_pretty(&m).unwrap_or_default();
            std::fs::write(&meta, text).map_err(|e| io_err(&meta, e))?;
        }
        let repo = (d.kind == RootKind::Git).then(|| RepoGit::new(&self.env, &d.path));
        let (store, mut notices) = Store::open(&self.env, &d.path, d.kind, &paths, repo.as_ref())?;
        let exclude_from = repo
            .as_ref()
            .and_then(|rg| rg.git_path("info/exclude").ok());
        let index = PrivateIndex::new(store.git().clone(), &paths, d.kind, exclude_from);
        let head = match &repo {
            Some(rg) => headstate::inspect(rg)?,
            None => HeadState::none(),
        };
        let user_email = repo
            .as_ref()
            .and_then(|rg| rg.config_get("user.email").ok().flatten())
            .filter(|e| !e.is_empty());

        // A `ledger.json.tmp` left by a crash between write and rename (E1) is garbage:
        // the rename never happened, so `ledger.json` is still the previous version. A
        // live writer holds the lock between its write and rename, so the tmp is judged
        // under the lock: once we hold it, any tmp still there is stale.
        let stale_tmp = paths.ledger.with_extension("json.tmp");
        if stale_tmp.exists() {
            let _lock = LedgerLock::acquire(&paths)?;
            if stale_tmp.exists() {
                let _ = std::fs::remove_file(&stale_tmp);
                notices
                    .push("removed a stale ledger.json.tmp from an interrupted write".to_owned());
            }
        }
        let clock: &dyn Clock = self.options.clock.as_ref();
        let mut ledger = match ledger::load(&paths, clock)? {
            LoadResult::Loaded { ledger, notices: n } => {
                notices.extend(n);
                ledger
            }
            first => {
                // Nothing usable on disk. Whatever is written now is written under the
                // lock after a second look, so two processes opening the same never-seen
                // root cannot clobber each other's first sight (or an accept in between),
                // and a corrupt ledger moved aside by one of them is never mistaken for
                // "never seen" by the other: a moved-aside sibling with no ledger beside
                // it can only mean that, and it opens with nothing seen (over-shows).
                let _lock = LedgerLock::acquire(&paths)?;
                match ledger::load(&paths, clock)? {
                    LoadResult::Loaded { ledger, notices: n } => {
                        notices.extend(n);
                        notices.push("ledger written by another process while opening".to_owned());
                        ledger
                    }
                    second => {
                        let unreadable = match (first, second) {
                            (LoadResult::Unreadable { moved_to, reason }, _)
                            | (_, LoadResult::Unreadable { moved_to, reason }) => {
                                Some((moved_to, reason))
                            }
                            _ => ledger::moved_aside_sibling(&paths)
                                .map(|p| (p, "moved aside by another process".to_owned())),
                        };
                        match unreadable {
                            Some((moved_to, reason)) => {
                                notices.push(format!(
                                    "ledger unreadable ({reason}); moved to {} and reopened with nothing seen",
                                    moved_to.display()
                                ));
                                // Persisted at once, with no `seen_at`: the next open must
                                // find *this* ledger, not fall to first sight at the current
                                // HEAD (which would hide everything committed since the old
                                // ledger was last good).
                                let l = Ledger::new(
                                    &d.path,
                                    d.kind,
                                    None,
                                    SeenAt {
                                        head_commit: None,
                                        branch: None,
                                        at: clock.now_iso8601(),
                                    },
                                );
                                ledger::save(&paths, &l)?;
                                l
                            }
                            None => {
                                // E4: a sibling ledger whose root no longer exists is
                                // probably this root under its old name. First-sight rules
                                // apply; say where the old state is.
                                for (old_root, dir) in
                                    orphaned_ledgers(&self.layout.repos_dir(&parent_id))
                                {
                                    notices.push(format!(
                                        "first sight; previous state for {old_root} (no longer on disk) is kept at {}",
                                        dir.display()
                                    ));
                                }
                                let l = first_sight(
                                    &d.path,
                                    d.kind,
                                    &store,
                                    &head,
                                    self.config.draft_initial,
                                    clock,
                                )?;
                                ledger::save(&paths, &l)?;
                                l
                            }
                        }
                    }
                }
            }
        };
        if let Some(t) = ledger.seen_tree.clone()
            && !store.exists(&t)
        {
            notices.push(format!(
                "seen tree {t} no longer exists in the object store (git gc?); treating everything as unseen"
            ));
            ledger.seen_tree = None;
        }
        let tree = match &ledger.seen_tree {
            Some(t) => store.ls_tree(t)?,
            None => TreeEntries::new(),
        };
        Ok(RootState {
            path: d.path.clone(),
            kind: d.kind,
            parent: d.parent.clone(),
            badge: d.badge.clone(),
            case_insensitive: scan::probe_case_insensitive(&d.path),
            paths,
            store,
            index,
            repo,
            ledger,
            tree,
            head,
            classifier: Classifier::default(),
            excluded_dirs: Vec::new(),
            notices,
            last_pile: None,
            nested_repos: Vec::new(),
            user_email,
        })
    }

    /// Scan one root: candidates → rows → annotation.
    pub fn scan(&mut self, root: &Path) -> Result<Pile, EngineError> {
        let collapsed = self.collapsed.clone();
        let collapse_size = self.config.collapse_size_bytes;
        let state = self
            .roots
            .get_mut(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        let out = scan::scan(&ScanInputs {
            store: &state.store,
            index: &state.index,
            repo: state.repo.as_ref(),
            ledger: &state.ledger,
            seen_tree: state.ledger.seen_tree.as_ref(),
            tree: &state.tree,
            case_insensitive: state.case_insensitive,
            collapsed_globs: &collapsed,
            collapse_size_bytes: collapse_size,
            excluded_dirs: &state.excluded_dirs,
            index_tmp: &state.paths.index_tmp,
        })?;
        let mut pile = out.pile;
        if out.nested_repos != state.nested_repos {
            state.nested_repos = out.nested_repos;
        }
        if let Some(rg) = &state.repo {
            // Classify against the *live* HEAD, not the last inspected one: a pull or
            // rebase changes files before (or without) a git-dir event reaching
            // `inspect_head`, and the pile must never depend on that ordering.
            // `state.head` stays the last *reported* state so the transition notice
            // is still emitted exactly once by `inspect_head`.
            // Annotation is a label layer: none of its inputs failing may drop a row.
            let seen_head = state.ledger.seen_at.head_commit.clone();
            let skipped = match headstate::inspect(rg) {
                Err(e) => Some(format!("head inspection skipped: {e}")),
                Ok(live) => match state.classifier.get(
                    rg,
                    seen_head.as_ref(),
                    &live,
                    state.user_email.as_deref(),
                ) {
                    Err(e) => Some(format!("upstream classification skipped: {e}")),
                    Ok(class) => upstream::annotate(&mut pile, class, rg)
                        .err()
                        .map(|e| format!("upstream annotation skipped: {e}")),
                },
            };
            if let Some(n) = skipped {
                pile.notices.push(n);
            }
        }
        state.last_pile = Some(pile.clone());
        Ok(pile)
    }

    /// Scan every root (opening nested repositories discovered on the way).
    pub fn scan_all(&mut self) -> Vec<(PathBuf, Result<Pile, EngineError>)> {
        let mut results: BTreeMap<PathBuf, Result<Pile, EngineError>> = BTreeMap::new();
        for _ in 0..3 {
            let todo: Vec<PathBuf> = self
                .root_paths()
                .into_iter()
                .filter(|p| !results.contains_key(p))
                .collect();
            if todo.is_empty() {
                break;
            }
            for p in todo {
                let r = self.scan(&p);
                results.insert(p, r);
            }
            let has_new_nested = self.roots.values().any(|r| !r.nested_repos.is_empty());
            if !has_new_nested {
                break;
            }
            match self.rescan() {
                Ok(changed) if !changed.added.is_empty() => continue,
                _ => break,
            }
        }
        let mut v: Vec<(PathBuf, Result<Pile, EngineError>)> = results.into_iter().collect();
        v.sort_by(|a, b| a.0.as_os_str().cmp(b.0.as_os_str()));
        v
    }

    /// Re-inspect HEAD; when it moved (or an operation finished), scan and describe it.
    pub fn inspect_head(&mut self, root: &Path) -> Result<Option<HeadChange>, EngineError> {
        let state = self
            .roots
            .get(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        let Some(rg) = &state.repo else {
            return Ok(None);
        };
        let next = headstate::inspect(rg)?;
        let prev = state.head.clone();
        if next == prev {
            return Ok(None);
        }
        let commits = match (&prev.head, &next.head) {
            (Some(a), Some(b)) if a != b => rg
                .run(&["rev-list", "--count", &format!("{a}..{b}")])
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o).trim().parse::<u64>().ok()),
            _ => None,
        };
        let hint = headstate::last_reflog(&next.git_dir);
        self.roots.get_mut(root).expect("checked").head = next.clone();
        let pile = self.scan(root)?;
        let facts = TransitionFacts {
            commits,
            files_differ: pile.rows.len(),
        };
        let notice = headstate::transition(&prev, &next, hint.as_ref(), &facts);
        Ok(Some(HeadChange {
            root: root.to_path_buf(),
            from: prev.head,
            to: next.head.clone(),
            branch: next.branch.clone(),
            notice,
            pile,
        }))
    }

    /// The accept operations for one root.
    pub fn ops(&mut self, root: &Path) -> Result<Ops<'_>, EngineError> {
        let threshold = self.options.compaction_threshold;
        let clock: &dyn Clock = self.options.clock.as_ref();
        let state = self
            .roots
            .get_mut(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        Ok(Ops {
            store: &state.store,
            index: &state.index,
            repo: state.repo.as_ref(),
            paths: &state.paths,
            ledger: &mut state.ledger,
            tree: &mut state.tree,
            clock,
            compaction_threshold: threshold,
            staged: BTreeMap::new(),
        })
    }
}

/// `git --version` must be ≥ [`MIN_GIT`].
pub fn check_git(env: &Env) -> Result<String, EngineError> {
    let v = git::git_version(env).map_err(|e| EngineError::NoGit(e.to_string()))?;
    let found = format!("{}.{}.{}", v.0, v.1, v.2);
    if !git::version_supported(v) {
        return Err(EngineError::GitTooOld { found });
    }
    Ok(found)
}

/// Build a glob set where a bare name (no `/`) also matches at any depth.
pub fn build_globs(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let add = |b: &mut GlobSetBuilder, pat: &str| {
            if let Ok(g) = GlobBuilder::new(pat).literal_separator(false).build() {
                b.add(g);
            }
        };
        add(&mut b, p);
        if !p.contains('/') {
            add(&mut b, &format!("**/{p}"));
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

/// Ledgers under a parent's `repos/` whose `root` path no longer exists on disk (E4).
fn orphaned_ledgers(repos_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(repos_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(bytes) = std::fs::read(dir.join("ledger.json")) else {
            continue;
        };
        let Ok((ledger, _)) = ledger::parse(&bytes) else {
            continue;
        };
        if !Path::new(&ledger.root).exists() {
            out.push((ledger.root.clone(), dir));
        }
    }
    out.sort();
    out
}

/// §6.2 first sight.
fn first_sight(
    root: &Path,
    kind: RootKind,
    store: &Store,
    head: &HeadState,
    draft_initial: DraftInitial,
    clock: &dyn Clock,
) -> Result<Ledger, EngineError> {
    let seen_tree = match kind {
        RootKind::Git => match &head.head {
            Some(h) => {
                let spec = format!("{h}^{{tree}}");
                store
                    .git()
                    .run(&["rev-parse", "--verify", &spec])
                    .ok()
                    .and_then(|o| Oid::parse(String::from_utf8_lossy(&o).trim()))
            }
            None => None,
        },
        RootKind::Draft => match draft_initial {
            DraftInitial::Seen => Some(store.tree_of_disk()?),
            DraftInitial::Pending => None,
        },
    };
    Ok(Ledger::new(
        root,
        kind,
        seen_tree,
        SeenAt {
            head_commit: head.head.clone(),
            branch: head.branch.clone(),
            at: clock.now_iso8601(),
        },
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::ConfigSource;
    use crate::ops::{NoFault, Rendered};
    use crate::status::StatusReport;
    use crate::store::tests::fixture_env;
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    /// A `Loaded`/`Resolved` pair watching the fixture's parent dir.
    pub(crate) fn loaded_for(
        repo: &FixtureRepo,
        state: &TempDir,
        config: Config,
    ) -> (Loaded, Resolved) {
        let parent = std::fs::canonicalize(repo.parent_dir()).unwrap();
        let loaded = Loaded {
            config: Config {
                parent_dirs: vec![parent.clone()],
                ..config
            },
            source: ConfigSource::Defaults { searched: vec![] },
            state_dir: state.path().to_path_buf(),
        };
        let resolved = Resolved {
            parent_dirs: vec![parent],
            notices: vec![],
        };
        (loaded, resolved)
    }

    pub(crate) fn open_engine(repo: &FixtureRepo, state: &TempDir, config: Config) -> Engine {
        let (loaded, resolved) = loaded_for(repo, state, config);
        let env = fixture_env(repo, state);
        Engine::open(&loaded, &resolved, &env, EngineOptions::default()).unwrap()
    }

    fn only_root(engine: &Engine) -> PathBuf {
        let roots = engine.roots();
        assert_eq!(roots.len(), 1, "{:?}", engine.root_paths());
        roots[0].path.clone()
    }

    #[test]
    fn engine_open_first_sight_scan_and_stable_status() {
        let repo = FixtureRepo::new("eng").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let r = engine.root(&root).unwrap();
        assert_eq!(r.kind, RootKind::Git);
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let tree = repo
            .git(&["rev-parse", "HEAD^{tree}"])
            .unwrap()
            .trim()
            .to_owned();
        assert_eq!(
            r.ledger.seen_at.head_commit.as_ref().map(Oid::as_str),
            Some(head.as_str())
        );
        assert_eq!(
            r.ledger.seen_tree.as_ref().map(Oid::as_str),
            Some(tree.as_str())
        );
        assert!(
            engine.scan(&root).unwrap().is_empty(),
            "clean after first sight"
        );

        repo.write("f1", "changed\n");
        repo.write("new.txt", "hello\n");
        let pile = engine.scan(&root).unwrap();
        assert_eq!(scan::pile_lines(&pile), vec!["f1", "new.txt"]);

        let a = StatusReport::build(&mut engine, None).to_json();
        let b = StatusReport::build(&mut engine, None).to_json();
        assert_eq!(a, b, "status is byte-stable across scans");
        assert!(a.contains("\"status_version\": 1"));
        assert!(!a.contains("\"at\""), "no timestamps in status: {a}");
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["roots"][0]["pending"][0]["path"], "f1");
        assert_eq!(v["roots"][0]["pending"][0]["change"], "modified");
        assert_eq!(v["roots"][0]["pending"][1]["change"], "added");
        assert_eq!(v["roots"][0]["branch"], "main");
        let human = StatusReport::build(&mut engine, None).render_human();
        assert!(human.starts_with("eng (main)  2 pending\n"), "{human}");
        assert!(human.contains("  M f1  +1 −"), "{human}");
        assert!(human.contains("  A new.txt  +1 −0"), "{human}");
    }

    #[test]
    fn engine_ops_accept_clears_the_row_and_persists() {
        let repo = FixtureRepo::new("eng-ops").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "changed\n");
        let pile = engine.scan(&root).unwrap();
        let rendered = Rendered::of(pile.row(b"f1").unwrap());
        let out = engine
            .ops(&root)
            .unwrap()
            .accept_file(&rendered, &NoFault)
            .unwrap();
        assert!(out.refused.is_empty());
        assert!(engine.scan(&root).unwrap().is_empty());
        drop(engine);
        // A fresh engine reads the same ledger.
        let mut engine = open_engine(&repo, &state, Config::default());
        assert!(engine.scan(&root).unwrap().is_empty());
    }

    #[test]
    fn engine_inspect_head_reports_a_commit_notice() {
        let mut repo = FixtureRepo::new("eng-head").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        assert!(
            engine.inspect_head(&root).unwrap().is_none(),
            "nothing moved"
        );
        let before = engine.root(&root).unwrap().head.head.clone();
        repo.commit_files(&[("f1", "committed\n")], "B1").unwrap();
        let change = engine.inspect_head(&root).unwrap().expect("HEAD moved");
        assert_eq!(change.from, before);
        assert_eq!(change.branch.as_deref(), Some("main"));
        let notice = change.notice.expect("a notice");
        assert!(notice.starts_with("committed on main"), "{notice}");
        assert!(
            change.pile.row(b"f1").is_some(),
            "the committed edit stays pending (B1)"
        );
    }

    #[test]
    fn engine_unreadable_ledger_and_missing_seen_tree_fail_open() {
        let repo = FixtureRepo::new("eng-open").unwrap();
        let state = TempDir::new("lc-eng-state");
        let engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let ledger_path = engine.root(&root).unwrap().paths.ledger.clone();
        drop(engine);
        std::fs::write(&ledger_path, b"{ not json").unwrap();
        let mut engine = open_engine(&repo, &state, Config::default());
        let r = engine.root(&root).unwrap();
        assert!(r.ledger.seen_tree.is_none(), "reopened with nothing seen");
        assert!(
            r.notices.iter().any(|n| n.contains("ledger unreadable")),
            "{:?}",
            r.notices
        );
        let pile = engine.scan(&root).unwrap();
        assert!(
            pile.rows.len() >= 3,
            "every tracked file is pending: {:?}",
            scan::pile_lines(&pile)
        );

        // The null-seen-tree ledger was persisted: a further reopen is not first sight at
        // the current HEAD (which would hide everything committed since).
        drop(engine);
        let mut engine = open_engine(&repo, &state, Config::default());
        let r = engine.root(&root).unwrap();
        assert!(
            r.ledger.seen_tree.is_none(),
            "still nothing seen after a second reopen"
        );
        assert!(r.ledger.seen_at.head_commit.is_none());
        assert!(
            !r.notices
                .iter()
                .any(|n| n.contains("unreadable") || n.contains("first sight")),
            "{:?}",
            r.notices
        );
        assert!(engine.scan(&root).unwrap().rows.len() >= 3);

        // A seen tree that vanished from the store (gc) is treated the same way.
        let mut ledger = engine.root(&root).unwrap().ledger.clone();
        ledger.seen_tree = Oid::parse(&"a".repeat(40));
        ledger::save(&engine.root(&root).unwrap().paths, &ledger).unwrap();
        drop(engine);
        let mut engine = open_engine(&repo, &state, Config::default());
        let r = engine.root(&root).unwrap();
        assert!(r.ledger.seen_tree.is_none());
        assert!(
            r.notices.iter().any(|n| n.contains("no longer exists")),
            "{:?}",
            r.notices
        );

        // The disk still names the pruned tree; accepting must merge with that ledger
        // without resolving it, and accept-all must write a fresh seen tree.
        let pile = engine.scan(&root).unwrap();
        let rendered = Rendered::of(pile.row(b"f1").unwrap());
        let out = engine
            .ops(&root)
            .unwrap()
            .accept_file(&rendered, &NoFault)
            .unwrap();
        assert!(out.ok(), "accept over a pruned seen tree: {out:?}");
        let on_disk = std::fs::read_to_string(&ledger_path).unwrap();
        assert!(
            !on_disk.contains(&"a".repeat(40)),
            "the pruned tree is not written back: {on_disk}"
        );
        let pile = engine.scan(&root).unwrap();
        assert!(pile.row(b"f1").is_none(), "{:?}", scan::pile_lines(&pile));
        let out = engine
            .ops(&root)
            .unwrap()
            .accept_all(&pile, &NoFault)
            .unwrap();
        assert!(out.ok(), "{out:?}");
        let seen = engine.root(&root).unwrap().ledger.seen_tree.clone();
        assert!(
            seen.as_ref().is_some_and(|t| t.as_str() != "a".repeat(40)),
            "{seen:?}"
        );
        assert_eq!(
            scan::pile_lines(&engine.scan(&root).unwrap()),
            Vec::<String>::new()
        );
    }

    /// The interleaving that must never first-sight: process A found the ledger
    /// unreadable and moved it aside (but has not written its null ledger yet) when
    /// process B opens the same root and finds no ledger at all.
    #[test]
    fn engine_open_treats_a_ledger_moved_aside_by_another_process_as_unreadable() {
        let repo = FixtureRepo::new("eng-race").unwrap();
        let state = TempDir::new("lc-eng-state");
        let engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let paths = engine.root(&root).unwrap().paths.clone();
        drop(engine);
        std::fs::write(&paths.ledger, b"{ not json").unwrap();
        let a = ledger::load(&paths, EngineOptions::default().clock.as_ref()).unwrap();
        assert!(matches!(a, LoadResult::Unreadable { .. }));
        assert!(!paths.ledger.exists(), "A moved the ledger aside");

        let mut engine = open_engine(&repo, &state, Config::default());
        let r = engine.root(&root).unwrap();
        assert!(
            r.ledger.seen_tree.is_none() && r.ledger.seen_at.head_commit.is_none(),
            "B opened with nothing seen, not first sight: {:?}",
            r.notices
        );
        assert!(
            r.notices
                .iter()
                .any(|n| n.contains("moved aside by another process")),
            "{:?}",
            r.notices
        );
        assert!(engine.scan(&root).unwrap().rows.len() >= 3);
        assert!(paths.ledger.exists(), "the null ledger was persisted");
    }

    /// The rung of the ladder where HEAD cannot be inspected at all: rows still come
    /// through, with a notice instead of an annotation.
    #[test]
    fn engine_scan_survives_a_head_that_cannot_be_inspected() {
        let repo = FixtureRepo::new("eng-nohead").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\nedit\n");
        assert_eq!(scan::pile_lines(&engine.scan(&root).unwrap()), ["f1"]);
        let head_file = repo.path().join(".git/HEAD");
        let keep = std::fs::read(&head_file).unwrap();
        std::fs::write(&head_file, b"garbage\n").unwrap();
        let pile = engine.scan(&root).unwrap();
        std::fs::write(&head_file, keep).unwrap();
        assert_eq!(scan::pile_lines(&pile), ["f1"], "{:?}", pile.notices);
        assert!(
            pile.notices
                .iter()
                .any(|n| n.contains("head inspection skipped")),
            "{:?}",
            pile.notices
        );
    }

    /// Remote reachability is a classification input: a fetch that moves `refs/remotes`
    /// without moving HEAD must relabel.
    #[test]
    fn engine_upstream_labels_follow_remote_refs_without_head_moving() {
        let mut repo = FixtureRepo::new("eng-remotes").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        // The coworker's commit reaches HEAD by URL (FETCH_HEAD), so no remote-tracking
        // ref knows it yet: neither mine nor remote-reachable, hence plain.
        repo.coworker_commit(&[("f1", "a1\ncoworker\n")], "cw")
            .unwrap();
        let origin = repo.origin().to_string_lossy().into_owned();
        repo.git(&["fetch", "-q", &origin, "main"]).unwrap();
        repo.git(&["merge", "-q", "--ff-only", "FETCH_HEAD"])
            .unwrap();
        assert_eq!(scan::pile_lines(&engine.scan(&root).unwrap()), ["f1"]);
        // `origin/main` catches up; HEAD is unchanged.
        repo.git(&["fetch", "-q", "origin"]).unwrap();
        assert_eq!(
            scan::pile_lines(&engine.scan(&root).unwrap()),
            ["f1 upstream"]
        );
    }

    #[test]
    fn engine_draft_root_follows_draft_initial() {
        let repo = FixtureRepo::new("eng-draft").unwrap();
        let state = TempDir::new("lc-eng-state");
        let drafts = repo.parent_dir().join("_drafts");
        std::fs::create_dir_all(&drafts).unwrap();
        std::fs::write(drafts.join("n1.md"), "one\n").unwrap();
        let config = Config {
            draft_dirs: vec!["_drafts".to_owned()],
            draft_initial: DraftInitial::Pending,
            ..Config::default()
        };
        let mut engine = open_engine(&repo, &state, config.clone());
        let draft = engine
            .roots()
            .iter()
            .find(|r| r.kind == RootKind::Draft)
            .map(|r| r.path.clone())
            .expect("a draft root");
        assert!(engine.root(&draft).unwrap().ledger.seen_tree.is_none());
        assert_eq!(
            scan::pile_lines(&engine.scan(&draft).unwrap()),
            vec!["n1.md"]
        );

        let state2 = TempDir::new("lc-eng-state");
        let mut engine = open_engine(
            &repo,
            &state2,
            Config {
                draft_initial: DraftInitial::Seen,
                ..config
            },
        );
        assert!(engine.root(&draft).unwrap().ledger.seen_tree.is_some());
        assert!(engine.scan(&draft).unwrap().is_empty());
    }

    #[test]
    fn engine_build_globs_matches_bare_names_at_any_depth() {
        let g = build_globs(&["Cargo.lock".to_owned(), "vendor/**".to_owned()]);
        assert!(g.is_match("Cargo.lock"));
        assert!(g.is_match("sub/dir/Cargo.lock"));
        assert!(g.is_match("vendor/x/y"));
        assert!(!g.is_match("src/main.rs"));
    }
}
