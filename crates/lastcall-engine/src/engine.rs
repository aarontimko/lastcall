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
use std::time::SystemTime;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::config::{Config, DraftInitial, Loaded, Resolved};
use crate::env::Env;
use crate::git::{self, ConfigList, GitError, Mode, Oid, RepoGit};
use crate::headstate::{self, HeadState, TransitionFacts};
use crate::hunks::{self, Hunk};
use crate::index::{IndexError, PrivateIndex};
use crate::ledger::{
    self, Clock, FlagHunk, FlagSummary, Ledger, LedgerError, LedgerLock, LoadResult, SeenAt,
    SystemClock, TreeEntries, UndoOp,
};
use crate::ops::{self, FaultInjector, NoFault, Ops, OpsError, Outcome, Refused, Rendered};
use crate::paths::{Layout, ParentId, ParentMeta, RepoPaths, RootId};
use crate::roots::{self, Badge, DiscoverInputs, Discovery, RootsChanged};
use crate::scan::{self, Entry, Pile, Row, ScanError, ScanInputs};
use crate::store::{Current, DraftScope, ExcludedDir, RepoFacts, RootKind, Store, StoreError};
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
    /// An on-demand expansion ([`Engine::hunks_of`]) wanted a blob the store cannot
    /// produce. Reading it as an empty side would render the live file as deleted, which
    /// hides the content the reviewer asked to see (invariant 2 is over-show, never hide),
    /// so the expansion refuses instead and the caller says "refresh".
    #[error("blob {oid} is missing from the store for {}", root.display())]
    MissingBlob { root: PathBuf, oid: String },
    /// An on-demand expansion reached a [`crate::scan::Collapsed::Binary`] row. There is no
    /// text diff to show and rendering NUL bytes into a terminal is worse than nothing, so
    /// the expansion refuses. The TUI never sends the request (`e` is a no-op on a binary
    /// row); this is the engine-side guard behind that.
    #[error("{path} is binary; there is no text expansion")]
    BinaryRow { root: PathBuf, path: String },
}

fn io_err(path: &Path, source: std::io::Error) -> EngineError {
    EngineError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// The default [`EngineOptions::row_cap`]: rows materialised per scan beyond the override
/// paths (kickoff ruling; not a config key).
pub const DEFAULT_ROW_CAP: usize = 10_000;

/// The ceiling on [`EngineOptions::parallelism`]. Every root's work is a git subprocess,
/// so the useful width is the machine's, and past a handful of concurrent `git` children
/// the disk, not the CPU, is the limit. There is deliberately no config key for it
/// (Phase 5 ruling): the binary's `LASTCALL_PARALLELISM` override exists for the golden
/// test, not for users.
pub const MAX_PARALLELISM: usize = 8;

/// `min(available_parallelism(), MAX_PARALLELISM)`, floor 1.
pub fn default_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, MAX_PARALLELISM)
}

/// Injectable knobs.
#[derive(Clone)]
pub struct EngineOptions {
    /// Compaction runs after a ledger write when more overrides than this carry a blob.
    pub compaction_threshold: usize,
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// Rows materialised per scan beyond the priority set (override paths); further
    /// changed paths are counted in [`Pile::omitted`] with a notice, never hashed.
    pub row_cap: usize,
    /// How many roots are opened, and scanned, at once. 1 runs everything inline on the
    /// calling thread — no thread is spawned at all — and the resulting state must be
    /// identical at every value (`engine_parallel_open_and_scan_match_the_sequential_run_exactly`).
    pub parallelism: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            compaction_threshold: 500,
            clock: Arc::new(SystemClock),
            row_cap: DEFAULT_ROW_CAP,
            parallelism: default_parallelism(),
        }
    }
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineOptions")
            .field("compaction_threshold", &self.compaction_threshold)
            .field("row_cap", &self.row_cap)
            .field("parallelism", &self.parallelism)
            .finish_non_exhaustive()
    }
}

/// Run `job` over `items` on at most `width` threads and return the results **in the order
/// of `items`** — never in completion order, so nothing downstream can depend on which
/// worker finished first.
///
/// `width <= 1` (or a single item) runs inline on the calling thread with no thread spawned
/// at all: the sequential path is then literally the same code, which is what makes
/// "identical state at parallelism 1 and 8" a claim about the *work*, not about two
/// implementations that have to be kept in step.
///
/// The only shared state is the work queue and the result slots; each item is handed to
/// exactly one worker, which owns it for the whole call, so `job` needs no lock of its own
/// and none is held across it.
fn parallel_map<I, T, F>(items: Vec<I>, width: usize, job: F) -> Vec<T>
where
    I: Send,
    T: Send,
    F: Fn(I) -> T + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let width = width.clamp(1, MAX_PARALLELISM).min(n);
    if width == 1 {
        return items.into_iter().map(job).collect();
    }
    // Reversed so `pop` hands out index 0 first: a run that is *effectively* sequential
    // (one slow root, the rest trivial) still starts in path order.
    let queue: Vec<(usize, I)> = items.into_iter().enumerate().rev().collect();
    let queue = std::sync::Mutex::new(queue);
    let slots: std::sync::Mutex<Vec<Option<T>>> =
        std::sync::Mutex::new((0..n).map(|_| None).collect());
    let job = &job;
    let queue = &queue;
    let slots = &slots;
    std::thread::scope(|scope| {
        for _ in 0..width {
            scope.spawn(move || {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
                    let Some((i, item)) = next else { break };
                    let value = job(item);
                    slots.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(value);
                }
            });
        }
    });
    let slots = std::mem::take(&mut *slots.lock().unwrap_or_else(|e| e.into_inner()));
    slots
        .into_iter()
        .map(|s| s.expect("every index is filled before the scope ends"))
        .collect()
}

/// Everything the engine holds for one root.
pub struct RootState {
    pub path: PathBuf,
    pub kind: RootKind,
    pub parent: PathBuf,
    pub badge: Option<Badge>,
    /// What every surface shows for this root, as discovery decided it: a repository's own
    /// folder name, and a watched folder's name with the folders above it that
    /// `draft_dir_parents` asks for.
    pub name: String,
    /// `Some` for a watched folder: how much of it this root covers and the size at which
    /// it stops reading a file. `None` for a repository.
    pub scope: Option<DraftScope>,
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
    /// Root-relative folders inside this root that another root looks after (their files
    /// are theirs), with whether the whole tree below each belongs there.
    pub excluded_dirs: Vec<ExcludedDir>,
    /// Notices from open (kept until the root is reopened).
    pub notices: Vec<String>,
    pub last_pile: Option<Pile>,
    pub nested_repos: Vec<Vec<u8>>,
    /// Set when a scan changed `nested_repos`; `scan_all` re-runs discovery only then.
    pub nested_changed: bool,
    pub user_email: Option<String>,
    /// `org/repo` from `remote.origin.url` ([`remote_slug`]); `None` for no remote, a
    /// local-path or `file://` origin, or anything unparsable. Presentation only (§6.7).
    pub remote: Option<String>,
    /// Identity of `ledger.json` when `ledger` was read; a scan re-reads on change.
    pub ledger_stamp: Option<ledger::Stamp>,
    /// Set by [`RootState::sync_branch`] when the switch it performed was the arriving
    /// branch's **first sight**, and taken by the next head inspection for R8's notice.
    ///
    /// It has to wait here rather than be computed in `inspect_head`: a scan can perform
    /// the switch (a file event arriving before the git-dir event) long before any head
    /// inspection runs, and by then the record has been made and the fact is gone.
    pub first_sight_from: Option<String>,
    /// Why the first-sight fold did not run, when the switch [`RootState::sync_branch`]
    /// performed skipped or abandoned it (verifier F6). Taken by the next head inspection,
    /// exactly as [`RootState::first_sight_from`] is, and also said once in
    /// [`RootState::notices`] so a one-shot `status` — which never inspects a head — reports
    /// it too.
    pub fold_skipped: Option<String>,
}

impl RootState {
    /// Adopt a ledger another process wrote since this one was read (an accept in a second
    /// `lastcall`, a fold) so a long-running engine never shows a stale baseline. The read
    /// is unlocked: writes are rename-atomic, so it sees either the old or the new file.
    /// Content that does not parse is left to `open`'s rules; the loaded ledger stays.
    fn reload_ledger_if_changed(&mut self) {
        let now = ledger::stamp(&self.paths);
        if now == self.ledger_stamp {
            return;
        }
        self.ledger_stamp = now;
        let Ok(bytes) = std::fs::read(&self.paths.ledger) else {
            return;
        };
        let Ok((mut fresh, _)) = ledger::parse(&bytes) else {
            return;
        };
        if fresh.seen_tree != self.ledger.seen_tree {
            self.tree = match &fresh.seen_tree {
                Some(t) if self.store.exists(t) => match self.store.ls_tree(t) {
                    Ok(entries) => entries,
                    Err(_) => {
                        fresh.seen_tree = None;
                        TreeEntries::new()
                    }
                },
                Some(_) => {
                    fresh.seen_tree = None;
                    TreeEntries::new()
                }
                None => TreeEntries::new(),
            };
        }
        self.ledger = fresh;
    }

    /// The per-worktree `HEAD` file this root's record in force follows (R1). `None` for
    /// a draft root and before the first head inspection.
    fn head_file_dir(&self) -> Option<&Path> {
        if self.repo.is_none() || self.head.git_dir.as_os_str().is_empty() {
            return None;
        }
        Some(&self.head.git_dir)
    }

    /// R1: bring the record in force into step with the branch `<git_dir>/HEAD` names.
    ///
    /// Every reader and writer of a baseline runs this first: `scan_root` right after
    /// `reload_ledger_if_changed` (so `scan` and `scan_all` both reach it), `inspect_head`
    /// before it compares heads, and the tail of `open_root_with` (where an offline switch
    /// lands, D21). It is a `RootState` step and not an `Engine` method because the scan
    /// pool owns one `&mut RootState` per worker and an `Engine` method cannot be called
    /// there.
    ///
    /// Four outcomes, and only the last writes anything:
    /// - **no switch** — a draft root, a detached or unborn `HEAD`, a missing or locked
    ///   `HEAD`, or the name already in force (R4);
    /// - **adopt** — the record belongs to no branch yet (a file a 1.1 binary wrote, R6, or
    ///   a record made at a detached HEAD): it is attributed to `HEAD`'s branch with no
    ///   park, no first sight and no fold, so nothing in the pile moves;
    /// - **re-label** — `HEAD` names another branch and the record's own ref is gone
    ///   (`git branch -m` of the current branch, R3): the same record, a new name;
    /// - **switch** — [`Ops::switch_branch`], the one that parks, loads or first-sights.
    ///
    /// The first three write no ledger: they are attributions, not changes, and the next
    /// ordinary write stamps them. That also makes them idempotent, which matters because
    /// `reload_ledger_if_changed` can put the file's version back at any time.
    fn sync_branch(&mut self, clock: &dyn Clock) {
        let Some(dir) = self.head_file_dir() else {
            return;
        };
        let Some(name) = headstate::head_branch(dir) else {
            return; // R4: detached, unborn-and-unnamed, missing or mid-write
        };
        match self.ledger.seen_branch.clone() {
            Some(current) if current == name => {
                self.ledger.adopt_branch = false;
            }
            None => {
                self.ledger.seen_branch = Some(name);
                self.ledger.adopt_branch = false;
            }
            Some(_) => {
                let mut ops = self.switch_ops(clock);
                match ops.switch_branch(&name, &NoFault) {
                    Ok(switched) => {
                        // A switch that happened says what the notice must say, and that
                        // includes saying nothing: a **load** clears the value, so a first
                        // sight one sync performed cannot travel to the next head move and
                        // relabel it as one. Only a switch that wrote nothing (the record
                        // already on this branch, or R5's adopt) leaves it alone, and those
                        // leave the record alone too.
                        if switched.happened {
                            self.first_sight_from = switched.first_sight_from;
                            // F6: a fold that did not run says so once, here and at the
                            // next head inspection. The copy stands whole either way, so
                            // this explains an over-show rather than reporting a failure.
                            if let Some(why) = &switched.fold_skipped {
                                self.notices.push(format!(
                                    "switched to {name}: first time here, seen state carried \
                                     without folding ({why})"
                                ));
                            }
                            self.fold_skipped = switched.fold_skipped;
                        }
                        // The ledger was re-read under the lock on every `Ok` path, so what
                        // is in memory is the file, whether this switch wrote one or adopted
                        // another process's (F10b): the next `reload_ledger_if_changed` has
                        // nothing to re-read.
                        self.ledger_stamp = ledger::stamp(&self.paths);
                    }
                    Err(e) => {
                        // Fail open: the record in force stays, which over-shows the new
                        // branch's delta and hides nothing.
                        self.notices
                            .push(format!("branch switch to {name} skipped: {e}"));
                    }
                }
            }
        }
    }

    /// An [`Ops`] over this root for the branch switch alone. It stages nothing and never
    /// compacts (`switch_branch` does not reach the threshold check), so the threshold is
    /// the one field with no meaningful value here.
    fn switch_ops<'a>(&'a mut self, clock: &'a dyn Clock) -> Ops<'a> {
        let git_dir =
            (!self.head.git_dir.as_os_str().is_empty()).then(|| self.head.git_dir.clone());
        Ops {
            store: &self.store,
            index: &self.index,
            repo: self.repo.as_ref(),
            paths: &self.paths,
            branch: self.ledger.seen_branch.clone(),
            git_dir,
            ledger: &mut self.ledger,
            tree: &mut self.tree,
            clock,
            compaction_threshold: usize::MAX,
            case_insensitive: self.case_insensitive,
            staged: BTreeMap::new(),
            pending_undo: None,
            lock: crate::ops::DEFAULT_LOCK,
        }
    }

    /// The ops one scope trim runs through (Amendment v1.13 R6). Like `switch_ops` it is
    /// not an accept: nothing is staged and the compaction threshold cannot be reached, so
    /// the field has no meaningful value here.
    fn trim_ops<'a>(&'a mut self, clock: &'a dyn Clock) -> Ops<'a> {
        Ops {
            store: &self.store,
            index: &self.index,
            repo: self.repo.as_ref(),
            paths: &self.paths,
            branch: self.ledger.seen_branch.clone(),
            git_dir: None,
            ledger: &mut self.ledger,
            tree: &mut self.tree,
            clock,
            compaction_threshold: usize::MAX,
            case_insensitive: self.case_insensitive,
            staged: BTreeMap::new(),
            pending_undo: None,
            lock: crate::ops::DEFAULT_LOCK,
        }
    }

    pub fn seen_head(&self) -> Option<&Oid> {
        self.ledger.seen_at.head_commit.as_ref()
    }

    pub fn name(&self) -> String {
        self.name.clone()
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
    /// The engine-global number of the scan that produced `pile` ([`Engine::scan_seq`]).
    pub seq: u64,
    /// The pile from the scan that followed.
    pub pile: Pile,
}

/// One accept as a UI asks for it (§6.3): every variant pins what the user looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptRequest {
    /// Hunk `index` of `hunks` (the row's hunks as rendered) on `rendered`.
    Hunk {
        rendered: Rendered,
        hunks: Vec<Hunk>,
        index: usize,
    },
    /// One file; a deletion row (`rendered.oid == None`) accepts the deletion.
    File(Rendered),
    /// Several files in one ledger write. `rendered_on` is the branch the pile these rows
    /// were taken from was scanned under, so the write can be refused when the record in
    /// force has moved on since (R5, and [`Pile::seen_branch`]).
    Group {
        rows: Vec<Rendered>,
        rendered_on: Option<String>,
    },
    /// Everything in the pile the user saw (a fold).
    All(Pile),
}

/// One restore as a UI asks for it (§6.3). The same rendered tokens an accept pins, and
/// deliberately fewer variants: there is no restore-group and no restore-all — undoing
/// everything at once is `git checkout`, and lastcall does not own that gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreRequest {
    /// Hunk `index` of `hunks` goes back to the baseline; the other hunks stay.
    Hunk {
        rendered: Rendered,
        hunks: Vec<Hunk>,
        index: usize,
    },
    /// One file; a deletion row (`rendered.oid == None`) puts the file back.
    File(Rendered),
}

/// What [`Engine::restore`] produced. The same shape as [`Accepted`] — a refusal is data,
/// never `Err` — with one difference worth stating: `outcome.written` is always `false`,
/// because a restore never writes the ledger. What it changed is the working tree, and
/// `pile` is the rescan that shows the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub outcome: Outcome,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
    pub pile: Pile,
}

/// One editor save as a UI asks for it (§6.3 "editor save"; Phase 8 deliverable 1).
///
/// `rendered` is what the buffer was read from — the CAS target — and `bytes` is the
/// buffer verbatim, line endings and a missing trailing newline included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest {
    pub rendered: Rendered,
    pub bytes: Vec<u8>,
}

/// What [`Engine::save`] produced. Like [`Restored`] plus the ledger: a save writes the
/// working tree *and* the override, so `outcome.written` is `true` on success, and `pile`
/// is the rescan that should show the saved row gone (invariant 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub outcome: Outcome,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
    pub pile: Pile,
}

/// A hunk flag as the **caller rendered it**: the record the ledger takes, plus the total
/// the export's `hunk n of m` names.
///
/// `of` is caller-side on purpose (verifier F5). Deriving it from the pile the flag's own
/// rescan produced meant the header and text came from the screen while the total came from
/// the file as it is *now*: an agent that rewrote the file between the render and the
/// keystroke produced `hunk 2 of 1`, a shape that never existed. The caller has the number
/// that was true when the user looked, and that is the only one the export may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedHunk {
    pub hunk: FlagHunk,
    /// **Content** hunks in the row as it was on screen. The synthetic mode hunk is not one
    /// of them: a `chmod` on a two-hunk file is `of 2`, not `of 3`.
    pub of: usize,
}

/// What [`Engine::flag`] and [`Engine::unflag`] produced.
///
/// `export` is the paste-ready message for the flag that was just written
/// ([`crate::flags::export`]) — computed here rather than in the UI because only the engine
/// has the flag's `created_at`. The `hunk n of m` count comes from the caller's
/// [`RenderedHunk`]. An `unflag`, or a refusal, leaves it empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flagged {
    pub outcome: Outcome,
    pub export: String,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
    pub pile: Pile,
}

/// What [`Engine::undo`] produced (Amendment v1.11): the outcome, what the popped entry
/// was, the paths it put back in path order, and the pile of the rescan that followed.
///
/// `op` and `paths` are empty when nothing was undone (`outcome` then carries
/// [`Refused::NothingToUndo`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    pub outcome: Outcome,
    pub op: Option<UndoOp>,
    pub paths: Vec<String>,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
    pub pile: Pile,
}

/// What [`Engine::snooze`] produced: the deadline it wrote (`None` for a wake) and the
/// pile of the rescan that followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snoozed {
    pub outcome: Outcome,
    pub until: Option<String>,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
    pub pile: Pile,
}

/// What [`Engine::accept`] produced: the op's outcome (a refusal is data, never `Err`) and
/// the pile of the rescan that followed, numbered like every other pile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub outcome: Outcome,
    /// [`Engine::scan_seq`] of `pile`.
    pub seq: u64,
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
    /// How many times discovery re-ran after open (a budget probe for tests).
    discovery_runs: u64,
    git_version: String,
    /// The home directory the paths shown to the user are collapsed to `~` against,
    /// canonicalized once here. A home reached through a symlink (`/home/u` pointing at
    /// `/data/u`) would otherwise never match a root path, which is always canonical, and
    /// every path shown would be the long one. The raw value is the fallback when it will
    /// not canonicalize, which is the answer that was right before this existed.
    home_shown: Option<PathBuf>,
    /// Engine-global scan sequence number: `+1` per pile [`Engine::scan`] produces, whichever
    /// root; every publisher of a pile carries it so a consumer can drop a pile older than
    /// one it already holds (a `scan_all` result arriving after an accept's rescan).
    scan_seq: u64,
}

/// Take this process's temp index in every root it opened (Phase 5 deliverable 2a).
///
/// Each scan already unlinks it the moment it is done with it, so this only matters when a
/// scan died between the `read-tree` and the `write-tree` - but a long-lived TUI over many
/// roots would otherwise leave one such file per root behind until the hour-old sweep at the
/// next open, and the sweep is the fallback for a *killed* process, not for a clean quit.
impl Drop for Engine {
    fn drop(&mut self) {
        for state in self.roots.values() {
            let _ = std::fs::remove_file(&state.paths.index_tmp);
        }
    }
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
            discovery_runs: 0,
            git_version,
            home_shown: env
                .home()
                .map(|h| std::fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf())),
            scan_seq: 0,
        };
        let started = std::time::Instant::now();
        engine.rescan()?;
        // Deliverable 8: the one number a "lastcall took forever to start" report needs.
        tracing::debug!(
            roots = engine.roots.len(),
            ms = started.elapsed().as_millis() as u64,
            "open done"
        );
        Ok(engine)
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The canonicalized home directory, for collapsing a path shown to the user to `~`.
    pub fn home_shown(&self) -> Option<&Path> {
        self.home_shown.as_deref()
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
    /// Discovery re-runs since open (`rescan`, including those `scan_all` triggers).
    pub fn discovery_runs(&self) -> u64 {
        self.discovery_runs
    }

    /// Change how many folders below each parent dir the next discovery pass reads
    /// (Amendment v1.11, deliverable 8). The value the first-launch tour's depth card
    /// applies for the session, whether or not it could write it to the config file.
    ///
    /// It only sets the number: the roots appear on the next [`Engine::rescan`], which the
    /// watcher runs when it is asked to and on its own backstop. Out-of-range values are
    /// clamped rather than refused — `Config::validate` is where a bad *file* is rejected,
    /// and a caller in the binary asking for depth 9 should get the deepest walk there is,
    /// not a panic in the middle of a keystroke.
    pub fn set_search_depth(&mut self, depth: u8) {
        self.config.search_depth = depth.clamp(1, crate::config::MAX_SEARCH_DEPTH);
    }

    /// How many folders below each parent dir discovery currently reads.
    pub fn search_depth(&self) -> u8 {
        self.config.search_depth
    }

    /// The number of the most recent scan that produced a pile (engine-global, strictly
    /// increasing across roots, `0` before the first). Read it under the same lock as the
    /// [`Engine::scan`] it describes.
    pub fn scan_seq(&self) -> u64 {
        self.scan_seq
    }

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
        self.discovery_runs += 1;
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
            search_depth: self.config.search_depth,
            collapse_size_bytes: self.config.collapse_size_bytes,
            draft_dir_parents: self.config.draft_dir_parents,
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
        // Each parent's `meta.json` is written **before** the pool starts: it is the one
        // piece of shared state an open touches, so writing it serially is what keeps the
        // pool free of any lock of its own (deliverable 1a).
        // A root whose parent `meta.json` cannot be written is not opened at all — one
        // "cannot open" notice, and the root stays out of `roots` (as `open_root` did
        // before the pool existed), rather than a notice for a root that is then open.
        let mut to_open = to_open;
        to_open.retain(|d| match self.ensure_parent_meta(&d.parent) {
            Ok(()) => true,
            Err(e) => {
                self.notices
                    .push(format!("{}: cannot open: {e}", d.path.display()));
                false
            }
        });
        // Opened on up to `parallelism` threads; applied here, serially, in path order, so
        // the resulting state does not depend on which root finished first.
        let ctx = self.open_ctx();
        let opened = parallel_map(to_open.iter().collect(), self.options.parallelism, |d| {
            open_root_with(&ctx, d)
        });
        for (d, state) in to_open.iter().zip(opened) {
            match state {
                Ok(state) => {
                    self.roots.insert(d.path.clone(), state);
                }
                Err(e) => self
                    .notices
                    .push(format!("{}: cannot open: {e}", d.path.display())),
            }
        }
        // Badges, names, scopes and who looks after what can all change with the root set:
        // an open root that has not had a first sight yet still picks up the current
        // answer, which is what makes a config change visible after a restart without a
        // root having to be re-opened.
        let discovery = self.discovery.clone();
        for root in self.roots.values_mut() {
            if let Some(d) = discovery.get(&root.path) {
                root.badge = d.badge.clone();
                root.name = d.name.clone();
                root.scope = d.scope.clone();
                root.excluded_dirs = d.excluded_dirs.clone();
            }
        }
        Ok(changed)
    }

    /// Write `<parent>/meta.json` if it is not there yet. Serial, before the open pool.
    fn ensure_parent_meta(&self, parent: &Path) -> Result<(), EngineError> {
        let meta = self.layout.meta_path(&ParentId::of(parent));
        if meta.exists() {
            return Ok(());
        }
        // The parent's directory used to be created as a side effect of the first root's
        // `repo_dir`; the meta write now runs before any of that (deliverable 1a).
        if let Some(dir) = meta.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }
        let m = ParentMeta {
            schema_version: ledger::SCHEMA_VERSION.to_string(),
            parent: parent.to_string_lossy().into_owned(),
            created_at: self.options.clock.now_iso8601(),
        };
        let text = serde_json::to_string_pretty(&m).unwrap_or_default();
        std::fs::write(&meta, text).map_err(|e| io_err(&meta, e))
    }

    fn open_ctx(&self) -> OpenCtx<'_> {
        OpenCtx {
            env: &self.env,
            layout: &self.layout,
            clock: self.options.clock.as_ref(),
            draft_initial: self.config.draft_initial,
        }
    }

    /// Open one root on the calling thread — what the pool does per item, without the
    /// pool. The budget test measures this.
    #[cfg(test)]
    fn open_root(&self, d: &roots::DiscoveredRoot) -> Result<RootState, EngineError> {
        self.ensure_parent_meta(&d.parent)?;
        open_root_with(&self.open_ctx(), d)
    }
}

/// Everything opening one root reads from the engine. `&OpenCtx` is what crosses into the
/// pool — never `&Engine`, whose `roots` map the apply step mutates.
struct OpenCtx<'a> {
    env: &'a Env,
    layout: &'a Layout,
    clock: &'a (dyn Clock + Send + Sync),
    draft_initial: DraftInitial,
}

fn open_root_with(ctx: &OpenCtx<'_>, d: &roots::DiscoveredRoot) -> Result<RootState, EngineError> {
    let parent_id = ParentId::of(&d.parent);
    let root_id = RootId::of(&d.path);
    let paths = ctx.layout.repo_paths(&parent_id, &root_id);
    std::fs::create_dir_all(&paths.repo_dir).map_err(|e| io_err(&paths.repo_dir, e))?;
    let repo = (d.kind == RootKind::Git).then(|| RepoGit::new(ctx.env, &d.path));
    // Two spawns of the user's repo answer everything this open needs from it
    // (deliverable 1c): one `config --list -z`, and the head inspection's batched
    // `rev-parse` with the three git paths folded in. A config that cannot be read is
    // a notice and an empty reading — the same fail-open the per-key reads had.
    let mut notices = Vec::new();
    let repo_config = match &repo {
        Some(rg) => match rg.config_list() {
            Ok(c) => c,
            Err(e) => {
                notices.push(format!("cannot read the repository config: {e}"));
                ConfigList::default()
            }
        },
        None => ConfigList::default(),
    };
    const GIT_PATHS: [&str; 3] = ["objects", "info/attributes", "info/exclude"];
    let (head, git_paths) = match &repo {
        Some(rg) => headstate::inspect_with_paths(rg, &GIT_PATHS)?,
        None => (HeadState::none(), Vec::new()),
    };
    let facts = repo.as_ref().map(|_| RepoFacts {
        config: &repo_config,
        objects_dir: git_paths[0].clone(),
        info_attributes: git_paths[1].clone(),
    });
    let (store, store_notices) = Store::open(ctx.env, &d.path, d.kind, &paths, facts.as_ref())?;
    notices.extend(store_notices);
    let exclude_from = git_paths.get(2).cloned();
    let index = PrivateIndex::new(store.git().clone(), &paths, d.kind, exclude_from);
    let user_email = repo_config
        .get("user.email")
        .filter(|e| !e.is_empty())
        .map(str::to_owned);
    // `config --list` resolves exactly what `config --get` would; `remote get-url
    // origin` (§6.7) is not allowlisted, and the two differ only under
    // `url.<base>.insteadOf` rewriting.
    let remote = repo_config
        .get("remote.origin.url")
        .filter(|u| !u.is_empty())
        .and_then(remote_slug);

    // A `ledger.json.tmp` left by a crash between write and rename (E1) is garbage:
    // the rename never happened, so `ledger.json` is still the previous version. A
    // live writer holds the lock between its write and rename, so the tmp is judged
    // under the lock: once we hold it, any tmp still there is stale.
    let stale_tmp = paths.ledger.with_extension("json.tmp");
    if stale_tmp.exists() {
        let _lock = LedgerLock::acquire(&paths)?;
        if stale_tmp.exists() {
            let _ = std::fs::remove_file(&stale_tmp);
            notices.push("removed a stale ledger.json.tmp from an interrupted write".to_owned());
        }
    }
    let clock: &dyn Clock = ctx.clock;
    let expected_root = d.path.to_string_lossy();
    let first = match ledger::load(&paths, clock)? {
        LoadResult::Loaded { ledger, .. } if ledger.root != expected_root => {
            // A ledger recorded for another path is another root's state, however it
            // got here (kickoff deliverable 2): exactly an unreadable ledger.
            let moved_to = ledger::move_aside_ledger(&paths, clock)?;
            LoadResult::Unreadable {
                moved_to,
                reason: format!("recorded root {} is not {expected_root}", ledger.root),
            }
        }
        other => other,
    };
    let mut ledger = match first {
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
                            let mut l = Ledger::new(
                                &d.path,
                                d.kind,
                                None,
                                SeenAt {
                                    head_commit: None,
                                    branch: None,
                                    at: clock.now_iso8601(),
                                },
                            );
                            l.seen_branch = head.branch.clone();
                            ledger::save(&paths, &l)?;
                            l
                        }
                        None => {
                            // E4: a sibling ledger whose root no longer exists is
                            // probably this root under its old name. First-sight rules
                            // apply; say where the old state is.
                            for (old_root, dir) in
                                orphaned_ledgers(&ctx.layout.repos_dir(&parent_id))
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
                                ctx.draft_initial,
                                d.scope.as_ref().map(|s| (s, d.excluded_dirs.as_slice())),
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
    let ledger_stamp = ledger::stamp(&paths);
    let mut state = RootState {
        path: d.path.clone(),
        kind: d.kind,
        parent: d.parent.clone(),
        badge: d.badge.clone(),
        name: d.name.clone(),
        scope: d.scope.clone(),
        case_insensitive: scan::probe_case_insensitive(&d.path),
        paths,
        store,
        index,
        repo,
        ledger,
        tree,
        head,
        classifier: Classifier::default(),
        excluded_dirs: d.excluded_dirs.clone(),
        notices,
        last_pile: None,
        nested_repos: Vec::new(),
        nested_changed: false,
        user_email,
        remote,
        ledger_stamp,
        first_sight_from: None,
        fold_skipped: None,
    };
    // R1, and D21's landing place: a switch made while lastcall was not running is seen
    // here, before the root's first scan, so the very first pile is the arriving branch's.
    // A root that first-sighted just now already named its branch, so this is a no-op.
    state.sync_branch(clock);
    // An open describes nothing: there is no previous HEAD to have moved, so the first
    // inspection answers `None`. A first sight performed here must therefore not leave the
    // R8 wording waiting for whatever the user does next (verifier F3).
    state.first_sight_from = None;
    Ok(state)
}

/// What one root's scan needs from the engine that is not in its own [`RootState`]. Shared
/// by reference across the scan pool; nothing in it is mutated there.
struct ScanCtx {
    collapsed: GlobSet,
    collapse_size_bytes: u64,
    row_cap: usize,
    /// The engine's injected wall clock, read once per `scan_all` so every root in one
    /// sweep decides snooze expiry against the same instant (Amendment v1.11).
    now: SystemTime,
    /// The clock itself, for the branch sync's ledger write (Amendment v1.12): a switch
    /// stamps `seen_at` and `parked_at`.
    clock: Arc<dyn Clock + Send + Sync>,
}

/// Scan one root, start to finish, mutating **only** that root's state: its nested-repo
/// list, its classifier cache and its `last_pile`. This is the pool's unit of work
/// (deliverable 1b).
///
/// `scan_seq` is deliberately *not* touched here. It is the engine's ordering of scans, and
/// assigning it inside the pool would make it depend on which worker finished first; the
/// serial apply step in [`Engine::scan_all`] hands it out in path order instead.
/// Deliverable 8's engine probe: one `scan done` line per scanned root, with the four field
/// names a diagnosis reads — which root, how long, how many rows came back, and the
/// engine-global `seq` that orders the piles. Emitted at `debug`, so it costs nothing until
/// `LASTCALL_LOG=debug` asks for it, and emitted from the **serial** step of `scan_all` as
/// well as from `scan`, so the `seq` in the line is the one the pile carries.
fn trace_scan_done(root: &Path, started: &std::time::Instant, rows: usize, seq: u64) {
    tracing::debug!(
        root = %root.display(),
        ms = started.elapsed().as_millis() as u64,
        rows,
        seq,
        "scan done"
    );
}

fn scan_root(state: &mut RootState, ctx: &ScanCtx) -> Result<Pile, EngineError> {
    state.reload_ledger_if_changed();
    // R1: which record is in force, before a single baseline is read. A scan that arrives
    // on file events alone, with no head inspection behind it, still sees the switch.
    state.sync_branch(ctx.clock.as_ref());
    // R6: a record written while the folder's scope was wider still holds paths the scope
    // no longer covers. They are never candidates, and the private index would otherwise
    // carry and `lstat` every one of them at every scan, so the record is trimmed to the
    // scope first. Path shape only: a large file is R2's unread row, never a silent drop.
    // The test is an in-memory pass over the record's own keys, so a scan of a folder
    // already inside its scope pays no syscall for it.
    let mut trimmed = 0usize;
    if let Some(scope) = state.scope.clone() {
        let excluded = state.excluded_dirs.clone();
        let out_of_scope =
            |p: &[u8]| !scope.admits_shape(p) || crate::store::under_excluded(p, &excluded);
        // The record in force is the seen tree composed with the overrides, so a path the
        // tree never held but an override sets still has to be trimmed (verifier F2).
        let held_outside = state.tree.keys().any(|p| out_of_scope(p))
            || state
                .ledger
                .overrides
                .iter()
                .any(|(k, o)| matches!(o.blob, Some(Some(_))) && out_of_scope(k.as_bytes()));
        if held_outside {
            trimmed = state
                .trim_ops(ctx.clock.as_ref())
                .trim(&out_of_scope, &NoFault)?;
        }
    }
    let out = scan::scan(&ScanInputs {
        store: &state.store,
        index: &state.index,
        repo: state.repo.as_ref(),
        ledger: &state.ledger,
        seen_tree: state.ledger.seen_tree.as_ref(),
        tree: &state.tree,
        case_insensitive: state.case_insensitive,
        collapsed_globs: &ctx.collapsed,
        collapse_size_bytes: ctx.collapse_size_bytes,
        scope: state.scope.as_ref(),
        excluded_dirs: &state.excluded_dirs,
        index_tmp: &state.paths.index_tmp,
        row_cap: ctx.row_cap,
    })?;
    let mut pile = out.pile;
    // The two per-root ledger facts the reducer may never read for itself (design review
    // F8), stamped here rather than inside `scan` because the expiry needs a wall clock and
    // the engine owns the injected one.
    if trimmed > 0 {
        pile.notices.push(format!(
            "{} path{} outside the root's scope dropped from its record",
            crate::count::with_thousands(trimmed),
            if trimmed == 1 { "" } else { "s" }
        ));
    }
    pile.undo = state.ledger.undo.len();
    pile.snoozed_until = ledger::snooze_active(state.ledger.snoozed_until.as_deref(), ctx.now);
    if out.nested_repos != state.nested_repos {
        state.nested_repos = out.nested_repos;
        state.nested_changed = true;
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

impl Engine {
    fn scan_ctx(&self) -> ScanCtx {
        ScanCtx {
            collapsed: self.collapsed.clone(),
            collapse_size_bytes: self.config.collapse_size_bytes,
            row_cap: self.options.row_cap,
            now: self.options.clock.now(),
            clock: self.options.clock.clone(),
        }
    }

    /// Scan one root: candidates → rows → annotation.
    pub fn scan(&mut self, root: &Path) -> Result<Pile, EngineError> {
        let ctx = self.scan_ctx();
        let started = std::time::Instant::now();
        let state = self
            .roots
            .get_mut(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        let pile = scan_root(state, &ctx)?;
        self.scan_seq += 1;
        trace_scan_done(root, &started, pile.rows.len(), self.scan_seq);
        Ok(pile)
    }

    /// Scan every root (opening nested repositories discovered on the way). Each entry
    /// carries the [`Engine::scan_seq`] of the scan that produced it (a failed scan reports
    /// the number of the last one that succeeded; its pile is the error).
    pub fn scan_all(&mut self) -> Vec<(PathBuf, u64, Result<Pile, EngineError>)> {
        self.scan_all_with(&|_, _| {})
    }

    /// [`Engine::scan_all`] with a progress hook: `on_scanned(root, rows)` runs on the pool
    /// thread the moment that root's scan returns (`rows` = pending rows, 0 for a failed
    /// scan), before the serial apply step. A consumer can count roots as they finish —
    /// the TUI's `N of M repos checked` — while the piles themselves still land together,
    /// in path order, with `scan_seq` numbered that way. The hook runs under the engine
    /// lock: keep it to a counter or a `try_send`.
    pub fn scan_all_with(
        &mut self,
        on_scanned: &(dyn Fn(&Path, usize) + Sync),
    ) -> Vec<(PathBuf, u64, Result<Pile, EngineError>)> {
        let mut results: BTreeMap<PathBuf, (u64, Result<Pile, EngineError>)> = BTreeMap::new();
        for _ in 0..3 {
            let todo: Vec<PathBuf> = self
                .root_paths()
                .into_iter()
                .filter(|p| !results.contains_key(p))
                .collect();
            if todo.is_empty() {
                break;
            }
            // Scanned on up to `parallelism` threads, each worker owning one root's state
            // outright; then applied here, serially, in path order. `scan_seq` is handed
            // out only in this serial step, so the numbering is the path order and never
            // the order the workers happened to finish in.
            let ctx = self.scan_ctx();
            let width = self.options.parallelism;
            let mut states: Vec<(PathBuf, &mut RootState)> = self
                .roots
                .iter_mut()
                .filter(|(p, _)| todo.contains(p))
                .map(|(p, s)| (p.clone(), s))
                .collect();
            states.sort_by(|a, b| a.0.as_os_str().cmp(b.0.as_os_str()));
            let scanned = parallel_map(states, width, |(p, state)| {
                let started = std::time::Instant::now();
                let r = scan_root(state, &ctx);
                on_scanned(&p, r.as_ref().map(|pile| pile.rows.len()).unwrap_or(0));
                (p, started, r)
            });
            for (p, started, r) in scanned {
                self.scan_seq += 1;
                trace_scan_done(
                    &p,
                    &started,
                    r.as_ref().map(|pile| pile.rows.len()).unwrap_or(0),
                    self.scan_seq,
                );
                results.insert(p, (self.scan_seq, r));
            }
            // A root that is in `todo` but no longer in `roots` cannot happen (both come
            // from the same map), but a missing one must still be recorded so the retry
            // loop terminates.
            for p in todo {
                results
                    .entry(p.clone())
                    .or_insert_with(|| (self.scan_seq, Err(EngineError::NoSuchRoot(p.clone()))));
            }
            // Discovery re-runs only when a scan saw the set of nested repos change (a
            // new `dir/` in `ls-files --others`), not on every call while one exists.
            let mut nested_changed = false;
            for r in self.roots.values_mut() {
                nested_changed |= std::mem::take(&mut r.nested_changed);
            }
            if !nested_changed {
                break;
            }
            match self.rescan() {
                Ok(changed) if !changed.added.is_empty() => continue,
                _ => break,
            }
        }
        let mut v: Vec<(PathBuf, u64, Result<Pile, EngineError>)> = results
            .into_iter()
            .map(|(p, (seq, r))| (p, seq, r))
            .collect();
        v.sort_by(|a, b| a.0.as_os_str().cmp(b.0.as_os_str()));
        v
    }

    /// The hunks of one **collapsed** row, computed on demand and capped at
    /// [`hunks::EXPAND_LINE_CAP`] body lines (§10 2026-09-05 ruling 1; Phase 6
    /// deliverable 4). The `e` key's engine half — never part of a scan, because the
    /// whole point of a collapsed class is that a lockfile rewrite does not pay a second
    /// Myers pass on every rescan.
    ///
    /// The diff is computed from **the row's own** `baseline`/`current` oids, not from a
    /// fresh resolution, so the expansion shows exactly the delta the row's counts were
    /// rendered from even if the file has moved since.
    ///
    /// A side the row does not have (an added or a deleted file) is legitimately empty.
    /// A side the row *does* have whose oid the store cannot produce is
    /// [`EngineError::MissingBlob`], never an empty side: reading a missing **current**
    /// blob as empty would draw the live file as one enormous deletion, hiding exactly
    /// what the reviewer pressed `e` to read (invariant 2 is over-show, never hide, and
    /// the scan's own `render_content` pushes a notice for the same condition). The only
    /// way to reach it is a state dir replaced between the scan that produced the row and
    /// the expansion — the caller's answer is "refresh", not "trust this diff".
    ///
    /// The result is a view. It is never written back onto the [`Row`]: `accept_file` and
    /// `accept_all` must stay whole-row for a collapsed path (§6.3 "single accept").
    pub fn hunks_of(&self, root: &Path, row: &Row) -> Result<hunks::Expanded, EngineError> {
        let state = self
            .roots
            .get(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        if row.collapsed == Some(crate::scan::Collapsed::Binary) {
            return Err(EngineError::BinaryRow {
                root: root.to_path_buf(),
                path: row.path_lossy(),
            });
        }
        let mut wanted: Vec<Oid> = [&row.baseline, &row.current]
            .into_iter()
            .flatten()
            .map(|e| e.oid.clone())
            .collect();
        wanted.sort();
        wanted.dedup();
        let blobs = state.store.cat_blobs(&wanted)?;
        let side = |e: &Option<crate::scan::Entry>| -> Result<Vec<u8>, EngineError> {
            match e {
                None => Ok(Vec::new()),
                Some(entry) => {
                    blobs
                        .get(&entry.oid)
                        .cloned()
                        .ok_or_else(|| EngineError::MissingBlob {
                            root: root.to_path_buf(),
                            oid: entry.oid.as_str().to_owned(),
                        })
                }
            }
        };
        let mut out = hunks::expand(&side(&row.baseline)?, &side(&row.current)?);
        // A collapsed row never reaches `render_content`'s mode-change synthesis (it
        // returns at the collapse early-out, `scan.rs`), so a `chmod +x` on a lockfile is a
        // row with zero content hunks. Without this the expansion would be an empty pane
        // for the one change the row is about; the gate is the scan's own.
        if let (Some(b), Some(c)) = (&row.baseline, &row.current)
            && b.mode != c.mode
            && (state.store.filemode()
                || b.mode == crate::git::Mode::Symlink
                || c.mode == crate::git::Mode::Symlink)
        {
            let mut h = hunks::Hunk::mode_change(b.mode.as_str(), c.mode.as_str());
            h.index = out.hunks.len();
            out.hunks.push(h);
        }
        Ok(out)
    }

    /// Re-inspect HEAD; when it moved (or an operation finished), scan and describe it.
    pub fn inspect_head(&mut self, root: &Path) -> Result<Option<HeadChange>, EngineError> {
        let clock = self.options.clock.clone();
        let state = self
            .roots
            .get_mut(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        // R1: the record in force follows `<git_dir>/HEAD`, and this is the event that
        // usually carries the checkout. It runs before the heads are compared so the notice
        // below reports the delta against the branch's own baseline, not the one it left.
        state.sync_branch(clock.as_ref());
        let state = &*state;
        let Some(rg) = &state.repo else {
            return Ok(None);
        };
        let next = headstate::inspect(rg)?;
        let prev = state.head.clone();
        if next == prev {
            // Nothing to describe, so nothing may keep the first-sight wording waiting: the
            // sync above (or the one a scan ran) may have first-sighted a branch whose
            // checkout `HEAD` had already reached by the time this inspection ran, and the
            // *next* head move is not that first sight.
            if let Some(s) = self.roots.get_mut(root) {
                s.first_sight_from = None;
                s.fold_skipped = None;
            }
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
        let seq = self.scan_seq;
        // R8: taken, not read, so the first-sight wording is used once. The scan above
        // cannot have set it (the sync at the top of this function already moved the
        // record in force), but it is taken after the scan so a switch either sync saw
        // reaches this notice.
        let (first_sight_from, fold_skipped) = match self.roots.get_mut(root) {
            Some(s) => (s.first_sight_from.take(), s.fold_skipped.take()),
            None => (None, None),
        };
        let facts = TransitionFacts {
            commits,
            files_differ: pile.rows.len(),
            first_sight_from,
            fold_skipped,
        };
        let notice = headstate::transition(&prev, &next, hint.as_ref(), &facts);
        Ok(Some(HeadChange {
            root: root.to_path_buf(),
            from: prev.head,
            to: next.head.clone(),
            branch: next.branch.clone(),
            notice,
            seq,
            pile,
        }))
    }

    /// Run one accept on `root` and rescan it: the op and the scan are one critical section
    /// for whoever holds the engine (the watcher's mutex), so nothing observes the ledger
    /// between them and the returned pile is the one the UI should show next. The path the
    /// TUI takes; `Ok` with refusals is a normal outcome (the pile still shows the row).
    pub fn accept(&mut self, root: &Path, req: AcceptRequest) -> Result<Accepted, EngineError> {
        self.accept_with(root, req, &NoFault)
    }

    /// [`Engine::accept`] with a fault injector. An op that fails (`Err`) wrote nothing the
    /// ledger's rename did not land; the engine re-reads the ledger the disk still has so
    /// the next scan shows the pre-accept pile rather than the staged one.
    pub fn accept_with(
        &mut self,
        root: &Path,
        req: AcceptRequest,
        fault: &dyn FaultInjector,
    ) -> Result<Accepted, EngineError> {
        let result = {
            let mut ops = self.ops(root)?;
            match &req {
                AcceptRequest::Hunk {
                    rendered,
                    hunks,
                    index,
                } => ops.accept_hunk(rendered, hunks, *index, fault),
                AcceptRequest::File(rendered) if rendered.oid.is_none() => {
                    ops.accept_deletion(rendered, fault)
                }
                AcceptRequest::File(rendered) => ops.accept_file(rendered, fault),
                AcceptRequest::Group { rows, rendered_on } => {
                    ops.accept_group(rows, rendered_on.as_deref(), fault)
                }
                AcceptRequest::All(pile) => ops.accept_all(pile, fault),
            }
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                if let Some(state) = self.roots.get_mut(root) {
                    state.ledger_stamp = None;
                    state.reload_ledger_if_changed();
                }
                return Err(EngineError::Ops(e));
            }
        };
        let pile = self.scan(root)?;
        Ok(Accepted {
            outcome,
            seq: self.scan_seq,
            pile,
        })
    }

    /// Put one hunk or one file back to its baseline in the working tree (§6.3).
    ///
    /// The same op-then-rescan critical section as [`Engine::accept_with`], for the same
    /// reason: the pile the caller gets back is the state *after* the write, so a hunk
    /// restore's remaining hunks and a file restore's cleared row are visible without a
    /// second round trip. The rescan is also the §11 mitigation for the hash-then-rename
    /// window — if the file moved inside it, the next pile says so.
    pub fn restore(&mut self, root: &Path, req: RestoreRequest) -> Result<Restored, EngineError> {
        self.restore_with(root, req, &NoFault)
    }

    /// [`Engine::restore`] with a fault injector.
    pub fn restore_with(
        &mut self,
        root: &Path,
        req: RestoreRequest,
        fault: &dyn FaultInjector,
    ) -> Result<Restored, EngineError> {
        let result = {
            let mut ops = self.ops(root)?;
            match &req {
                RestoreRequest::Hunk {
                    rendered,
                    hunks,
                    index,
                } => ops.restore_hunk(rendered, hunks, *index, fault),
                RestoreRequest::File(rendered) if rendered.oid.is_none() => {
                    ops.restore_deletion(rendered, fault)
                }
                RestoreRequest::File(rendered) => ops.restore_file(rendered, fault),
            }
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                // A restore writes no ledger, so there is nothing staged to roll back —
                // but the reload costs one read and keeps the failure path identical to
                // accept's, which is worth more than the read.
                if let Some(state) = self.roots.get_mut(root) {
                    state.ledger_stamp = None;
                    state.reload_ledger_if_changed();
                }
                return Err(EngineError::Ops(e));
            }
        };
        let pile = self.scan(root)?;
        Ok(Restored {
            outcome,
            seq: self.scan_seq,
            pile,
        })
    }

    /// What the work tree holds at `path` **right now** — one `lstat` plus, for a file,
    /// one `hash-object -w` (Phase 8 deliverable 3).
    ///
    /// The read the post-`$EDITOR` blessing is built on: the TUI has a [`Rendered`] row
    /// from before the editor ran and needs to know what the editor left behind. It is a
    /// plain read — no ledger, no override, no scan — so it stays cheap enough to run on
    /// the one path the user just edited, and it is the same `hash_path` a scan would use,
    /// which is what makes the oid it returns comparable with a row's.
    ///
    /// A blessing built on this answer is the **one** deliberate exception to invariant 3
    /// (accept is never a fresh read), and it is guarded by a confirm the user answers:
    /// §6.3's editor-save row and §11's residual both say so (Amendment v1.8, ruling P1).
    pub fn current(&self, root: &Path, path: &[u8]) -> Result<Current, EngineError> {
        let state = self
            .roots
            .get(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        Ok(state.store.hash_path(path))
    }

    /// The live bytes of `rendered`, for the inline editor to open on — or the refusal that
    /// says why the file will not go in a text buffer (Phase 8 deliverable 8; F11).
    ///
    /// The size cap and the binary rule are **engine** decisions, not the reducer's: the cap
    /// is `[config] collapse_size_bytes`, which the TUI never sees, and "binary" here means
    /// exactly what a text buffer cannot hold — bytes that are not UTF-8, or that carry a
    /// NUL. Both come back as [`Refused::NotEditable`], whose `why` the status line prints
    /// on its own (`use shift-i: <why>`), so the user is pointed at the key that *can* open
    /// the file.
    ///
    /// The order is CAS, read, **re-hash what was read**: [`ops::cas_live`] proves the file
    /// is still the row that was drawn, and hashing the bytes that came back closes the
    /// window between that check and the read — an agent that rewrote the file in between
    /// gets a [`Refused::Moved`] rather than an editor full of content the row never
    /// described. Nothing is written and no ledger lock is taken.
    pub fn read_rendered(&self, root: &Path, rendered: &Rendered) -> Result<Vec<u8>, Refused> {
        let not_editable = |why: &str| Refused::NotEditable {
            path: rendered.path.clone(),
            why: why.to_owned(),
        };
        let unhashable = |reason: String| Refused::Unhashable {
            path: rendered.path.clone(),
            reason,
        };
        // The same two rows `Ops::save_file` refuses outright: a deletion has no file to
        // open, and a symlink's content is its target — opening it would edit whatever it
        // points at, which is not the row on screen.
        if rendered.oid.is_none() {
            return Err(not_editable("the file is gone"));
        }
        if rendered.mode == Some(Mode::Symlink) {
            return Err(not_editable("not a regular file"));
        }
        let state = self
            .roots
            .get(root)
            .ok_or_else(|| unhashable(format!("no such root: {}", root.display())))?;
        let live = ops::cas_live(&state.store, rendered)?;
        let full = state
            .store
            .root()
            .join(std::ffi::OsStr::from_bytes(&rendered.path));
        let bytes = std::fs::read(&full).map_err(|e| unhashable(e.to_string()))?;
        // The read is a *fresh* read, so it is hashed through the store's own conversion
        // (the same one `hash_path` applies) and compared with the row. A mismatch is the
        // agent that wrote between the CAS and the read.
        let oid = state
            .store
            .hash_bytes_as(&rendered.path, &bytes)
            .map_err(|e| unhashable(e.to_string()))?;
        if rendered.oid.as_ref() != Some(&oid) {
            return Err(Refused::Moved {
                path: rendered.path.clone(),
                live: Some(Entry {
                    oid,
                    mode: live.mode,
                }),
            });
        }
        let cap = self.config.collapse_size_bytes;
        if bytes.len() as u64 > cap {
            return Err(not_editable(&format!("over {} KiB", cap / 1024)));
        }
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            return Err(not_editable("binary"));
        }
        Ok(bytes)
    }

    /// Write an editor buffer back to the working tree and advance the path's baseline to
    /// it (§6.3 "editor save"; invariant 8 — the user is never asked to review their own
    /// just-typed change).
    ///
    /// The same op-then-rescan critical section as [`Engine::accept_with`], and for the
    /// extra reason that a save has: `pile` is what proves the invariant, because a clean
    /// save must leave the row *gone*. The rescan is also the §11 mitigation for the
    /// hash-then-rename window, exactly as it is for a restore.
    pub fn save(&mut self, root: &Path, req: SaveRequest) -> Result<Saved, EngineError> {
        self.save_with(root, req, &NoFault)
    }

    /// [`Engine::save`] with a fault injector.
    pub fn save_with(
        &mut self,
        root: &Path,
        req: SaveRequest,
        fault: &dyn FaultInjector,
    ) -> Result<Saved, EngineError> {
        let result = {
            let mut ops = self.ops(root)?;
            ops.save_file(&req.rendered, &req.bytes, fault)
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                // The file may well be on disk — the failure is the ledger's — so the
                // staged override is dropped and the on-disk ledger re-read. The next scan
                // then shows the saved bytes as *pending*, which is the honest answer.
                if let Some(state) = self.roots.get_mut(root) {
                    state.ledger_stamp = None;
                    state.reload_ledger_if_changed();
                }
                return Err(EngineError::Ops(e));
            }
        };
        let pile = self.scan(root)?;
        Ok(Saved {
            outcome,
            seq: self.scan_seq,
            pile,
        })
    }

    /// Reverse the most recent accept in `root` (Amendment v1.11, deliverable 2).
    ///
    /// Op-then-rescan like [`Engine::accept_with`], and for the same reason: the pile that
    /// comes back is the one the UI should show next, with the undone paths pending again.
    /// `paths` is what the entry put back, in path order, so the caller can move the
    /// selection to the first of them; an empty stack comes back as
    /// [`Refused::NothingToUndo`] in `outcome`, never as `Err`.
    pub fn undo(&mut self, root: &Path) -> Result<Undone, EngineError> {
        self.undo_with(root, &NoFault)
    }

    /// [`Engine::undo`] with a fault injector.
    pub fn undo_with(
        &mut self,
        root: &Path,
        fault: &dyn FaultInjector,
    ) -> Result<Undone, EngineError> {
        let (result, preview) = {
            let mut ops = self.ops(root)?;
            let preview = ops.undo_preview();
            (ops.undo(fault), preview)
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                if let Some(state) = self.roots.get_mut(root) {
                    state.ledger_stamp = None;
                    state.reload_ledger_if_changed();
                }
                return Err(EngineError::Ops(e));
            }
        };
        let (op, paths) = match preview {
            Some((op, paths)) if outcome.ok() => (Some(op), paths),
            _ => (None, Vec::new()),
        };
        let pile = self.scan(root)?;
        Ok(Undone {
            outcome,
            op,
            paths,
            seq: self.scan_seq,
            pile,
        })
    }

    /// Snooze `root` for `days` (1 to 365), or wake it when `days` is `None`.
    ///
    /// The deadline is computed from the engine's injected clock, so a `FixedClock` test
    /// can name the date the status line will print.
    pub fn snooze(&mut self, root: &Path, days: Option<u32>) -> Result<Snoozed, EngineError> {
        let (result, until) = {
            let mut ops = self.ops(root)?;
            match days {
                Some(d) => {
                    let until = ops.snooze_deadline(d);
                    (ops.snooze(&until, &NoFault), Some(until))
                }
                None => (ops.unsnooze(&NoFault), None),
            }
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                if let Some(state) = self.roots.get_mut(root) {
                    state.ledger_stamp = None;
                    state.reload_ledger_if_changed();
                }
                return Err(EngineError::Ops(e));
            }
        };
        let pile = self.scan(root)?;
        Ok(Snoozed {
            outcome,
            until,
            seq: self.scan_seq,
            pile,
        })
    }

    /// Flag `path` — optionally one hunk of it — with a note, and render the export.
    ///
    /// Op-then-rescan like [`Engine::accept_with`], though a flag never changes a baseline:
    /// the rescan is what puts the new `⚑` on the row the UI is about to draw. The
    /// `hunk n of **m**` total does **not** come from it — [`RenderedHunk::of`] carries the
    /// count the caller had on screen (verifier F5), because a rescan reads the file as it
    /// is now and an agent that rewrote it between the render and the keystroke would
    /// otherwise produce a `hunk 2 of 1` that never existed. `summary` travels with a
    /// whole-file flag for the same reason (Amendment v1.8) and is ignored for a hunk flag.
    pub fn flag(
        &mut self,
        root: &Path,
        path: &[u8],
        note: &str,
        hunk: Option<RenderedHunk>,
        summary: Option<FlagSummary>,
    ) -> Result<Flagged, EngineError> {
        self.flag_with(root, path, note, hunk, summary, &NoFault)
    }

    /// [`Engine::flag`] with a fault injector.
    pub fn flag_with(
        &mut self,
        root: &Path,
        path: &[u8],
        note: &str,
        hunk: Option<RenderedHunk>,
        summary: Option<FlagSummary>,
        fault: &dyn FaultInjector,
    ) -> Result<Flagged, EngineError> {
        // The total the export will name, taken now, from what the caller rendered — not
        // from the pile the rescan below produces (F5).
        let of = hunk.as_ref().map(|h| h.of);
        let hunk = hunk.map(|h| h.hunk);
        let (outcome, written) = {
            let mut ops = self.ops(root)?;
            match ops.flag(path, note, hunk, summary, fault) {
                Ok(o) => {
                    // The flag as the ledger now holds it: `created_at` is the op's clock
                    // reading, which the UI has no way to reproduce.
                    let flag = String::from_utf8(path.to_vec())
                        .ok()
                        .and_then(|k| ops.ledger.overrides.get(&k))
                        .and_then(|o| o.flags.last())
                        .cloned();
                    (o, flag)
                }
                Err(e) => {
                    if let Some(state) = self.roots.get_mut(root) {
                        state.ledger_stamp = None;
                        state.reload_ledger_if_changed();
                    }
                    return Err(EngineError::Ops(e));
                }
            }
        };
        let pile = self.scan(root)?;
        let export = match written {
            Some(flag) => crate::flags::export(
                &crate::flags::ExportContext {
                    root: root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| root.to_string_lossy().into_owned()),
                    of,
                    attribution: None,
                },
                path,
                &flag,
            ),
            None => String::new(),
        };
        Ok(Flagged {
            outcome,
            export,
            seq: self.scan_seq,
            pile,
        })
    }

    /// Clear **every** flag on `path`. The export is empty: there is nothing to send.
    pub fn unflag(&mut self, root: &Path, path: &[u8]) -> Result<Flagged, EngineError> {
        self.unflag_with(root, path, &NoFault)
    }

    /// [`Engine::unflag`] with a fault injector.
    pub fn unflag_with(
        &mut self,
        root: &Path,
        path: &[u8],
        fault: &dyn FaultInjector,
    ) -> Result<Flagged, EngineError> {
        let outcome = {
            let mut ops = self.ops(root)?;
            match ops.unflag(path, fault) {
                Ok(o) => o,
                Err(e) => {
                    if let Some(state) = self.roots.get_mut(root) {
                        state.ledger_stamp = None;
                        state.reload_ledger_if_changed();
                    }
                    return Err(EngineError::Ops(e));
                }
            }
        };
        let pile = self.scan(root)?;
        Ok(Flagged {
            outcome,
            export: String::new(),
            seq: self.scan_seq,
            pile,
        })
    }

    /// The accept operations for one root.
    pub fn ops(&mut self, root: &Path) -> Result<Ops<'_>, EngineError> {
        let threshold = self.options.compaction_threshold;
        let clock: &dyn Clock = self.options.clock.as_ref();
        let state = self
            .roots
            .get_mut(root)
            .ok_or_else(|| EngineError::NoSuchRoot(root.to_path_buf()))?;
        let case_insensitive = state.case_insensitive;
        // R5: the branch this op is staged under, and the `HEAD` file `commit` re-reads
        // under the lock to prove it is still in force.
        let branch = state.ledger.seen_branch.clone();
        let git_dir =
            (!state.head.git_dir.as_os_str().is_empty()).then(|| state.head.git_dir.clone());
        Ok(Ops {
            store: &state.store,
            index: &state.index,
            repo: state.repo.as_ref(),
            paths: &state.paths,
            branch,
            git_dir,
            ledger: &mut state.ledger,
            tree: &mut state.tree,
            clock,
            compaction_threshold: threshold,
            case_insensitive,
            staged: BTreeMap::new(),
            pending_undo: None,
            lock: crate::ops::DEFAULT_LOCK,
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

/// The `org/repo` slug of a remote URL for the nav's dimmed remote label (§6.7):
/// `git@host:org/repo(.git)`, `ssh://git@host/org/repo(.git)`, `https://host/org/repo(.git)`
/// and scp-like `host:org/repo` all give `org/repo` (the last two path segments). A local
/// path or a `file://` URL gives `None` — the fixture repos' origins are temp-dir paths, so
/// anything else would leak a per-run name into the golden and every snapshot — as does
/// anything unparsable.
pub fn remote_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") || scheme.is_empty() {
            return None;
        }
        // `[user@]host[:port]/path`
        rest.split_once('/')?.1
    } else if url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        return None;
    } else {
        // scp-like `[user@]host:path`; a `/` before the colon means a local path.
        let (host, path) = url.split_once(':')?;
        if host.is_empty() || host.contains('/') {
            return None;
        }
        path
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let n = segments.len();
    if n < 2 {
        return None;
    }
    Some(format!("{}/{}", segments[n - 2], segments[n - 1]))
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
    // `Some` for a watched folder: how much of it this root covers, and the folders
    // inside it another root looks after. `None` for a repository, whose first sight
    // comes from `HEAD`.
    watched: Option<(&DraftScope, &[ExcludedDir])>,
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
            // Only what the folder covers, and nothing at or above the size limit: a
            // first sight of a folder holding a disk image never copies it.
            DraftInitial::Seen => match watched {
                Some((scope, excluded)) => Some(store.tree_of_disk(scope, excluded)?),
                None => None,
            },
            DraftInitial::Pending => None,
        },
    };
    let mut ledger = Ledger::new(
        root,
        kind,
        seen_tree,
        SeenAt {
            head_commit: head.head.clone(),
            branch: head.branch.clone(),
            at: clock.now_iso8601(),
        },
    );
    // R7: a root's first sight is unchanged and names the branch it happened on, so the
    // first sync is a no-op rather than an adoption.
    ledger.seen_branch = head.branch.clone();
    // R2's seen-state target: the commit the root was first sighted at, set once and never
    // changed again. `None` at an unborn head and for a draft root.
    if matches!(kind, RootKind::Git) {
        ledger.first_sight_head = head.head.clone();
    }
    Ok(ledger)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::ConfigSource;
    use crate::ledger::FixedClock;
    use crate::scan::{Change, Row};
    use crate::status::StatusReport;
    use crate::store::tests::fixture_env;
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;
    use std::time::Duration;

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
        open_engine_with(repo, state, config, EngineOptions::default())
    }

    pub(crate) fn open_engine_with(
        repo: &FixtureRepo,
        state: &TempDir,
        config: Config,
        options: EngineOptions,
    ) -> Engine {
        let (loaded, resolved) = loaded_for(repo, state, config);
        let env = fixture_env(repo, state);
        Engine::open(&loaded, &resolved, &env, options).unwrap()
    }

    fn only_root(engine: &Engine) -> PathBuf {
        let roots = engine.roots();
        assert_eq!(roots.len(), 1, "{:?}", engine.root_paths());
        roots[0].path.clone()
    }

    /// Six roots in one parent dir, so a pool has something to spread.
    fn six_roots(state: &TempDir) -> (PathBuf, Env, Vec<FixtureRepo>, TempDir) {
        let parent = TempDir::new("lc-many");
        let parent_path = parent.path().to_path_buf();
        let repos: Vec<FixtureRepo> = (0..6)
            .map(|i| FixtureRepo::new_in(TempDir::adopt(&parent_path), &format!("r{i}")).unwrap())
            .collect();
        // Each root gets a different amount of unseen work, so the workers finish out of
        // path order and a result that depended on completion order would show.
        for (i, r) in repos.iter().enumerate() {
            for f in 0..=i {
                r.write(
                    &format!("f{f}.txt"),
                    format!("root {i} file {f}\nsecond line\n"),
                );
            }
        }
        let home = parent_path.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let env = Env::empty(&parent_path)
            .with_home(home)
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1")
            .with_var("LASTCALL_STATE_DIR", state.path().to_string_lossy());
        // The TempDir goes back to the caller: the fixtures live under it, and dropping
        // it at the end of the test removes all six roots in one go.
        (
            std::fs::canonicalize(&parent_path).unwrap(),
            env,
            repos,
            parent,
        )
    }

    fn many_root_engine(parent: &Path, env: &Env, state: &TempDir, width: usize) -> Engine {
        let loaded = Loaded {
            config: Config {
                parent_dirs: vec![parent.to_path_buf()],
                ..Config::default()
            },
            source: ConfigSource::Defaults { searched: vec![] },
            state_dir: state.path().to_path_buf(),
        };
        let resolved = Resolved {
            parent_dirs: vec![parent.to_path_buf()],
            notices: vec![],
        };
        let options = EngineOptions {
            parallelism: width,
            ..EngineOptions::default()
        };
        Engine::open(&loaded, &resolved, env, options).unwrap()
    }

    /// Deliverable 1a/1b: the pool is a performance change and nothing else. Opening and
    /// scanning six roots at width 1 and at width 4 must produce byte-identical state —
    /// the same roots in the same order, the same ledgers, the same piles, and the same
    /// `scan_seq` per root, which is only true because the serial apply step hands
    /// `scan_seq` out in path order rather than in completion order.
    #[test]
    fn engine_parallel_open_and_scan_match_the_sequential_run_exactly() {
        let state1 = TempDir::new("lc-par1");
        let (parent, env, _repos, _dir) = six_roots(&state1);

        let describe = |engine: &mut Engine| -> Vec<String> {
            engine
                .scan_all()
                .into_iter()
                .map(|(p, seq, r)| {
                    let root = engine.root(&p).expect("scanned root is open");
                    let rows: Vec<String> = r
                        .as_ref()
                        .map(|pile| {
                            pile.rows
                                .iter()
                                .map(|row| {
                                    format!(
                                        "{} {:?}",
                                        String::from_utf8_lossy(&row.path),
                                        row.change
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_else(|e| vec![format!("ERR {e}")]);
                    format!(
                        "{} seq={seq} tree={:?} seen={:?} rows={rows:?}",
                        p.file_name().unwrap().to_string_lossy(),
                        root.ledger.seen_tree.as_ref().map(Oid::as_str),
                        root.ledger.seen_at.head_commit.as_ref().map(Oid::as_str),
                    )
                })
                .collect()
        };

        let mut serial = many_root_engine(&parent, &env, &state1, 1);
        assert_eq!(serial.roots().len(), 6, "{:?}", serial.root_paths());
        let a = describe(&mut serial);
        drop(serial);

        // A *fresh* state dir, so the parallel run does its own first sight too.
        let state2 = TempDir::new("lc-par4");
        let env2 = {
            let mut e = env.clone();
            e = e.with_var("LASTCALL_STATE_DIR", state2.path().to_string_lossy());
            e
        };
        let mut parallel = many_root_engine(&parent, &env2, &state2, 4);
        let b = describe(&mut parallel);
        assert_eq!(a, b, "width 1 and width 4 disagree");
        assert_eq!(a.len(), 6);
        // `scan_seq` is 1..=6 in path order in both runs.
        for (i, line) in a.iter().enumerate() {
            assert!(line.contains(&format!("seq={}", i + 1)), "{line}");
        }
    }

    /// A root the pool cannot open must not take the others down with it: it becomes a
    /// notice, the rest open, and the engine is usable.
    #[test]
    fn engine_parallel_open_reports_an_unopenable_root_as_a_notice() {
        let state = TempDir::new("lc-badroot");
        let (parent, env, repos, _dir) = six_roots(&state);
        // Make one root's state dir unusable by putting a *file* where its repo dir goes.
        // Discovery still finds the root; only its open fails.
        let engine = many_root_engine(&parent, &env, &state, 4);
        let victim = repos[2].path().to_path_buf();
        let victim = std::fs::canonicalize(&victim).unwrap();
        let repo_dir = engine.root(&victim).unwrap().paths.repo_dir.clone();
        drop(engine);
        let state2 = TempDir::new("lc-badroot2");
        let env2 = env.with_var("LASTCALL_STATE_DIR", state2.path().to_string_lossy());
        let rel = repo_dir.strip_prefix(state.path()).unwrap().to_path_buf();
        let blocked = state2.path().join(&rel);
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        std::fs::write(&blocked, b"not a directory").unwrap();

        let engine = many_root_engine(&parent, &env2, &state2, 4);
        assert_eq!(engine.roots().len(), 5, "{:?}", engine.root_paths());
        assert!(engine.root(&victim).is_none());
        assert!(
            engine
                .notices()
                .iter()
                .any(|n| n.contains(&victim.display().to_string()) && n.contains("cannot open")),
            "{:?}",
            engine.notices()
        );
    }

    /// A parent whose `meta.json` cannot be written is reported once per root and none of
    /// its roots is opened — not a "cannot open" notice for a root that is then open, and
    /// not two notices for one root (verifier (a) F2).
    #[test]
    fn engine_open_with_an_unwritable_parent_meta_opens_nothing_and_notices_once_per_root() {
        let state = TempDir::new("lc-badmeta");
        let (parent, env, repos, _dir) = six_roots(&state);
        let engine = many_root_engine(&parent, &env, &state, 4);
        let meta = {
            let root = std::fs::canonicalize(repos[0].path()).unwrap();
            let repo_dir = engine.root(&root).unwrap().paths.repo_dir.clone();
            // `<parent>/repos/<id>` → the parent's dir is two levels up.
            repo_dir.parent().unwrap().parent().unwrap().to_path_buf()
        };
        drop(engine);
        let state2 = TempDir::new("lc-badmeta2");
        let env2 = env.with_var("LASTCALL_STATE_DIR", state2.path().to_string_lossy());
        let rel = meta.strip_prefix(state.path()).unwrap().to_path_buf();
        let blocked = state2.path().join(&rel);
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        std::fs::write(&blocked, b"not a directory").unwrap();

        let engine = many_root_engine(&parent, &env2, &state2, 4);
        assert!(engine.roots().is_empty(), "{:?}", engine.root_paths());
        for repo in &repos {
            let root = std::fs::canonicalize(repo.path()).unwrap();
            let n = engine
                .notices()
                .iter()
                .filter(|n| n.contains(&root.display().to_string()) && n.contains("cannot open"))
                .count();
            assert_eq!(n, 1, "{}: {:?}", root.display(), engine.notices());
        }
    }

    /// The per-root git process budget (Phase 5 deliverable 1c). Both figures are measured
    /// on the calling thread — `thread_spawn_count` rather than the process-wide counter,
    /// which is a race in a test binary that runs tests in parallel.
    ///
    /// The budget is the point of the deliverable: if a later change reintroduces a
    /// per-key `config --get` or a per-path `rev-parse`, these numbers move and this fails.
    #[test]
    fn engine_open_and_scan_stay_inside_the_per_root_git_budget() {
        let repo = FixtureRepo::new("budget").unwrap();
        let state = TempDir::new("lc-budget");
        let (loaded, resolved) = loaded_for(&repo, &state, Config::default());
        let env = fixture_env(&repo, &state);

        // Discovery, then one root opened on this thread: the per-root open cost.
        let engine = Engine::open(&loaded, &resolved, &env, EngineOptions::default()).unwrap();
        let root = only_root(&engine);
        let d = roots::DiscoveredRoot {
            path: root.clone(),
            kind: RootKind::Git,
            parent: std::fs::canonicalize(repo.parent_dir()).unwrap(),
            badge: None,
            scope: None,
            name: "budget".to_owned(),
            excluded_dirs: Vec::new(),
        };
        drop(engine);
        let engine = Engine::open(&loaded, &resolved, &env, EngineOptions::default()).unwrap();
        let before = crate::git::thread_spawn_count();
        let opened = engine.open_root(&d).unwrap();
        let open_spawns = crate::git::thread_spawn_count() - before;
        drop(opened);
        eprintln!("BUDGET open_spawns_per_root={open_spawns}");
        assert!(
            open_spawns <= 10,
            "opening one root costs {open_spawns} git processes, budget 10"
        );

        // One scan of one root, on this thread, in the bench's pile shape: an edit, an
        // add and a delete together, so rename detection runs through `index.<pid>.tmp`
        // — the most expensive scan shape, and the one S1/S1h measure (17 per root).
        // An add alone costs 12, an edit alone 13; the ceiling guards the worst shape.
        let mut engine = engine;
        engine.scan(&root).unwrap();
        repo.write("f1", "a1\nchanged\na3\na4\na5\na6\na7\na8\na9\na10\n");
        repo.write("budget.txt", "one\ntwo\nthree\n");
        repo.remove("f3");
        let before = crate::git::thread_spawn_count();
        let pile = engine.scan(&root).unwrap();
        let scan_spawns = crate::git::thread_spawn_count() - before;
        eprintln!(
            "BUDGET scan_spawns_per_root={scan_spawns} rows={}",
            pile.rows.len()
        );
        assert_eq!(pile.rows.len(), 3, "edit, add and delete are three rows");
        // Measured, not aspirational. This shape is 17 today (read-tree seeding aside:
        // one `update-index --refresh`, `diff-files`, three `ls-files`, the four-spawn
        // head inspection, **one** `for-each-ref refs/remotes` for the classification
        // key, `hash-object --stdin-paths`, `cat-file --batch`, and rename detection's
        // passes over the temp index). Phase 6 deliverable 5 removed the second listing:
        // this shape re-uses a memoized classification, so it paid one listing already,
        // but every scan whose key moved (a fetch, a commit) paid two and now pays one —
        // `engine_classification_lists_remote_refs_once_per_scan` is the assertion, and
        // the ceiling here tightened 18 → 17 to hold the win. 1c reduces the *open*; the
        // scan pipeline is not batchable without a redesign (see docs/dev/bench.md).
        assert!(
            scan_spawns <= 17,
            "scanning one root costs {scan_spawns} git processes, budget 17"
        );
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

        let a = StatusReport::build(&mut engine, None).unwrap().to_json();
        let b = StatusReport::build(&mut engine, None).unwrap().to_json();
        assert_eq!(a, b, "status is byte-stable across scans");
        assert!(a.contains("\"status_version\": 1"));
        assert!(!a.contains("\"at\""), "no timestamps in status: {a}");
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["roots"][0]["pending"][0]["path"], "f1");
        assert_eq!(v["roots"][0]["pending"][0]["change"], "modified");
        assert_eq!(v["roots"][0]["pending"][1]["change"], "added");
        assert_eq!(v["roots"][0]["branch"], "main");
        let human = StatusReport::build(&mut engine, None)
            .unwrap()
            .render_human();
        assert!(
            human.starts_with(&format!("state dir: {}\n\n", state.path().display())),
            "{human}"
        );
        assert!(human.contains("\neng (main)  2 pending\n"), "{human}");
        assert!(human.contains("  M f1  +1 −"), "{human}");
        assert!(human.contains("  A new.txt  +1 −0"), "{human}");
    }

    /// Amendment v1.9: the report names the store it read — the state dir once at the top,
    /// each root's `<repo-hash>` directory and the ledger's write time — so two runs over
    /// two state dirs can be told apart from their output alone (the Gate 8 discrepancy).
    #[test]
    fn status_json_names_the_state_dir_and_each_roots_store() {
        let repo = FixtureRepo::new("eng-store").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let repo_dir = engine.root(&root).unwrap().paths.repo_dir.clone();
        let ledger_path = engine.root(&root).unwrap().paths.ledger.clone();

        let report = StatusReport::build(&mut engine, None).unwrap();
        assert_eq!(report.state_dir, state.path().to_string_lossy());
        assert_eq!(report.roots.len(), 1);
        let r = &report.roots[0];
        assert_eq!(r.store, repo_dir.file_name().unwrap().to_string_lossy());
        assert_eq!(
            r.store.len(),
            16,
            "the repo hash is sixteen hex: {}",
            r.store
        );
        assert!(
            r.store.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}",
            r.store
        );
        assert!(
            repo_dir.ends_with(&r.store) && ledger_path.starts_with(&repo_dir),
            "the named store is the directory this root's ledger lives in"
        );

        let written = r.ledger_written_at.clone().expect("first sight wrote one");
        let mtime = std::fs::metadata(&ledger_path).unwrap().modified().unwrap();
        assert_eq!(written, crate::ledger::iso8601(mtime));
        assert!(
            written.len() == 20 && written.ends_with('Z') && written.starts_with("20"),
            "ISO-8601 UTC, second precision: {written}"
        );

        // The JSON carries all three, and the human form leads with the state dir.
        let v: serde_json::Value =
            serde_json::from_str(&StatusReport::build(&mut engine, None).unwrap().to_json())
                .unwrap();
        assert_eq!(v["state_dir"], report.state_dir);
        assert_eq!(v["roots"][0]["store"], r.store);
        assert_eq!(v["roots"][0]["ledger_written_at"], written);
        assert!(
            StatusReport::build(&mut engine, None)
                .unwrap()
                .render_human()
                .starts_with(&format!("state dir: {}\n", report.state_dir))
        );

        // A root whose ledger is gone reports `null`, never a guess.
        std::fs::remove_file(&ledger_path).unwrap();
        let gone = StatusReport::build(&mut engine, None).unwrap();
        assert_eq!(gone.roots[0].ledger_written_at, None);
        assert_eq!(gone.roots[0].store, r.store);
    }

    /// Amendment v1.11: `status --json` gains two additive per-root fields and the human
    /// report two suffixes. `status_version` stays 1; the golden
    /// (`crates/lastcall/tests/golden/status_multi_repo.json`) carries both at their zero
    /// values for every root.
    #[test]
    fn status_reports_the_undo_depth_and_the_snooze_deadline() {
        let repo = FixtureRepo::new("eng-status-ux").unwrap();
        let state = TempDir::new("lc-eng-state");
        // 2026-09-14T00:00:00Z, so a one-day snooze lands on 2026-09-15.
        let options = EngineOptions {
            clock: Arc::new(FixedClock::at_unix(1_789_344_000)),
            ..EngineOptions::default()
        };
        let mut engine = open_engine_with(&repo, &state, Config::default(), options);
        let root = only_root(&engine);
        assert!(engine.scan(&root).unwrap().is_empty(), "first sight");

        // Nothing accepted, nothing snoozed: both fields at their zero values.
        let clean = StatusReport::build(&mut engine, None).unwrap();
        assert_eq!(clean.roots[0].undo, 0);
        assert_eq!(clean.roots[0].snoozed_until, None);
        let v: serde_json::Value = serde_json::from_str(&clean.to_json()).unwrap();
        assert_eq!(v["roots"][0]["undo"], 0);
        assert_eq!(v["roots"][0]["snoozed_until"], serde_json::Value::Null);
        assert!(
            clean
                .render_human()
                .contains("\neng-status-ux (main)  0 pending\n"),
            "no suffix when there is neither: {}",
            clean.render_human()
        );

        // One accept, one snooze.
        repo.write("f1", "changed\n");
        let pile = engine.scan(&root).unwrap();
        let rendered = Rendered::of(pile.row(b"f1").unwrap());
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert!(acc.outcome.ok(), "{:?}", acc.outcome);
        assert_eq!(acc.pile.undo, 1, "the pile carries the depth to the UI");
        let snoozed = engine.snooze(&root, Some(1)).unwrap();
        assert_eq!(snoozed.until.as_deref(), Some("2026-09-15T00:00:00Z"));
        assert_eq!(
            snoozed.pile.snoozed_until.as_deref(),
            Some("2026-09-15T00:00:00Z")
        );

        let report = StatusReport::build(&mut engine, None).unwrap();
        assert_eq!(report.roots[0].undo, 1);
        assert_eq!(
            report.roots[0].snoozed_until.as_deref(),
            Some("2026-09-15T00:00:00Z")
        );
        let v: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(v["roots"][0]["undo"], 1);
        assert_eq!(v["roots"][0]["snoozed_until"], "2026-09-15T00:00:00Z");
        assert_eq!(v["status_version"], 1, "additive: the version stays 1");
        let human = report.render_human();
        assert!(
            human.contains(
                "\neng-status-ux (main)  0 pending · 1 undo · snoozed until 2026-09-15\n"
            ),
            "{human}"
        );

        // An expired snooze reads as none, and the read clears it on the next write.
        let options = EngineOptions {
            clock: Arc::new(FixedClock::at_unix(1_789_344_000 + 2 * 86_400)),
            ..EngineOptions::default()
        };
        let mut later = open_engine_with(&repo, &state, Config::default(), options);
        let report = StatusReport::build(&mut later, None).unwrap();
        assert_eq!(
            report.roots[0].snoozed_until, None,
            "a deadline in the past is not a snooze"
        );
        assert_eq!(report.roots[0].undo, 1, "the stack is untouched by expiry");
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

    /// A FixtureRepo `f1` with two separated edits (hunks at lines 1 and 10).
    const F1_TWO_HUNKS: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n";

    fn rendered_row(engine: &mut Engine, root: &Path, path: &[u8]) -> (Rendered, Row) {
        let pile = engine.scan(root).unwrap();
        let row = pile
            .row(path)
            .unwrap_or_else(|| panic!("{} is pending", String::from_utf8_lossy(path)))
            .clone();
        (Rendered::of(&row), row)
    }

    fn ledger_bytes(engine: &Engine, root: &Path) -> Vec<u8> {
        std::fs::read(&engine.root(root).unwrap().paths.ledger).unwrap()
    }

    /// Deliverable 5: the seam the TUI reducer calls. The export is built from the flag the
    /// ledger just took (its `created_at`, which the UI cannot reproduce) and the `of` count
    /// the caller rendered — and the flag is on disk whatever happens to the send.
    #[test]
    fn engine_flag_returns_the_export_for_the_flag_it_just_wrote() {
        let repo = FixtureRepo::new("eng-flag").unwrap();
        let state = TempDir::new("lc-eng-flag");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", F1_TWO_HUNKS);
        let (_, row) = rendered_row(&mut engine, &root, b"f1");
        assert_eq!(row.hunks.len(), 2);
        let hunk = RenderedHunk {
            hunk: FlagHunk {
                index: 1,
                header: "@@ -9,2 +9,2 @@".into(),
                text: "-a10\n+A10\n".into(),
            },
            of: row.hunks.len(),
        };
        let out = engine
            .flag(&root, b"f1", "why is this changed?", Some(hunk), None)
            .unwrap();
        assert!(out.outcome.ok() && out.outcome.written);
        let base = root.file_name().unwrap().to_string_lossy().into_owned();
        let created = engine.root(&root).unwrap().ledger.overrides["f1"].flags[0]
            .created_at
            .clone();
        assert_eq!(
            out.export,
            format!(
                "lastcall flag · {base} · f1 · hunk 2 of 2 · {created}\n\
                 note: why is this changed?\n\n\
                 ```diff\n@@ -9,2 +9,2 @@\n-a10\n+A10\n```"
            )
        );
        assert_eq!(out.seq, engine.scan_seq);
        assert_eq!(out.pile.row(b"f1").unwrap().flags.len(), 1);

        // A second, file-level flag: no hunk segment, no diff block, and it appends.
        let out = engine
            .flag(&root, b"f1", "and this file", None, None)
            .unwrap();
        assert!(!out.export.contains("```") && !out.export.contains("hunk"));
        assert!(out.export.ends_with("note: and this file"));
        assert_eq!(out.pile.row(b"f1").unwrap().flags.len(), 2);

        // Unflag clears all of them and has nothing to send.
        let out = engine.unflag(&root, b"f1").unwrap();
        assert!(out.outcome.ok() && out.export.is_empty());
        assert!(out.pile.row(b"f1").unwrap().flags.is_empty());
        assert!(
            !engine
                .root(&root)
                .unwrap()
                .ledger
                .overrides
                .contains_key("f1")
        );
    }

    /// The export names the total the **user saw**, not the one the file has by the time
    /// the flag lands (verifier F5).
    ///
    /// The probe: render a two-hunk file, let an agent rewrite it to one hunk before the
    /// keystroke, then flag hunk index 1. Taking `of` from the post-op rescan printed
    /// `hunk 2 of 1` — a shape that never existed, in a message being pasted to the agent
    /// as a description of what the human was looking at.
    #[test]
    fn engine_flag_export_names_the_rendered_hunk_total() {
        let repo = FixtureRepo::new("eng-flag-of").unwrap();
        let state = TempDir::new("lc-eng-flag-of");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", F1_TWO_HUNKS);
        let (_, row) = rendered_row(&mut engine, &root, b"f1");
        assert_eq!(row.hunks.len(), 2, "two hunks on screen");
        let rendered_total = row.hunks.iter().filter(|h| !h.is_mode_change()).count();

        // The agent rewrites the file between the render and the keystroke: one hunk now.
        repo.write("f1", "A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n");
        assert_eq!(
            engine.scan(&root).unwrap().row(b"f1").unwrap().hunks.len(),
            1,
            "the file really did move to one hunk"
        );

        let out = engine
            .flag(
                &root,
                b"f1",
                "why is this changed?",
                Some(RenderedHunk {
                    hunk: FlagHunk {
                        index: 1,
                        header: "@@ -9,2 +9,2 @@".into(),
                        text: "-a10\n+A10\n".into(),
                    },
                    of: rendered_total,
                }),
                None,
            )
            .unwrap();
        assert!(out.outcome.ok() && out.outcome.written);
        assert!(
            out.export.contains("· hunk 2 of 2 ·"),
            "the export names the rendered total, not the live one: {}",
            out.export.lines().next().unwrap_or_default()
        );
        assert!(
            !out.export.contains("of 1"),
            "and never a total the row never had"
        );
    }

    /// A refused flag (a non-UTF-8 path is not a ledger key) is data, not `Err`, and its
    /// export is empty — there is no flag to send.
    #[test]
    fn engine_flag_refused_has_no_export() {
        let repo = FixtureRepo::new("eng-flag-bad").unwrap();
        let state = TempDir::new("lc-eng-flag-bad");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let out = engine.flag(&root, b"bad\xff", "n", None, None).unwrap();
        assert!(matches!(
            out.outcome.refused[..],
            [crate::ops::Refused::NonUtf8Path { .. }]
        ));
        assert!(!out.outcome.written && out.export.is_empty());
    }

    #[test]
    fn engine_accept_hunk_leaves_the_other_hunk_pending_then_accept_file_clears_it() {
        let repo = FixtureRepo::new("eng-acc-hunk").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", F1_TWO_HUNKS);
        let (rendered, row) = rendered_row(&mut engine, &root, b"f1");
        assert_eq!(row.hunks.len(), 2);
        let acc = engine
            .accept(
                &root,
                AcceptRequest::Hunk {
                    rendered,
                    hunks: row.hunks.clone(),
                    index: 0,
                },
            )
            .unwrap();
        assert!(acc.outcome.ok() && acc.outcome.written);
        let left = acc.pile.row(b"f1").expect("still pending");
        assert_eq!(left.hunks.len(), 1, "hunk 2 only: {:?}", left.hunks);
        assert_eq!(left.hunks[0].change_lines(), row.hunks[1].change_lines());
        assert_eq!(
            engine.root(&root).unwrap().ledger.overrides["f1"].blob,
            Some(Some(
                engine
                    .root(&root)
                    .unwrap()
                    .store
                    .hash_bytes(b"A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n")
                    .unwrap()
            )),
            "the override is baseline plus hunk 1"
        );
        let (rendered, _) = rendered_row(&mut engine, &root, b"f1");
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert!(acc.outcome.ok());
        assert!(acc.pile.is_empty(), "{:?}", acc.pile.rows);
        assert_eq!(
            engine.scan(&root).unwrap(),
            acc.pile,
            "the returned pile is the rescan"
        );
    }

    // -----------------------------------------------------------------------------------
    // Several lastcall processes over one state dir (Phase 5 deliverable 2)
    // -----------------------------------------------------------------------------------

    /// Two engines over one root — two lastcall processes, as workspace scoping makes
    /// normal — accept a different file each, neither having seen the other's write. Both
    /// accepts must survive: `Ops::commit` re-reads the ledger under the lock and replays
    /// its staged change onto it, so the second write is a merge and not a clobber.
    #[test]
    fn engine_two_engines_over_one_root_keep_both_accepts() {
        let repo = FixtureRepo::new("eng-two").unwrap();
        let state = TempDir::new("lc-eng-two");
        repo.write("f1", "one edited\n");
        repo.write("f2", "two edited\n");
        let mut a = open_engine(&repo, &state, Config::default());
        let mut b = open_engine(&repo, &state, Config::default());
        let root = only_root(&a);
        assert_eq!(only_root(&b), root, "the same root, the same state dir");

        // Both scan first, so each holds the *pre-accept* ledger in memory: a commit that
        // wrote its own in-memory copy would drop whichever accept landed first.
        let (r1, _) = rendered_row(&mut a, &root, b"f1");
        let (r2, _) = rendered_row(&mut b, &root, b"f2");
        assert!(
            a.accept(&root, AcceptRequest::File(r1))
                .unwrap()
                .outcome
                .ok()
        );
        let acc = b.accept(&root, AcceptRequest::File(r2)).unwrap();
        assert!(acc.outcome.ok());

        // On disk, and after a third process opens the root cold.
        let c = open_engine(&repo, &state, Config::default());
        let overrides = &c.root(&root).unwrap().ledger.overrides;
        assert!(
            overrides.contains_key("f1"),
            "engine A's accept survived: {overrides:?}"
        );
        assert!(
            overrides.contains_key("f2"),
            "engine B's accept survived: {overrides:?}"
        );
        assert!(
            acc.pile.row(b"f1").is_none() && acc.pile.row(b"f2").is_none(),
            "neither is pending any more: {:?}",
            acc.pile.rows
        );
    }

    /// A lock the test holds makes an accept fail with `LockBusy` — an `Err` off the
    /// `Local::Accepted` path, never a `Refused` (a `Refused` carries a row path and renders
    /// per row; this is a whole-root condition and belongs on the status line). Short
    /// budget, so the test waits 20 ms rather than the shipping 2 s.
    #[test]
    fn engine_accept_under_a_held_ledger_lock_is_lock_busy_not_a_refusal() {
        let repo = FixtureRepo::new("eng-busy").unwrap();
        let state = TempDir::new("lc-eng-busy");
        repo.write("f1", "edited\n");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let (rendered, _) = rendered_row(&mut engine, &root, b"f1");
        let paths = engine.root(&root).unwrap().paths.clone();
        let held = ledger::LedgerLock::acquire(&paths).unwrap();

        let err = {
            let mut ops = engine.ops(&root).unwrap();
            ops.lock = (2, Duration::from_millis(10));
            ops.accept_file(&rendered, &NoFault).unwrap_err()
        };
        assert!(
            matches!(
                &err,
                OpsError::Ledger(ledger::LedgerError::LockBusy { retries: 2, path })
                    if *path == paths.lock
            ),
            "{err:?}"
        );
        // The shipping budget is the doubled one, and the accept path really uses it.
        assert_eq!(ledger::LOCK_RETRIES, 40);
        assert_eq!(crate::ops::DEFAULT_LOCK, (40, Duration::from_millis(50)));
        // What the engine hands the TUI is an `Err` carrying the lock path, and the TUI
        // turns it into the `ledger busy in <root> — try again` status line.
        let surfaced = EngineError::from(err);
        assert!(
            matches!(
                surfaced,
                EngineError::Ops(OpsError::Ledger(ledger::LedgerError::LockBusy { .. }))
            ),
            "{surfaced:?}"
        );
        assert_eq!(
            surfaced.to_string(),
            format!("could not take {} after 2 tries", paths.lock.display())
        );

        // Nothing reached the disk: the lock is taken before the merge and the tmp write.
        let on_disk =
            String::from_utf8_lossy(&std::fs::read(&paths.ledger).unwrap_or_default()).into_owned();
        assert!(
            !on_disk.contains("\"f1\""),
            "nothing written unlocked: {on_disk}"
        );
        // The engine's own copy *is* dirty — the op staged the override before it tried to
        // commit — and `Engine::accept_with` is what repairs it after an `Err`, by dropping
        // the stamp and re-reading the ledger the disk still has. Same two lines here.
        drop(held);
        let state = engine.roots.get_mut(&root).unwrap();
        state.ledger_stamp = None;
        state.reload_ledger_if_changed();
        assert!(
            state.ledger.overrides.is_empty(),
            "the staged accept is gone: {:?}",
            state.ledger.overrides
        );
        assert!(
            engine.scan(&root).unwrap().row(b"f1").is_some(),
            "still pending"
        );
    }

    /// Two engines scan one root at the same time. They share the persistent private index
    /// (`<store>/index`, `index.tree`) on purpose — deliverable 2d keeps it shared — so this
    /// is the test that the sharing holds: `PrivateIndex::refresh`'s retry absorbs git's own
    /// `index.lock` and the two piles agree. A scan is read-only about the ledger, so
    /// agreement is the whole assertion. Both engines live in this one process, so they
    /// share `index.<pid>.tmp` too (it is per process, not per engine); the fixture is
    /// adds only, so rename detection never touches the temp index here and the shared
    /// path is not contended — two *processes* are what production has, and each gets
    /// its own temp index by pid.
    #[test]
    fn engine_two_engines_scan_one_root_concurrently_and_agree() {
        let repo = FixtureRepo::new("eng-conc").unwrap();
        let state = TempDir::new("lc-eng-conc");
        for i in 0..12 {
            repo.write(&format!("c{i}.txt"), format!("edited {i}\nsecond\n"));
        }
        let mut a = open_engine(&repo, &state, Config::default());
        let mut b = open_engine(&repo, &state, Config::default());
        let root = only_root(&a);
        assert_ne!(
            a.root(&root).unwrap().paths.index_tmp,
            PathBuf::from("index.tmp"),
            "the temp index is per-process"
        );

        let (pa, pb) = std::thread::scope(|s| {
            let root_a = root.clone();
            let root_b = root.clone();
            let ha = s.spawn(move || {
                let mut out = Vec::new();
                for _ in 0..4 {
                    out.push(a.scan(&root_a).unwrap());
                }
                out
            });
            let hb = s.spawn(move || {
                let mut out = Vec::new();
                for _ in 0..4 {
                    out.push(b.scan(&root_b).unwrap());
                }
                out
            });
            (ha.join().unwrap(), hb.join().unwrap())
        });

        // Every scan on both sides saw the same 12 rows: no retry exhaustion, no half-index.
        for (i, pile) in pa.iter().chain(pb.iter()).enumerate() {
            assert_eq!(pile.rows.len(), 12, "scan {i}: {:?}", pile.rows);
            assert_eq!(
                pile.rows, pa[0].rows,
                "scan {i} disagrees with the first scan"
            );
        }
    }

    #[test]
    fn engine_accept_file_with_no_oid_accepts_the_deletion() {
        let repo = FixtureRepo::new("eng-acc-del").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.remove("f2");
        let (rendered, row) = rendered_row(&mut engine, &root, b"f2");
        assert_eq!((rendered.oid.as_ref(), row.change), (None, Change::Deleted));
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert!(acc.outcome.ok());
        assert!(acc.pile.is_empty());
        let o = &engine.root(&root).unwrap().ledger.overrides["f2"];
        assert_eq!(o.blob, Some(None), "an absent override (§6.2 null)");
    }

    #[test]
    fn engine_accept_group_clears_every_row_in_one_write() {
        let repo = FixtureRepo::new("eng-acc-group").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f2", "b2\n");
        repo.write("f3", "c3\n");
        repo.write("f1", "one\n");
        let pile = engine.scan(&root).unwrap();
        let rows: Vec<Rendered> = [b"f2".as_slice(), b"f3"]
            .iter()
            .map(|p| Rendered::of(pile.row(p).unwrap()))
            .collect();
        let before = engine.root(&root).unwrap().ledger.seen_at.at.clone();
        let acc = engine
            .accept(
                &root,
                AcceptRequest::Group {
                    rows,
                    rendered_on: pile.seen_branch.clone(),
                },
            )
            .unwrap();
        assert!(acc.outcome.ok() && acc.outcome.written);
        assert_eq!(scan::pile_lines(&acc.pile), vec!["f1".to_owned()]);
        let ledger = &engine.root(&root).unwrap().ledger;
        assert!(ledger.overrides.contains_key("f2") && ledger.overrides.contains_key("f3"));
        assert_eq!(ledger.seen_at.at, before, "a group is not a fold");
    }

    #[test]
    fn engine_accept_all_folds_the_snapshot_and_moves_seen_at() {
        let mut repo = FixtureRepo::new("eng-acc-all").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "one\n");
        repo.commit_files(&[("f2", "committed\n")], "B1").unwrap();
        let snapshot = engine.scan(&root).unwrap();
        assert_eq!(
            scan::pile_lines(&snapshot),
            vec!["f1".to_owned(), "f2".to_owned()]
        );
        let acc = engine.accept(&root, AcceptRequest::All(snapshot)).unwrap();
        assert!(acc.outcome.ok());
        assert!(acc.pile.is_empty(), "{:?}", acc.pile.rows);
        let ledger = &engine.root(&root).unwrap().ledger;
        assert!(
            ledger.overrides.is_empty(),
            "folded: {:?}",
            ledger.overrides
        );
        assert_eq!(
            ledger.seen_at.head_commit.as_ref().map(Oid::as_str),
            Some(repo.head().unwrap().trim()),
            "seen_at follows the fold"
        );
        let seen = ledger.seen_tree.clone().unwrap();
        let entries = engine.root(&root).unwrap().store.ls_tree(&seen).unwrap();
        let store = &engine.root(&root).unwrap().store;
        assert_eq!(
            entries[b"f1".as_slice()].1,
            store.hash_bytes(b"one\n").unwrap()
        );
        assert_eq!(
            entries[b"f2".as_slice()].1,
            store.hash_bytes(b"committed\n").unwrap()
        );
    }

    #[test]
    fn engine_accept_refusal_is_ok_and_the_pile_still_shows_the_row() {
        let repo = FixtureRepo::new("eng-acc-refuse").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "one\n");
        let (rendered, _) = rendered_row(&mut engine, &root, b"f1");
        // The file moved on between render and accept (§6.3 CAS).
        repo.write("f1", "two\n");
        let disk = ledger_bytes(&engine, &root);
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert_eq!(acc.outcome.refused.len(), 1, "{:?}", acc.outcome);
        assert!(!acc.outcome.written);
        let row = acc.pile.row(b"f1").expect("the row re-renders");
        let store = &engine.root(&root).unwrap().store;
        assert_eq!(
            row.current.as_ref().map(|e| e.oid.clone()),
            Some(store.hash_bytes(b"two\n").unwrap()),
            "with the live content"
        );
        assert_eq!(ledger_bytes(&engine, &root), disk, "nothing written");
    }

    /// E1 in-process: at `AfterLedgerTmpWrite` the temp file vanishes (a crash before the
    /// rename leaves the same on-disk state), so the rename fails.
    struct DropTmp(PathBuf);

    impl FaultInjector for DropTmp {
        fn at(&self, point: crate::ops::FaultPoint) {
            if point == crate::ops::FaultPoint::AfterLedgerTmpWrite {
                std::fs::remove_file(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn engine_accept_with_fault_at_ledger_tmp_write_is_err_and_the_next_scan_is_the_pre_accept_pile()
     {
        let repo = FixtureRepo::new("eng-acc-e1").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "one\n");
        repo.write("f2", "b2\n");
        let before = engine.scan(&root).unwrap();
        let rendered = Rendered::of(before.row(b"f1").unwrap());
        let disk = ledger_bytes(&engine, &root);
        let tmp = engine
            .root(&root)
            .unwrap()
            .paths
            .ledger
            .with_extension("json.tmp");
        let err = engine
            .accept_with(&root, AcceptRequest::File(rendered), &DropTmp(tmp))
            .expect_err("the rename fails");
        assert!(matches!(err, EngineError::Ops(_)), "{err}");
        assert_eq!(
            ledger_bytes(&engine, &root),
            disk,
            "on-disk ledger unchanged"
        );
        assert_eq!(
            engine.scan(&root).unwrap(),
            before,
            "the next scan's pile is the pre-accept pile"
        );
        assert!(
            !engine
                .root(&root)
                .unwrap()
                .ledger
                .overrides
                .contains_key("f1"),
            "the staged override did not survive the failure"
        );
    }

    fn cap3(repo: &FixtureRepo, state: &TempDir) -> Engine {
        let options = EngineOptions {
            row_cap: 3,
            ..EngineOptions::default()
        };
        open_engine_with(repo, state, Config::default(), options)
    }

    #[test]
    fn engine_row_cap_shows_the_first_cap_paths_counts_the_rest_and_accept_all_folds_the_shown() {
        let repo = FixtureRepo::new("eng-cap").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = cap3(&repo, &state);
        let root = only_root(&engine);
        for n in ["n1", "n2", "n3", "n4", "n5"] {
            repo.write(n, "new\n");
        }
        let pile = engine.scan(&root).unwrap();
        assert_eq!(scan::pile_lines(&pile), ["n1", "n2", "n3"]);
        assert_eq!(pile.omitted, 2);
        assert_eq!(
            pile.notices,
            vec!["3 files shown · 2 more changed paths not scanned (first 3 by path)".to_owned()]
        );
        let acc = engine.accept(&root, AcceptRequest::All(pile)).unwrap();
        assert!(acc.outcome.ok());
        assert_eq!(
            scan::pile_lines(&acc.pile),
            ["n4", "n5"],
            "the shown rows folded"
        );
        assert_eq!(acc.pile.omitted, 0);
        assert!(acc.pile.notices.is_empty(), "{:?}", acc.pile.notices);
    }

    #[test]
    fn engine_row_cap_override_row_is_priority_over_path_order() {
        let repo = FixtureRepo::new("eng-cap-prio").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = cap3(&repo, &state);
        let root = only_root(&engine);
        repo.write("zzz/late", "v1\n");
        let (rendered, _) = rendered_row(&mut engine, &root, b"zzz/late");
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert!(acc.outcome.ok() && acc.pile.is_empty());
        repo.write("zzz/late", "v2\n");
        for n in ["aaa/1", "aaa/2", "aaa/3", "aaa/4"] {
            repo.write(n, "new\n");
        }
        let pile = engine.scan(&root).unwrap();
        assert!(
            pile.row(b"zzz/late").is_some(),
            "{:?}",
            scan::pile_lines(&pile)
        );
        assert_eq!(
            scan::pile_lines(&pile),
            ["aaa/1", "aaa/2", "aaa/3", "zzz/late"],
            "priority beats path order"
        );
        assert_eq!(pile.omitted, 1);
        assert_eq!(
            pile.notices,
            vec!["4 files shown · 1 more changed paths not scanned (first 3 by path)".to_owned()]
        );
    }

    #[test]
    fn engine_scan_seq_is_monotone_across_scan_scan_all_inspect_head_and_accept() {
        let mut repo = FixtureRepo::new("eng-seq").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let s0 = engine.scan_seq();
        engine.scan(&root).unwrap();
        assert_eq!(engine.scan_seq(), s0 + 1, "one scan, one step");
        assert!(
            engine.scan(Path::new("/no/such/root")).is_err() && engine.scan_seq() == s0 + 1,
            "a scan that produced no pile takes no number"
        );
        let all = engine.scan_all();
        assert_eq!(all.len(), 1);
        let (_, seq, pile) = &all[0];
        assert!(pile.is_ok());
        assert_eq!(*seq, s0 + 2, "scan_all numbers each pile it returns");
        assert_eq!(
            engine.scan_seq(),
            *seq,
            "the engine's counter is the last pile's number"
        );
        repo.commit_files(&[("f1", "committed\n")], "B1").unwrap();
        let change = engine.inspect_head(&root).unwrap().expect("HEAD moved");
        assert_eq!(change.seq, s0 + 3, "inspect_head's scan is numbered too");
        assert_eq!(engine.scan_seq(), change.seq);
        let rendered = Rendered::of(change.pile.row(b"f1").unwrap());
        let acc = engine.accept(&root, AcceptRequest::File(rendered)).unwrap();
        assert_eq!(acc.seq, s0 + 4, "an accept's rescan is numbered too");
        assert_eq!(engine.scan_seq(), acc.seq);
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

    /// Phase 6 deliverable 4: `e` on a collapsed row asks the engine for its hunks. The
    /// scan still skips the second Myers pass; the expansion is computed from the row's
    /// own oids, on demand, and never lands on the row.
    #[test]
    fn engine_hunks_of_expands_a_collapsed_row_without_touching_the_pile() {
        let mut repo = FixtureRepo::new("eng-expand").unwrap();
        let before: String = (0..40)
            .map(|i| format!("  \"pkg-{i}\": \"1.0.0\",\n"))
            .collect();
        repo.commit_files(&[("package-lock.json", before.as_str())], "lock")
            .unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let after: String = (0..40)
            .map(|i| {
                if i == 7 {
                    "  \"pkg-7\": \"2.0.0\",\n".to_owned()
                } else {
                    format!("  \"pkg-{i}\": \"1.0.0\",\n")
                }
            })
            .collect();
        repo.write("package-lock.json", after);
        let pile = engine.scan(&root).unwrap();
        let row = pile.row(b"package-lock.json").unwrap().clone();
        assert_eq!(row.collapsed, Some(crate::scan::Collapsed::Glob));
        assert!(row.hunks.is_empty(), "the scan skips the hunk pass");

        let expanded = engine.hunks_of(&root, &row).unwrap();
        assert_eq!(expanded.omitted_lines, 0);
        assert_eq!(expanded.hunks.len(), 1);
        let inserted: Vec<String> = expanded.hunks[0]
            .lines
            .iter()
            .filter(|(t, _)| *t == crate::hunks::Tag::Insert)
            .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert_eq!(inserted, ["  \"pkg-7\": \"2.0.0\",\n"]);
        // The pile is untouched: accept stays whole-row for a collapsed path.
        assert!(
            engine
                .scan(&root)
                .unwrap()
                .row(b"package-lock.json")
                .unwrap()
                .hunks
                .is_empty()
        );
    }

    /// The cap is enforced where the UI reads it, not only in `hunks::truncate`.
    #[test]
    fn engine_hunks_of_caps_a_whole_file_rewrite() {
        let mut repo = FixtureRepo::new("eng-expand-cap").unwrap();
        let before: String = (0..3_000)
            .map(|i| format!("  \"pkg-{i}\": \"1.0.0\",\n"))
            .collect();
        repo.commit_files(&[("package-lock.json", before.as_str())], "lock")
            .unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let after: String = (0..3_000)
            .map(|i| format!("  \"pkg-{i}\": \"9.9.9\",\n"))
            .collect();
        repo.write("package-lock.json", after);
        let pile = engine.scan(&root).unwrap();
        let row = pile.row(b"package-lock.json").unwrap().clone();
        assert_eq!(row.collapsed, Some(crate::scan::Collapsed::Glob));

        let expanded = engine.hunks_of(&root, &row).unwrap();
        let shown: usize = expanded.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(shown, crate::hunks::EXPAND_LINE_CAP);
        assert!(
            expanded.omitted_lines > 0,
            "the footer has something to say"
        );
        assert_eq!(
            shown + expanded.omitted_lines,
            row.added + row.deleted,
            "every body line of this rewrite is a change line"
        );
    }

    /// Verifier (a) F4: `e` must never reach a binary row. The TUI's key is a no-op there,
    /// and the engine refuses too rather than diffing NUL bytes into a terminal.
    #[test]
    fn engine_hunks_of_refuses_a_binary_row() {
        let mut repo = FixtureRepo::new("eng-expand-binary").unwrap();
        repo.commit_files(&[("img.png", "placeholder\n")], "seed")
            .unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.resize(4_096, b'\x42');
        repo.write("img.png", &png);
        let pile = engine.scan(&root).unwrap();
        let row = pile.row(b"img.png").unwrap().clone();
        assert_eq!(row.collapsed, Some(crate::scan::Collapsed::Binary));
        let err = engine.hunks_of(&root, &row).unwrap_err();
        match err {
            EngineError::BinaryRow { path, .. } => assert_eq!(path, "img.png"),
            other => panic!("expected BinaryRow, got {other:?}"),
        }
    }

    /// Verifier (a) F5: a mode-only change on a **collapsed** row carries no content hunk —
    /// `render_content` synthesises the mode hunk only after the collapse early return — so
    /// without this the expansion would be an empty pane for the one thing that changed.
    #[test]
    fn engine_hunks_of_shows_a_mode_only_change_on_a_collapsed_row() {
        let mut repo = FixtureRepo::new("eng-expand-mode").unwrap();
        repo.commit_files(&[("package-lock.json", "{\"v\":1}\n")], "lock")
            .unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.git(&["update-index", "--chmod=+x", "package-lock.json"])
            .unwrap();
        repo.git(&["checkout-index", "-f", "package-lock.json"])
            .unwrap();
        let pile = engine.scan(&root).unwrap();
        let Some(row) = pile.row(b"package-lock.json") else {
            // A filesystem or a `core.filemode=false` clone that cannot carry the exec bit
            // has nothing to show; the rule under test is the one the scan uses.
            assert!(!engine.roots[&root].store.filemode(), "{pile:?}");
            return;
        };
        let row = row.clone();
        assert_eq!(row.collapsed, Some(crate::scan::Collapsed::Glob));
        assert!(row.hunks.is_empty(), "the scan writes no hunks here");
        let expanded = engine.hunks_of(&root, &row).unwrap();
        assert_eq!(expanded.hunks.len(), 1, "{expanded:?}");
        assert!(
            expanded.hunks[0].is_mode_change(),
            "the expansion says what changed: {:?}",
            expanded.hunks[0]
        );
    }

    /// Deliverable 8, F11: the inline editor's read decides binary-ness and the size cap
    /// **here**, because `collapse_size_bytes` is engine config the reducer cannot see. The
    /// three answers in one test, on three rows of one root: the text file opens, the PNG
    /// and the oversize file come back as `NotEditable` with the `why` the status prints.
    #[test]
    fn engine_read_rendered_opens_text_and_refuses_binary_and_oversize() {
        let mut repo = FixtureRepo::new("eng-read").unwrap();
        repo.commit_files(
            &[
                ("t.txt", "one\n"),
                ("img.png", "seed\n"),
                ("big.txt", "seed\n"),
            ],
            "seed",
        )
        .unwrap();
        let state = TempDir::new("lc-eng-state");
        let config = Config {
            collapse_size_bytes: 4 * 1024,
            ..Config::default()
        };
        let mut engine = open_engine(&repo, &state, config);
        let root = only_root(&engine);
        repo.write("t.txt", "one\ntwo\n");
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.resize(1_024, b'\x42');
        repo.write("img.png", &png);
        repo.write("big.txt", "x".repeat(5_000));
        let pile = engine.scan(&root).unwrap();
        let of = |name: &[u8]| Rendered::of(pile.row(name).unwrap());

        assert_eq!(
            engine.read_rendered(&root, &of(b"t.txt")).unwrap(),
            b"one\ntwo\n",
            "the live bytes, verbatim"
        );
        match engine.read_rendered(&root, &of(b"img.png")) {
            Err(Refused::NotEditable { why, .. }) => assert_eq!(why, "binary"),
            other => panic!("{other:?}"),
        }
        match engine.read_rendered(&root, &of(b"big.txt")) {
            Err(Refused::NotEditable { why, .. }) => assert_eq!(why, "over 4 KiB"),
            other => panic!("{other:?}"),
        }
    }

    /// The CAS and the re-hash: a write between the render and the read is a `Moved`, and
    /// the editor never opens on content the row did not describe.
    #[test]
    fn engine_read_rendered_refuses_a_file_that_moved_since_it_was_rendered() {
        let repo = FixtureRepo::new("eng-read-moved").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write("f1", "first\n");
        let pile = engine.scan(&root).unwrap();
        let rendered = Rendered::of(pile.row(b"f1").unwrap());
        repo.write("f1", "an agent got here first\n");
        match engine.read_rendered(&root, &rendered) {
            Err(Refused::Moved { path, .. }) => assert_eq!(path, b"f1"),
            other => panic!("{other:?}"),
        }
    }

    /// An unknown root is an error, not a panic or an empty expansion.
    #[test]
    fn engine_hunks_of_on_an_unknown_root_is_no_such_root() {
        let repo = FixtureRepo::new("eng-expand-noroot").unwrap();
        let state = TempDir::new("lc-eng-state");
        let engine = open_engine(&repo, &state, Config::default());
        let row = Row {
            path: b"x".to_vec(),
            change: Change::Modified,
            baseline: None,
            current: None,
            added: 0,
            deleted: 0,
            hunks: vec![],
            annotation: None,
            conflicted: false,
            collapsed: Some(crate::scan::Collapsed::Glob),
            flags: Vec::new(),
            rename: None,
        };
        let err = engine
            .hunks_of(Path::new("/nope/not/a/root"), &row)
            .unwrap_err();
        assert!(matches!(err, EngineError::NoSuchRoot(_)), "{err:?}");
    }

    /// Verifier (a) F1: a wanted oid the store cannot produce is an error on **either**
    /// side, never an empty one. An empty current side would draw the live file as a whole
    /// deletion — hiding what `e` was pressed to read — and an empty baseline would claim
    /// the whole file is new; both are worse than saying so.
    #[test]
    fn engine_hunks_of_refuses_a_row_whose_blob_the_store_lost() {
        let mut repo = FixtureRepo::new("eng-expand-lost").unwrap();
        let before: String = (0..40)
            .map(|i| format!("  \"pkg-{i}\": \"1.0.0\",\n"))
            .collect();
        repo.commit_files(&[("package-lock.json", before.as_str())], "lock")
            .unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        repo.write(
            "package-lock.json",
            format!("{before}  \"pkg-40\": \"1.0.0\",\n"),
        );
        let pile = engine.scan(&root).unwrap();
        let row = pile.row(b"package-lock.json").unwrap().clone();
        assert_eq!(row.collapsed, Some(crate::scan::Collapsed::Glob));
        // The real row expands.
        assert!(!engine.hunks_of(&root, &row).unwrap().hunks.is_empty());

        let ghost = Oid::parse(&"f".repeat(40)).expect("a well-formed but absent oid");
        let with = |side: fn(&mut Row, crate::scan::Entry)| {
            let mut row = row.clone();
            let mode = row.current.as_ref().expect("a modified row").mode;
            side(
                &mut row,
                crate::scan::Entry {
                    oid: ghost.clone(),
                    mode,
                },
            );
            engine.hunks_of(&root, &row).unwrap_err()
        };
        for err in [
            with(|r, e| r.baseline = Some(e)),
            with(|r, e| r.current = Some(e)),
        ] {
            match err {
                EngineError::MissingBlob { oid, .. } => assert_eq!(oid, ghost.as_str()),
                other => panic!("expected MissingBlob, got {other:?}"),
            }
        }
    }

    /// Phase 6 deliverable 5 (§11 "`for-each-ref refs/remotes` twice per scan"): the
    /// listing that builds the memo key is the listing the classification is computed
    /// with, so a scan lists the remote refs exactly once whether or not it recomputes.
    ///
    /// Verifier (a) F2: the **recompute** path is the one §11 was about (before this
    /// deliverable `get` listed once for the key and `classify` listed again for the same
    /// key), so the last third moves the key the way
    /// `engine_upstream_labels_follow_remote_refs_without_head_moving` does — a coworker
    /// push plus a fetch, HEAD unmoved — and counts that `get`.
    #[test]
    fn engine_classification_lists_remote_refs_once_per_scan() {
        let mut repo = FixtureRepo::new("eng-refs-once").unwrap();
        let state = TempDir::new("lc-eng-state");
        let engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let rs = engine.root(&root).unwrap();
        let rg = rs.repo.as_ref().expect("a git root has a repo runner");
        let live = headstate::inspect(rg).unwrap();

        // `classify` no longer runs a listing of its own: with the caller's listing in
        // hand, the no-seen-head early return spawns nothing at all (it cost one
        // `for-each-ref` before this deliverable), and the key records what it was given.
        let before = crate::git::thread_spawn_count();
        let class = upstream::classify(rg, None, &live, None, "given".to_owned()).unwrap();
        assert_eq!(
            crate::git::thread_spawn_count() - before,
            0,
            "classify runs no git process before its early return"
        );
        assert_eq!(class.key.remotes, "given");

        // And the memoized path costs exactly the one listing that builds the key.
        let mut classifier = Classifier::default();
        let seen_head = rs.seen_head().cloned();
        assert!(
            seen_head.is_some(),
            "first sight recorded a seen head, so `classify` does real work below"
        );
        let first = classifier
            .get(rg, seen_head.as_ref(), &live, None)
            .expect("first classification")
            .key
            .remotes
            .clone();
        let before = crate::git::thread_spawn_count();
        classifier
            .get(rg, seen_head.as_ref(), &live, None)
            .expect("cached classification");
        assert_eq!(
            crate::git::thread_spawn_count() - before,
            1,
            "one `for-each-ref refs/remotes` per scan, and nothing else when cached"
        );

        // The key moves without HEAD moving: `origin/main` catches up under us.
        repo.coworker_push(1).unwrap();
        repo.git(&["fetch", "-q", "origin"]).unwrap();
        assert_eq!(
            headstate::inspect(rg).unwrap(),
            live,
            "the fetch moved refs/remotes, not HEAD"
        );
        // Counted by **argv**, not by arithmetic on a total: two listings and one listing
        // differ by a spawn either way, so only the subcommand names tell them apart.
        let (moved, argv) = crate::git::recording_argv(|| {
            classifier
                .get(rg, seen_head.as_ref(), &live, None)
                .expect("recomputed classification")
                .key
                .remotes
                .clone()
        });
        assert_ne!(moved, first, "the listing moved, so the memo missed");
        let listings = argv
            .iter()
            .filter(|a| {
                a.iter().any(|w| w == "for-each-ref") && a.iter().any(|w| w == "refs/remotes")
            })
            .count();
        assert_eq!(
            listings, 1,
            "one listing on the recompute path; argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.iter().any(|w| w == "rev-list")),
            "the recompute really ran classify's own work; argv: {argv:?}"
        );
    }

    /// R1/R6: a global `core.fsmonitor = true` (file or env-injected) must neither hang the
    /// scan (`update-index --refresh` under our GIT_DIR waited on a daemon forever) nor
    /// start an `fsmonitor--daemon` anywhere.
    #[test]
    fn engine_scan_returns_under_a_global_fsmonitor_config() {
        let repo = FixtureRepo::new("eng-fsmon").unwrap();
        let state = TempDir::new("lc-eng-state");
        let global = state.path().join("gitconfig");
        std::fs::write(
            &global,
            "[core]\n\tfsmonitor = true\n\tuntrackedCache = true\n",
        )
        .unwrap();
        let env = fixture_env(&repo, &state)
            .with_var("GIT_CONFIG_GLOBAL", global.to_string_lossy().into_owned())
            .with_var("GIT_CONFIG_PARAMETERS", "'core.fsmonitor=true'");
        let (loaded, resolved) = loaded_for(&repo, &state, Config::default());
        repo.write("f1", "edited\n");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_env = env.clone();
        std::thread::spawn(move || {
            let mut engine =
                Engine::open(&loaded, &resolved, &worker_env, EngineOptions::default()).unwrap();
            let root = engine.root_paths()[0].clone();
            let store = engine.root(&root).unwrap().paths.store.clone();
            let pile = engine.scan(&root).map(|p| scan::pile_lines(&p));
            let _ = tx.send((pile, store));
        });
        let (pile, store) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the scan returned within 5 s (an fsmonitor hang otherwise)");
        assert_eq!(pile.unwrap(), ["f1"]);
        // No daemon for the user's repository nor for the store.
        for (dir, git_dir) in [(repo.path(), None), (repo.path(), Some(store.as_path()))] {
            let mut cmd = crate::git::base_command(&env, dir);
            if let Some(g) = git_dir {
                cmd.env("GIT_DIR", g);
            }
            let status = cmd.args(["fsmonitor--daemon", "status"]).output().unwrap();
            if status.status.success() {
                let _ = crate::git::base_command(&env, dir)
                    .args(["fsmonitor--daemon", "stop"])
                    .output();
                panic!("an fsmonitor daemon was started for {dir:?} (git_dir {git_dir:?})");
            }
        }
    }

    /// R2: a long-running engine adopts an accept made by another process. A and B open
    /// the same root; A accepts f1@v1; the user restores the committed content; B must
    /// show f1 pending (against A's baseline) exactly as a fresh engine does.
    #[test]
    fn engine_scan_reloads_a_ledger_written_by_another_engine() {
        let repo = FixtureRepo::new("eng-two").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut a = open_engine(&repo, &state, Config::default());
        let mut b = open_engine(&repo, &state, Config::default());
        let root = only_root(&a);
        let committed = std::fs::read(repo.path().join("f1")).unwrap();
        repo.write("f1", "v1\n");
        assert_eq!(scan::pile_lines(&b.scan(&root).unwrap()), ["f1"]);
        let pile = a.scan(&root).unwrap();
        let rendered = Rendered::of(pile.row(b"f1").unwrap());
        assert!(
            a.ops(&root)
                .unwrap()
                .accept_file(&rendered, &NoFault)
                .unwrap()
                .ok()
        );
        assert!(a.scan(&root).unwrap().is_empty());
        repo.write("f1", committed);
        let mut fresh = open_engine(&repo, &state, Config::default());
        assert_eq!(scan::pile_lines(&fresh.scan(&root).unwrap()), ["f1"]);
        assert_eq!(
            scan::pile_lines(&b.scan(&root).unwrap()),
            ["f1"],
            "B adopted A's accept instead of its stale in-memory ledger"
        );
    }

    /// R5: discovery re-runs when a scan first sees a nested repo, not on every
    /// `scan_all` while one exists.
    /// The progress hook fires once per root, as each scan returns, with that root's row
    /// count — including a nested root discovered on the way, which the retry round scans.
    #[test]
    fn engine_scan_all_with_reports_every_root_as_it_finishes() {
        let repo = FixtureRepo::new("eng-progress").unwrap();
        let state = TempDir::new("lc-eng-state");
        let nested = repo.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        repo.git_at(&nested, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(nested.join("n"), "n\n").unwrap();
        repo.write("f1", "pending\n");
        let mut engine = open_engine(&repo, &state, Config::default());
        let seen = std::sync::Mutex::new(Vec::<(PathBuf, usize)>::new());
        let results = engine
            .scan_all_with(&|root, rows| seen.lock().unwrap().push((root.to_path_buf(), rows)));
        let seen = seen.into_inner().unwrap();
        assert_eq!(results.len(), 2, "the nested repo became a root");
        assert_eq!(seen.len(), results.len(), "one report per root: {seen:?}");
        for (root, _seq, result) in &results {
            let rows = result.as_ref().map(|p| p.rows.len()).unwrap_or(0);
            assert!(
                seen.iter().filter(|(r, n)| r == root && *n == rows).count() == 1,
                "{root:?} reported with {rows} rows exactly once: {seen:?}"
            );
        }
        assert!(
            seen.iter().any(|(_, n)| *n > 0),
            "the pending file is counted: {seen:?}"
        );
    }

    #[test]
    fn engine_scan_all_rediscovers_only_when_nested_repos_change() {
        let repo = FixtureRepo::new("eng-nested").unwrap();
        let state = TempDir::new("lc-eng-state");
        let nested = repo.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        repo.git_at(&nested, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(nested.join("n"), "n\n").unwrap();
        let mut engine = open_engine(&repo, &state, Config::default());
        assert_eq!(engine.roots().len(), 1);
        let runs0 = engine.discovery_runs();
        let first = engine.scan_all();
        assert_eq!(engine.roots().len(), 2, "the nested repo became a root");
        assert_eq!(first.len(), 2);
        let runs1 = engine.discovery_runs();
        assert!(runs1 > runs0);
        let second = engine.scan_all();
        assert_eq!(second.len(), 2);
        assert_eq!(
            engine.discovery_runs(),
            runs1,
            "no rediscovery while the nested set is unchanged"
        );
    }

    /// Deliverable 8: the depth the tour's card applies is a session setting the next
    /// discovery pass reads. The repository two folders down is invisible at the default
    /// depth, appears after `set_search_depth(2)` and one `rescan`, and the ledger of the
    /// root that was already open is not touched on the way.
    #[test]
    fn engine_set_search_depth_adds_the_deeper_roots_on_the_next_rescan() {
        let repo = FixtureRepo::new("eng-depth").unwrap();
        let state = TempDir::new("lc-eng-state");
        // A plain folder beside the fixture repo, with a repository inside it: level 2.
        let deep = repo.parent_dir().join("worktrees").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        repo.git_at(&deep, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(deep.join("n"), "n\n").unwrap();

        repo.write("f1", "one\n");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        assert_eq!(engine.search_depth(), 1, "the default is level 1 alone");
        // Give the open root a ledger worth comparing: a fold, not a fresh file.
        let snapshot = engine.scan(&root).unwrap();
        assert_eq!(scan::pile_lines(&snapshot), vec!["f1".to_owned()]);
        assert!(
            engine
                .accept(&root, AcceptRequest::All(snapshot))
                .unwrap()
                .outcome
                .ok()
        );
        let before = engine.root(&root).unwrap().ledger.clone();

        let runs0 = engine.discovery_runs();
        engine.set_search_depth(2);
        assert_eq!(engine.search_depth(), 2);
        assert_eq!(
            engine.root_paths(),
            vec![root.clone()],
            "setting the number alone discovers nothing"
        );
        let changed = engine.rescan().unwrap();
        assert_eq!(
            engine.discovery_runs(),
            runs0 + 1,
            "one rescan is one discovery run"
        );
        let canon_deep = std::fs::canonicalize(&deep).unwrap();
        assert_eq!(changed.added, vec![canon_deep.clone()]);
        assert!(changed.removed.is_empty(), "{:?}", changed.removed);
        let mut want = vec![root.clone(), canon_deep];
        want.sort();
        assert_eq!(
            engine.root_paths(),
            want,
            "the level-2 repository is a root"
        );
        assert_eq!(
            engine.root(&root).unwrap().ledger,
            before,
            "the open root's ledger is untouched by a rescan"
        );

        // And it is a number, not a ratchet: back to 1 and the deeper root is gone again.
        engine.set_search_depth(1);
        engine.rescan().unwrap();
        assert_eq!(engine.root_paths(), vec![root]);
    }

    /// Out-of-range depths clamp rather than panic: the binary's callers are keystrokes.
    #[test]
    fn engine_set_search_depth_clamps_out_of_range_values() {
        let repo = FixtureRepo::new("eng-depth-clamp").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        engine.set_search_depth(0);
        assert_eq!(engine.search_depth(), 1);
        engine.set_search_depth(200);
        assert_eq!(engine.search_depth(), crate::config::MAX_SEARCH_DEPTH);
    }

    /// R7: `--root` outside every root (or nonexistent) is an error, not an empty report.
    #[test]
    fn status_root_outside_any_root_is_an_error() {
        let repo = FixtureRepo::new("eng-root").unwrap();
        let state = TempDir::new("lc-eng-state");
        let mut engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let bad = vec![PathBuf::from("/nonexistent/lastcall/root")];
        let err = StatusReport::build(&mut engine, Some(&bad)).unwrap_err();
        assert!(err.to_string().contains("not a watched root"), "{err}");
        let outside = vec![state.path().to_path_buf()];
        assert!(StatusReport::build(&mut engine, Some(&outside)).is_err());
        let good = vec![root.join("f1"), root.clone()];
        let report = StatusReport::build(&mut engine, Some(&good)).unwrap();
        assert_eq!(report.roots.len(), 1, "one report per selected root");
    }

    /// R8: a ledger whose recorded root is another path is another root's state and is
    /// handled like an unreadable ledger (moved aside, nothing seen, a notice).
    #[test]
    fn engine_open_treats_a_ledger_for_another_root_as_unreadable() {
        let repo = FixtureRepo::new("eng-otherroot").unwrap();
        let state = TempDir::new("lc-eng-state");
        let engine = open_engine(&repo, &state, Config::default());
        let root = only_root(&engine);
        let paths = engine.root(&root).unwrap().paths.clone();
        drop(engine);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.ledger).unwrap()).unwrap();
        json["root"] = serde_json::Value::String("/somewhere/else".to_owned());
        std::fs::write(&paths.ledger, serde_json::to_vec(&json).unwrap()).unwrap();
        let mut engine = open_engine(&repo, &state, Config::default());
        let r = engine.root(&root).unwrap();
        assert!(r.ledger.seen_tree.is_none(), "{:?}", r.notices);
        assert!(
            r.notices
                .iter()
                .any(|n| n.contains("recorded root /somewhere/else")),
            "{:?}",
            r.notices
        );
        assert!(ledger::moved_aside_sibling(&paths).is_some());
        assert_eq!(r.ledger.root, root.to_string_lossy());
        assert!(engine.scan(&root).unwrap().rows.len() >= 3);
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

    /// Amendment v1.13 R6 (verifier F2): the trim's trigger has to look at the overrides
    /// too. A single-file accept of a path the seen tree never held leaves the record
    /// holding it in an override alone, and narrowing the scope must still drop it on the
    /// very next scan rather than waiting for some later fold to surprise the reader.
    #[test]
    fn engine_scope_trim_fires_for_a_blob_override_outside_the_scope() {
        let repo = FixtureRepo::new("eng-trim-over").unwrap();
        let state = TempDir::new("lc-eng-state");
        let drafts = repo.parent_dir().join("_drafts");
        std::fs::create_dir_all(&drafts).unwrap();
        std::fs::write(drafts.join("n1.md"), "one\n").unwrap();
        let wide = Config {
            draft_dirs: vec!["_drafts/**".to_owned()],
            draft_initial: DraftInitial::Seen,
            ..Config::default()
        };
        let narrow = Config {
            draft_dirs: vec!["_drafts".to_owned()],
            ..wide.clone()
        };
        let mut engine = open_engine(&repo, &state, wide);
        let draft = engine
            .roots()
            .iter()
            .find(|r| r.kind == RootKind::Draft)
            .map(|r| r.path.clone())
            .expect("a draft root");
        assert!(engine.scan(&draft).unwrap().is_empty());
        std::fs::create_dir_all(drafts.join("sub")).unwrap();
        std::fs::write(drafts.join("sub/new.md"), "n\n").unwrap();
        let pile = engine.scan(&draft).unwrap();
        let rendered = Rendered::of(pile.row(b"sub/new.md").expect("the deep row"));
        assert!(
            engine
                .ops(&draft)
                .unwrap()
                .accept_file(&rendered, &NoFault)
                .unwrap()
                .refused
                .is_empty()
        );
        {
            // The premise: the accept wrote an override, and the seen tree itself still
            // knows nothing about the deep path.
            let r = engine.root(&draft).unwrap();
            assert!(!r.tree.contains_key(b"sub/new.md".as_slice()));
            assert!(matches!(
                r.ledger.overrides.get("sub/new.md").map(|o| &o.blob),
                Some(Some(Some(_)))
            ));
        }
        drop(engine);

        let mut engine = open_engine(&repo, &state, narrow);
        let pile = engine.scan(&draft).unwrap();
        assert!(
            pile.notices
                .iter()
                .any(|n| n == "1 path outside the root's scope dropped from its record"),
            "{:?}",
            pile.notices
        );
        assert!(
            !engine
                .root(&draft)
                .unwrap()
                .ledger
                .overrides
                .contains_key("sub/new.md"),
            "the out-of-scope override is gone"
        );
        // And no later fold has a surprise left to deliver.
        assert!(engine.scan(&draft).unwrap().notices.is_empty());
    }

    #[test]
    fn engine_remote_slug_on_every_url_form() {
        for (url, want) in [
            ("git@github.com:acme/alpha.git", Some("acme/alpha")),
            ("git@github.com:acme/alpha", Some("acme/alpha")),
            ("ssh://git@github.com/acme/alpha.git", Some("acme/alpha")),
            ("ssh://git@github.com:2222/acme/alpha", Some("acme/alpha")),
            ("https://github.com/acme/alpha.git", Some("acme/alpha")),
            ("https://github.com/acme/alpha/", Some("acme/alpha")),
            (
                "http://gitlab.example.com/group/sub/alpha.git",
                Some("sub/alpha"),
            ),
            ("github.com:acme/alpha", Some("acme/alpha")),
            ("  git@github.com:acme/alpha.git\n", Some("acme/alpha")),
            // Local paths and file URLs: no remote label, ever.
            ("/tmp/lc-w-123/alpha.git", None),
            ("/tmp/lc-w-123/alpha", None),
            ("./alpha.git", None),
            ("../alpha.git", None),
            ("~/repos/alpha.git", None),
            ("file:///tmp/lc-w-123/alpha.git", None),
            ("FILE:///tmp/alpha.git", None),
            ("/tmp/with:colon/alpha.git", None),
            // Unparsable.
            ("", None),
            ("alpha", None),
            ("https://github.com/", None),
            ("https://github.com/alpha.git", None),
            ("git@github.com:alpha.git", None),
            (":alpha/beta", None),
        ] {
            assert_eq!(remote_slug(url).as_deref(), want, "{url:?}");
        }
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
