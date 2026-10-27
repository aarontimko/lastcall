//! Candidates → pending rows (kickoff deliverable 5; docs/spec/00-spec.md §6.2, §6.4
//! "Path enumeration"/"Content model"/"Symlinks"/"Deletions and renames", §6.5).
//!
//! A port of the harness's `lc_candidates` + `lc_pending_p` + `lc_pile`:
//!
//! 1. candidates = `diff_files` paths ∪ `others` files ∪ every override path ∪ (on
//!    case-insensitive roots) seen-tree entries whose exact name is absent from their
//!    directory, minus paths tagged `S` in the **user's** index *and absent from the
//!    worktree* (D6: a sparse cone, never a present file), minus `others`
//!    entries under a nested repository (D9) or a draft root owned by another root. Paths
//!    that come from `diff_files`, an override, or the case rule are **never** excluded.
//! 2. `current = lstat` → `Absent` | `Unhashable(reason)` | `{oid, mode}`, hashed in one
//!    `hash-object -w --stdin-paths` call; `Unhashable` is always a row, never a skip.
//! 3. `baseline` per §6.2 (ledger.rs); a row is pending iff baseline ≠ current by oid or
//!    mode (D1).
//! 4. hunks (hunks.rs), collapse (D7/D8), conflict state (the user's `ls-files -u`), the
//!    override's flag, rename pairing (D5) — presentation, the ledger stores delete + add.
//!
//! **HEAD is never consulted here.** Annotation (upstream.rs) is layered on afterwards.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::GlobSet;

use crate::count::with_thousands;
use crate::git::{self, GitError, Mode, Oid, RepoGit};
use crate::hunks::{self, Hunk};
use crate::index::{IndexError, Other, PrivateIndex};
use crate::ledger::{Baseline, BaselineResolver, Flag, Ledger, TreeEntries};
use crate::store::{Current, DraftScope, ExcludedDir, MetaOf, Store, StoreError, under_excluded};

/// Git's binary heuristic: a NUL within the first 8000 bytes.
const BINARY_PROBE: usize = 8000;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The private index was reseeded by another process between this scan's reads of it
    /// (Phase 13 deliverable B, item 5). A reseed between `diff-files` and `ls-files
    /// --others` can drop a path that is pending under both trees, and a one-shot
    /// `lastcall status` has no next scan to correct it, so the scan says so instead.
    /// The engine loops [`crate::engine`]'s `scan_root`; on exhaustion the pile comes back
    /// with a notice, never as an error.
    #[error("the private index was reseeded during this scan")]
    IndexMoved,
}

/// An (oid, mode) pair as rendered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub oid: Oid,
    pub mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Mode,
    Typechange,
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Annotation {
    Upstream,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Collapsed {
    Glob,
    Binary,
    Size,
    /// A file in a watched folder that is at or above the size limit and is already in the
    /// folder's record: it keeps its row, and the row says the content was not read. The
    /// limit travels with the row so every surface prints the number the config set.
    Unread {
        over_bytes: u64,
    },
}

/// A size limit as a reader's line: whole KiB where it divides, bytes otherwise, and
/// `1 byte` rather than `1 bytes` at the one size where the plural is wrong (verifier F7).
pub fn size_limit_label(bytes: u64) -> String {
    if bytes.is_multiple_of(1024) {
        format!("{} KiB", with_thousands((bytes / 1024) as usize))
    } else if bytes == 1 {
        "1 byte".to_owned()
    } else {
        format!("{} bytes", with_thousands(bytes as usize))
    }
}

/// D5 pairing: the added row carries `From`, the deleted row carries `To`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rename {
    From { from: Vec<u8>, similarity: u8 },
    To { to: Vec<u8>, similarity: u8 },
}

/// One pending row. Serde is for recorded-pile fixtures (the TUI's unit tests), not a wire
/// format: `status --json` has its own stable schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub path: Vec<u8>,
    pub change: Change,
    pub baseline: Option<Entry>,
    pub current: Option<Entry>,
    pub added: usize,
    pub deleted: usize,
    /// Shared, read-only once rendered (Phase 17 Part B, fix 3): a row whose sides have not
    /// moved since the last scan takes that scan's hunks rather than a fresh diff, and an
    /// accept-by-hunk request carries them without a copy. Built as a `Vec`, wrapped once.
    pub hunks: Arc<Vec<Hunk>>,
    pub annotation: Option<Annotation>,
    pub conflicted: bool,
    pub collapsed: Option<Collapsed>,
    /// Every flag on the path, oldest first (Amendment v1.7). A file flag has `hunk: None`.
    /// `#[serde(default)]` so recorded-pile fixtures written before v1.7 still load.
    #[serde(default)]
    pub flags: Vec<Flag>,
    pub rename: Option<Rename>,
    /// The branches whose parked record already accepted exactly this content (Phase 14
    /// B, Amendment v1.15), sorted; empty when none. Set by `seen.rs` after the scan,
    /// never by it. `#[serde(default)]` so recorded-pile fixtures written before it load.
    #[serde(default)]
    pub seen_on: Vec<String>,
}

impl Row {
    pub fn path_lossy(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }

    /// Whether this row folds into the `[seen] N files` group (Phase 14 B): its content
    /// was accepted on another branch, it is not an upstream or mixed row, and it carries
    /// nothing of the user's own: no flag, so no note. A pending deletion is always a row.
    /// A flagged row stays a row (with a `[seen]` badge), and so does either half of a
    /// rename pair, so the deleted half never points at a path folded out of sight and the
    /// pair is accepted together. An accept-only override is **not** a reason to stay:
    /// compaction folds those into the seen tree, and whether a row folds must not change
    /// when the record is compacted (the pile is identical across a compaction).
    pub fn folds_seen(&self) -> bool {
        !self.seen_on.is_empty()
            && self.current.is_some()
            && self.annotation.is_none()
            && self.flags.is_empty()
            && self.rename.is_none()
    }
}

/// What a derived group row gathers. `Annotation` stays the row's own label, so a row is
/// never annotated `Seen`; the seen group is built from [`Row::seen_on`] instead.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum GroupKind {
    Upstream,
    Mixed,
    Seen,
}

impl GroupKind {
    pub fn name(self) -> &'static str {
        match self {
            GroupKind::Upstream => "upstream",
            GroupKind::Mixed => "mixed",
            GroupKind::Seen => "seen",
        }
    }
}

/// A derived group row (§6.7 `[upstream] N files`, Phase 14 B `[seen] N files`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub kind: GroupKind,
    pub paths: Vec<Vec<u8>>,
}

/// The pending set of one root after a scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Pile {
    /// Sorted by path bytes. Shared, read-only once built (Phase 17 Part B, fix 1): the
    /// engine's `last_pile`, the screen's view and an accept's request all hold this one
    /// list, so cloning a pile copies the small fields below and not a single row. Rows are
    /// built and sorted as a `Vec` and wrapped once; see [`Pile::rows_mut`].
    pub rows: Arc<Vec<Row>>,
    /// Root-level notices produced by this scan.
    pub notices: Vec<String>,
    /// Changed paths the row cap left unscanned (`0` when nothing was cut). Additive:
    /// older JSON without it reads as `0`.
    #[serde(default)]
    pub omitted: usize,
    /// How deep this root's undo stack is, stamped by the engine after every scan
    /// (Amendment v1.11). The reducer never reads a ledger, so everything the UI knows
    /// about undo rides here: the hint, the status suffix, and a second process's undo.
    /// Additive: older JSON without it reads as `0`.
    #[serde(default)]
    pub undo: usize,
    /// When this root stops being snoozed, `None` when it is not (an **expired** deadline
    /// is already `None` here: the engine applies its own injected clock at scan time, so
    /// the TUI never compares wall clocks of its own; design review F4).
    #[serde(default)]
    pub snoozed_until: Option<String>,
    /// The branch whose record this pile was computed against (the ledger's `seen_branch`,
    /// Amendment v1.12). An accept-all or a group accept carries the snapshot the user
    /// saw, and R5 is about exactly that: a snapshot rendered under one branch must not be
    /// written into another's record, even after the `Ops` has adopted it. `None` for a
    /// draft root, for a record that belongs to no branch, and for a pile built by hand in
    /// a test. Additive: older JSON without it reads as `None`.
    #[serde(default)]
    pub seen_branch: Option<String>,
}

impl Pile {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows, writable: copy-on-write, so free on a pile nobody else holds yet (the
    /// scan's own, while the engine annotates and marks it, before it is handed out) and a
    /// whole-list copy on one that is shared. Nothing outside tests calls it on a pile the
    /// engine or a view already holds.
    pub fn rows_mut(&mut self) -> &mut Vec<Row> {
        Arc::make_mut(&mut self.rows)
    }

    pub fn row(&self, path: &[u8]) -> Option<&Row> {
        self.rows.iter().find(|r| r.path == path)
    }

    /// The derived groups, in nav order: the rows tagged `upstream`, then the rows that
    /// fold into `seen` ([`Row::folds_seen`]). A group with no rows is not listed.
    pub fn groups(&self) -> Vec<Group> {
        let of = |kind: GroupKind, keep: &dyn Fn(&Row) -> bool| {
            let paths: Vec<Vec<u8>> = self
                .rows
                .iter()
                .filter(|r| keep(r))
                .map(|r| r.path.clone())
                .collect();
            (!paths.is_empty()).then_some(Group { kind, paths })
        };
        [
            of(GroupKind::Upstream, &|r: &Row| {
                r.annotation == Some(Annotation::Upstream)
            }),
            of(GroupKind::Seen, &Row::folds_seen),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The paths folded into the seen group, if any.
    pub fn seen_group(&self) -> Option<Group> {
        self.groups()
            .into_iter()
            .find(|g| g.kind == GroupKind::Seen)
    }
}

/// Everything one scan needs; the engine owns the long-lived pieces.
pub struct ScanInputs<'a> {
    pub store: &'a Store,
    pub index: &'a PrivateIndex,
    /// `Some` for git roots.
    pub repo: Option<&'a RepoGit>,
    pub ledger: &'a Ledger,
    /// The effective seen tree (already `None` when the ledger's tree is missing).
    pub seen_tree: Option<&'a Oid>,
    /// `ls-tree -r` of `seen_tree` (empty when `None`).
    pub tree: &'a TreeEntries,
    pub case_insensitive: bool,
    pub collapsed_globs: &'a GlobSet,
    pub collapse_size_bytes: u64,
    /// Rows materialised per scan beyond the priority set (override paths, where flags
    /// live): the rest of the candidates, in path order, are neither hashed nor diffed,
    /// only counted in [`Pile::omitted`].
    pub row_cap: usize,
    /// `Some` for a watched folder: how much of it this root covers, and the size at which
    /// it stops reading. `None` for a repository.
    pub scope: Option<&'a DraftScope>,
    /// Root-relative folders other roots look after (a watched folder inside this one),
    /// with whether the whole tree below each belongs there.
    pub excluded_dirs: &'a [ExcludedDir],
    /// `<repo>/index.tmp`, for D5 rename pairing.
    pub index_tmp: &'a Path,
    /// Whether a reseed noticed under this scan is worth another attempt. The engine sets
    /// it on every attempt but the last; on the last one the scan keeps the pile it built
    /// and says so in a notice, which is what every scan did before Phase 13.
    pub retry_on_reseed: bool,
    /// `skip_globs` for a watched folder (Amendment v1.15): a candidate or a nested
    /// repository whose parent-relative path matches is dropped where `excluded_dirs` are.
    /// `None` for a repository, whose own files no key may hide, and when the list is empty.
    pub skip: Option<DraftSkip<'a>>,
    /// The root's last pile, whose renderings a row may take instead of a diff (Phase 17
    /// Part B, fix 3). `None` renders every row, which is also how the tests force the reuse
    /// off: the engine passes it only when the collapse rules are the ones that pile was
    /// rendered under.
    pub reuse: Option<Reuse<'a>>,
}

/// What a scan may take from the root's last pile (Phase 17 Part B, fix 3).
///
/// A row's rendering, `(added, deleted, hunks, collapsed)`, is a function of its two sides
/// (oid and mode each), the collapse rules and the store's `core.filemode`, and of nothing
/// else. So a row whose sides are the ones the last pile's row had takes that row's
/// rendering whole, when that row was rendered with both blobs in hand (`rendered`): not
/// from a scan whose blob batch failed, and with neither side missing from the store. The
/// caller vouches for the collapse rules; `core.filemode` is fixed for a [`Store`].
#[derive(Debug, Clone, Copy)]
pub struct Reuse<'a> {
    pub pile: &'a Pile,
    /// [`ScanOutput::rendered`] of the scan that built `pile`.
    pub rendered: &'a HashSet<Vec<u8>>,
}

impl<'a> Reuse<'a> {
    /// The last pile's row for `row`'s path, when its rendering is `row`'s too.
    fn row_for(&self, row: &Row) -> Option<&'a Row> {
        // The rows that never read blobs keep their own path, as they always have.
        if matches!(row.change, Change::Typechange | Change::Unreadable)
            || matches!(row.collapsed, Some(Collapsed::Unread { .. }))
            || !self.rendered.contains(&row.path)
        {
            return None;
        }
        // A pile's rows are sorted by path; a miss only costs the reuse.
        let rows = &self.pile.rows;
        let i = rows
            .binary_search_by(|r| r.path.as_slice().cmp(&row.path))
            .ok()?;
        let prev = &rows[i];
        (prev.baseline == row.baseline && prev.current == row.current).then_some(prev)
    }
}

/// A watched folder's `skip_globs` test. The patterns are relative to the parent dir the
/// folder is filed under and the scan's paths to the folder itself, so each path is
/// matched with the folder's own parent-relative `prefix` in front of it (design review
/// F13): exactly the path discovery matches, so the two never disagree about a pattern
/// that starts with a glob (`*/z_ignore/**/evals/**`).
#[derive(Debug, Clone, Copy)]
pub struct DraftSkip<'a> {
    pub globs: &'a GlobSet,
    /// The folder's path below its parent dir, `/`-separated; empty when it is the parent
    /// dir itself.
    pub prefix: &'a [u8],
}

impl DraftSkip<'_> {
    /// Whether the root-relative `rel` is left out: it matches, or a folder above it inside
    /// the root matches (as `dir` or `dir/`), the way discovery never walks into one.
    pub fn skips(&self, rel: &[u8]) -> bool {
        let rel = rel.strip_suffix(b"/").unwrap_or(rel);
        let mut full = Vec::with_capacity(self.prefix.len() + 1 + rel.len() + 1);
        if !self.prefix.is_empty() {
            full.extend_from_slice(self.prefix);
            full.push(b'/');
        }
        let start = full.len();
        full.extend_from_slice(rel);
        let is_match = |b: &[u8]| self.globs.is_match(Path::new(OsStr::from_bytes(b)));
        if is_match(&full) {
            return true;
        }
        for i in start..full.len() {
            if full[i] == b'/' && (is_match(&full[..i]) || is_match(&full[..=i])) {
                return true;
            }
        }
        let mut dir = full;
        dir.push(b'/');
        is_match(&dir)
    }
}

/// What a scan produced besides the pile.
#[derive(Debug)]
pub struct ScanOutput {
    pub pile: Pile,
    /// Nested repositories reported by `ls-files --others` (`dir/`), for root discovery.
    pub nested_repos: Vec<Vec<u8>>,
    /// `hash-object` invocations this scan issued (the stat-cache test).
    pub hash_calls: u64,
    /// Whether `update-index --refresh` ran (false under `index.lock` contention).
    pub refreshed: bool,
    /// The paths whose rendering this scan had both blobs in hand for, taken over from
    /// the last pile included: what the next scan's [`Reuse::rendered`] is.
    pub rendered: HashSet<Vec<u8>>,
}

/// Probe whether the root's filesystem is case-insensitive: `lstat` of the root path with
/// its last component case-toggled resolves to the same device+inode. A name without
/// letters falls back to the platform default.
pub fn probe_case_insensitive(root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(name) = root.file_name() else {
        return cfg!(target_os = "macos");
    };
    let bytes = name.as_bytes();
    if !bytes.iter().any(|b| b.is_ascii_alphabetic()) {
        return cfg!(target_os = "macos");
    }
    let toggled: Vec<u8> = bytes
        .iter()
        .map(|b| {
            if b.is_ascii_lowercase() {
                b.to_ascii_uppercase()
            } else if b.is_ascii_uppercase() {
                b.to_ascii_lowercase()
            } else {
                *b
            }
        })
        .collect();
    let sibling = root.with_file_name(OsStr::from_bytes(&toggled));
    match (
        std::fs::symlink_metadata(root),
        std::fs::symlink_metadata(&sibling),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Byte-exact directory listings, one per directory per scan.
#[derive(Default)]
struct DirListings {
    cache: HashMap<PathBuf, Option<HashSet<OsString>>>,
}

impl DirListings {
    /// Whether `rel`'s final component is present **byte-exactly** in its parent directory.
    /// An unreadable parent returns `true` (let `lstat` decide).
    fn exact_exists(&mut self, root: &Path, rel: &[u8]) -> bool {
        let rel_path = Path::new(OsStr::from_bytes(rel));
        let Some(name) = rel_path.file_name() else {
            return true;
        };
        let parent = root.join(rel_path.parent().unwrap_or(Path::new("")));
        let listing = self.cache.entry(parent.clone()).or_insert_with(|| {
            std::fs::read_dir(&parent).ok().map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .collect::<HashSet<OsString>>()
            })
        });
        match listing {
            Some(set) => set.contains(name),
            None => true,
        }
    }
}

fn under_any(path: &[u8], dirs: &[Vec<u8>]) -> bool {
    dirs.iter()
        .any(|d| path.len() > d.len() && path.starts_with(d) && path[d.len()] == b'/')
}

/// Whether the record holds an entry for `path` — an accepted content, else the seen
/// tree's entry. The same order [`BaselineResolver::baseline`] resolves in, answered
/// without reading any object, because the size rule has to know before the row cap
/// whether a large file is one the user has already had on a screen.
///
/// An override that records the path as *absent* (an accepted deletion) is not an entry:
/// there is nothing left to show, which is exactly the state accepting an unread row
/// leaves behind.
///
/// A **flag-only** override counts (verifier F1). R2(b) says an override of any kind means
/// the record holds the path, and the reason is the reader's: a file somebody has flagged
/// must not leave the screen when it grows, or the flag goes with it into a ledger nothing
/// on screen can reach.
fn record_holds(inputs: &ScanInputs<'_>, path: &[u8]) -> bool {
    let over = std::str::from_utf8(path)
        .ok()
        .and_then(|s| inputs.ledger.overrides.get(s));
    match over.map(|o| &o.blob) {
        Some(Some(Some(_))) => true,
        Some(Some(None)) => false,
        Some(None) => true,
        None => inputs.tree.contains_key(path),
    }
}

/// The most blob bytes one `cat-file --batch` of the render fetches and holds: the rows are
/// rendered in chunks taken in order, each chunk's blobs fetched together and dropped
/// before the next chunk's are. A row whose blobs alone exceed it is a chunk of its own.
pub const BLOB_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

/// What a blob of unknown size is charged against [`BLOB_CHUNK_BYTES`]: a chunk of such
/// blobs is bounded by a count (1,024) instead. The scan knows every size in practice,
/// a baseline's from the resolver's `--batch-check`, a current side's from its `lstat`.
const UNKNOWN_BLOB_BYTES: u64 = BLOB_CHUNK_BYTES / 1024;

fn blob_chunk_bytes() -> u64 {
    #[cfg(test)]
    if let Some(b) = CHUNK_BYTES.with(std::cell::Cell::get) {
        return b;
    }
    BLOB_CHUNK_BYTES
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE)].contains(&0)
}

#[cfg(test)]
thread_local! {
    /// Between `diff_files` and `others`: the window schedule S2 opens, where a reseed
    /// costs a path that is pending under both trees (Phase 13 deliverable B).
    pub(crate) static DIFF_FILES_HOOK: crate::testhook::TestHook<()> =
        const { crate::testhook::TestHook::new() };
    /// `Some(n)`: the scan's `n`th blob fetch (from 0, counting only chunks that fetch
    /// anything) fails as a broken store's would, so the fail-open path runs on a healthy
    /// fixture.
    pub(crate) static FAIL_BLOB_BATCH: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
    /// `Some(bytes)`: the chunk budget in place of [`BLOB_CHUNK_BYTES`], so a fixture's few
    /// small blobs span several chunks.
    pub(crate) static CHUNK_BYTES: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    /// The blob fetches the scans on this thread have run (one per chunk that fetched).
    pub(crate) static BLOB_FETCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Blobs the batch comes back without, as if pruned between the resolver's existence
    /// check and the fetch (the only way a fixture reaches "not in the store").
    pub(crate) static DROP_FROM_BATCH: std::cell::RefCell<Vec<Oid>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run one scan.
pub fn scan(inputs: &ScanInputs<'_>) -> Result<ScanOutput, ScanError> {
    let store = inputs.store;
    let root = store.root();
    let mut notices: Vec<String> = Vec::new();
    let calls_before = store.git().hash_object_calls();

    // 1. Index refresh and enumeration.
    let mut marker = inputs.index.ensure(inputs.seen_tree)?.marker;
    let refreshed = inputs.index.refresh();
    if !refreshed {
        notices.push("index.lock held by another process; scanned without refresh".into());
    }
    let diff = match inputs.index.diff_files() {
        Ok(d) => d,
        Err(e) => {
            // The private index is a cache: a torn or truncated file is rebuilt from the
            // seen tree and the scan tries once more.
            notices.push(format!("private index unreadable ({e}); reseeded"));
            marker = inputs.index.reseed(inputs.seen_tree)?;
            inputs.index.diff_files()?
        }
    };
    #[cfg(test)]
    DIFF_FILES_HOOK.with(|h| h.fire(()));
    let others = inputs.index.others(inputs.scope)?;
    // The two reads above are one picture of the index, and a reseed between them can
    // omit a path that is pending under the reader's tree *and* under the new one. The
    // marker is replaced by rename at every seed, so its identity says whether this scan
    // read one index or two.
    // A missing marker is a seed in flight and never "the same as before", whatever the
    // first look saw (code verification F1).
    let marker_now = inputs.index.marker_id();
    if marker_now.is_none() || marker_now != marker {
        if inputs.retry_on_reseed {
            return Err(ScanError::IndexMoved);
        }
        notices.push(
            "the private index was reseeded while this root was scanned; \
             the list is refreshed by the next scan"
                .into(),
        );
    }
    let mut nested_repos: Vec<Vec<u8>> = others
        .iter()
        .filter_map(|o| match o {
            Other::NestedRepo(d) => Some(d.clone()),
            Other::File(_) => None,
        })
        // A nested repository `skip_globs` names is not reported for promotion.
        .filter(|d| !inputs.skip.is_some_and(|s| s.skips(d)))
        .collect();
    nested_repos.sort();

    // The user's index: skip-worktree paths (D6) and conflicted paths.
    let mut skip: HashSet<Vec<u8>> = HashSet::new();
    let mut conflicted: HashSet<Vec<u8>> = HashSet::new();
    if let Some(rg) = inputs.repo {
        match rg.run(&["ls-files", "-v", "-z"]) {
            Ok(out) => {
                for (tag, path) in git::parse_ls_files_v_z(&out) {
                    if tag == b'S' || tag == b's' {
                        skip.insert(path);
                    }
                }
            }
            Err(e) => notices.push(format!("cannot read the repository index: {e}")),
        }
        match rg.run(&["ls-files", "-u", "-z"]) {
            Ok(out) => {
                if let Ok(entries) = git::parse_ls_files_stage_z(&out) {
                    conflicted.extend(entries.into_iter().map(|e| e.path));
                }
            }
            Err(e) => notices.push(format!("cannot read conflict state: {e}")),
        }
    }

    // 2. Candidates.
    let mut candidates: BTreeSet<Vec<u8>> = BTreeSet::new();
    for d in &diff {
        candidates.insert(d.path.clone());
        if let Some(dest) = &d.dest {
            candidates.insert(dest.clone());
        }
    }
    for key in inputs
        .ledger
        .overrides
        .keys()
        .chain(inputs.ledger.unparsable.keys())
    {
        candidates.insert(key.as_bytes().to_vec());
    }
    let mut listings = DirListings::default();
    let mut forced_absent: HashSet<Vec<u8>> = HashSet::new();
    if inputs.case_insensitive {
        // D4: byte-exact readdir decides the current side of every tree/override path.
        for path in inputs.tree.keys() {
            if !listings.exact_exists(root, path) {
                candidates.insert(path.clone());
                forced_absent.insert(path.clone());
            }
        }
        for key in inputs.ledger.overrides.keys() {
            let p = key.as_bytes();
            if !listings.exact_exists(root, p) {
                forced_absent.insert(p.to_vec());
            }
        }
    }
    for o in &others {
        if let Other::File(p) = o
            && !under_any(p, &nested_repos)
            && !under_excluded(p, inputs.excluded_dirs)
            && !inputs.skip.is_some_and(|s| s.skips(p))
        {
            candidates.insert(p.clone());
        }
    }
    // D6: a skip-worktree path is excluded only while it is absent from the worktree (the
    // sparse cone); one that is present is a real file and stays a candidate. The filter
    // applies to every candidate whichever list it came from: an absent cone path is a
    // cone even when an override or `diff-files` names it.
    let root_dir = inputs.store.root();
    // A restore's in-flight temp file (`restore::RESTORE_TEMP_GLOB`) is never a row: it
    // exists for the microseconds between `create_new` and `rename`, and a second lastcall
    // scanning in that window would otherwise show it as an added file the user could
    // accept. A ghost left by a crash is hidden for the same reason and swept by the next
    // restore of that name (review F8).
    let candidates: Vec<Vec<u8>> = candidates
        .into_iter()
        .filter(|p| !crate::restore::is_restore_temp_path(p))
        .filter(|p| {
            !skip.contains(p)
                || root_dir
                    .join(OsStr::from_bytes(p))
                    .symlink_metadata()
                    .is_ok()
        })
        .collect();

    // 2b. The scope and the size limit of a watched folder (Amendment v1.13); a
    // repository passes through untouched.
    //
    // One `lstat` per candidate, **before** the row cap: a file the folder is not reading
    // must not take one of the cap's rows, and the answers are handed down to the hashing
    // pass so the syscall is paid once per candidate per scan rather than twice.
    //
    // A file at or above the limit is never read. It still gets a row when the folder's
    // record already holds an entry for it — the whole point of F1's fold: a file that
    // grew past the limit after it was recorded must not disappear off the screen — and is
    // only counted otherwise. A path with nothing at it is a deletion, not a large file.
    let mut meta_of: HashMap<Vec<u8>, MetaOf> = HashMap::new();
    let mut unread: HashSet<Vec<u8>> = HashSet::new();
    let mut over_size = 0usize;
    let candidates: Vec<Vec<u8>> = match inputs.scope {
        None => candidates,
        Some(scope) => {
            let mut kept: Vec<Vec<u8>> = Vec::with_capacity(candidates.len());
            for p in candidates {
                if !scope.admits_shape(&p)
                    || under_excluded(&p, inputs.excluded_dirs)
                    || inputs.skip.is_some_and(|s| s.skips(&p))
                {
                    continue;
                }
                let meta = store.lstat(&p);
                if let MetaOf::Present(m) = &meta
                    && scope.too_big(m)
                {
                    if record_holds(inputs, &p) {
                        unread.insert(p.clone());
                    } else {
                        over_size += 1;
                        continue;
                    }
                }
                meta_of.insert(p.clone(), meta);
                kept.push(p);
            }
            kept
        }
    };

    // 3. The row cap, decided before any hashing: override paths (flags live there) are
    // always materialised; every other candidate — diff-files, others, case-rule and
    // unparsable paths alike — is taken in path order until `row_cap` rows exist, and
    // what remains is only counted. `candidates` is path-ordered (it was a BTreeSet).
    let (priority, rest): (Vec<Vec<u8>>, Vec<Vec<u8>>) = candidates.into_iter().partition(|p| {
        std::str::from_utf8(p)
            .ok()
            .is_some_and(|s| inputs.ledger.overrides.contains_key(s))
    });

    // 4. Baselines.
    let mut resolver = BaselineResolver::new(
        inputs.ledger,
        inputs.tree,
        store,
        priority.iter().chain(rest.iter()).map(Vec::as_slice),
    );

    let filemode = store.filemode();
    let norm = |m: Mode| -> Mode {
        if !filemode && m == Mode::Executable {
            Mode::Regular
        } else {
            m
        }
    };

    // Current side hashed in one batch per call; a row per path whose sides differ.
    let over_bytes = inputs.collapse_size_bytes;
    // A current blob's size, from the same `lstat` the hashing used: the render's chunks
    // are sized from it.
    let mut cur_sizes: HashMap<Oid, u64> = HashMap::new();
    let mut materialise = |paths: &[Vec<u8>], rows: &mut Vec<Row>| {
        // A file over the limit is handed on as "nothing there", which is how it reaches
        // the hashing pass without being hashed: its row is built from the record alone.
        let metas: Vec<MetaOf> = paths
            .iter()
            .map(|p| {
                if unread.contains(p) {
                    return MetaOf::Absent;
                }
                match meta_of.get(p) {
                    Some(m) => m.clone(),
                    None => store.lstat(p),
                }
            })
            .collect();
        let currents = store.hash_paths_with(paths, &metas);
        for ((path, current), meta) in paths.iter().zip(currents).zip(&metas) {
            if let (Current::Present { oid, .. }, MetaOf::Present(m)) = (&current, meta) {
                cur_sizes.insert(oid.clone(), m.len());
            }
            let current = if forced_absent.contains(path) {
                Current::Absent
            } else {
                current
            };
            let baseline = resolver.baseline(path);
            let base_entry = match &baseline {
                Baseline::Present { oid, mode } => Some(Entry {
                    oid: oid.clone(),
                    mode: *mode,
                }),
                Baseline::Absent | Baseline::Empty => None,
            };
            let over = std::str::from_utf8(path)
                .ok()
                .and_then(|s| inputs.ledger.overrides.get(s));
            let flags = over.map(|o| o.flags.clone()).unwrap_or_default();
            let is_conflicted = conflicted.contains(path);
            let lossy = String::from_utf8_lossy(path).into_owned();
            if std::str::from_utf8(path).is_err() {
                notices.push(format!(
                    "{lossy}: non-UTF-8 path; shown pending, accept is refused in v1"
                ));
            }

            if unread.contains(path) {
                // Two ways to have no baseline. A path the user has flagged and the folder
                // never recorded is an **added** unread row, so the flag stays on screen
                // (verifier F1); a path whose recorded entry cannot be read back (its
                // object was pruned) has nothing for a row to stand on, and the file is
                // simply one this folder is not reading.
                let change = match (&base_entry, over.is_some()) {
                    (Some(_), _) => Change::Modified,
                    (None, true) => Change::Added,
                    (None, false) => {
                        over_size += 1;
                        continue;
                    }
                };
                rows.push(Row {
                    path: path.clone(),
                    change,
                    baseline: base_entry,
                    current: None,
                    added: 0,
                    deleted: 0,
                    hunks: Arc::default(),
                    annotation: None,
                    conflicted: is_conflicted,
                    collapsed: Some(Collapsed::Unread { over_bytes }),
                    flags,
                    rename: None,
                    seen_on: Vec::new(),
                });
                continue;
            }

            let (change, cur_entry) = match current {
                Current::Absent => match &base_entry {
                    Some(_) => (Change::Deleted, None),
                    None => continue, // nothing on either side
                },
                Current::Unhashable(reason) => {
                    notices.push(format!("{lossy}: cannot hash ({reason}); shown pending"));
                    let change = if reason.starts_with("typechange") {
                        Change::Typechange
                    } else {
                        Change::Unreadable
                    };
                    (change, None)
                }
                Current::Present { oid, mode } => {
                    let cur = Entry { oid, mode };
                    match &base_entry {
                        None => (Change::Added, Some(cur)),
                        Some(b) if b.oid != cur.oid => (Change::Modified, Some(cur)),
                        Some(b) if norm(b.mode) != norm(cur.mode) => (Change::Mode, Some(cur)),
                        Some(_) => continue, // equal: not pending
                    }
                }
            };

            let row = Row {
                path: path.clone(),
                change,
                baseline: base_entry,
                current: cur_entry,
                added: 0,
                deleted: 0,
                hunks: Arc::default(),
                annotation: None,
                conflicted: is_conflicted,
                collapsed: None,
                flags,
                rename: None,
                seen_on: Vec::new(),
            };
            rows.push(row);
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    materialise(&priority, &mut rows);
    let priority_rows = rows.len();
    let mut taken = 0usize;
    loop {
        let room = inputs.row_cap.saturating_sub(rows.len() - priority_rows);
        if room == 0 || taken == rest.len() {
            break;
        }
        let end = (taken + room).min(rest.len());
        materialise(&rest[taken..end], &mut rows);
        taken = end;
    }
    let omitted = rest.len() - taken;
    notices.append(&mut resolver.notices);

    // What rendering may change, as materialised: put back if a chunk's fetch fails.
    let materialised: Vec<Option<Collapsed>> = rows.iter().map(|r| r.collapsed).collect();

    // 4b. A row whose sides the last pile already rendered takes that rendering ([`Reuse`]):
    // its blobs are neither fetched nor diffed.
    let prior: Vec<Option<&Row>> = rows
        .iter()
        .map(|r| inputs.reuse.and_then(|reuse| reuse.row_for(r)))
        .collect();

    // 4c. Content for every other row, fetched in chunks of rows taken in order with one
    // `cat-file --batch` per chunk: rendering must not cost a process per row (2,000
    // unseen files are 2,000 rows on every rescan tick), nor hold every pending row's two
    // whole blobs at once (eight roots doing so at launch was a run's highest point).
    let reads = |r: &Row| {
        !matches!(r.change, Change::Typechange | Change::Unreadable)
            // An unread row has no content to diff, and its recorded blob may be large.
            && !matches!(r.collapsed, Some(Collapsed::Unread { .. }))
    };
    let oids_of = |r: &'_ Row| -> Vec<Oid> {
        let mut oids: Vec<Oid> = [&r.baseline, &r.current]
            .into_iter()
            .flatten()
            .map(|e| e.oid.clone())
            .collect();
        oids.dedup();
        oids
    };
    let size_of = |oid: &Oid| {
        resolver
            .size_of(oid)
            .or_else(|| cur_sizes.get(oid).copied())
            .unwrap_or(UNKNOWN_BLOB_BYTES)
    };
    // `[start, end)` row ranges. A chunk closes before the row whose blobs not already in
    // it would take it over the budget; the rows that fetch nothing ride along.
    let budget = blob_chunk_bytes();
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let (mut start, mut bytes, mut held) = (0usize, 0u64, HashSet::<Oid>::new());
    for (i, (row, prior)) in rows.iter().zip(&prior).enumerate() {
        if prior.is_some() || !reads(row) {
            continue;
        }
        let oids = oids_of(row);
        let cost = |held: &HashSet<Oid>| -> u64 {
            oids.iter()
                .filter(|o| !held.contains(*o))
                .map(&size_of)
                .sum()
        };
        let mut add = cost(&held);
        if !held.is_empty() && bytes.saturating_add(add) > budget {
            chunks.push((start, i));
            (start, bytes) = (i, 0);
            held.clear();
            add = cost(&held);
        }
        bytes = bytes.saturating_add(add);
        held.extend(oids);
    }
    chunks.push((start, rows.len()));
    drop(held);

    let notices_before = notices.len();
    let mut rendered: HashSet<Vec<u8>> = HashSet::new();
    let mut failed: Option<StoreError> = None;
    #[cfg(test)]
    let mut fetch_no = 0usize;
    for (start, end) in chunks {
        let mut wanted: Vec<Oid> = rows[start..end]
            .iter()
            .zip(&prior[start..end])
            .filter(|(r, prior)| prior.is_none() && reads(r))
            .flat_map(|(r, _)| oids_of(r))
            .collect();
        wanted.sort();
        wanted.dedup();
        let fetched = store.cat_blobs(&wanted);
        #[cfg(test)]
        // Only where a real batch runs: `cat_blobs` of nothing spawns no process.
        let fetched = match wanted.is_empty() {
            true => fetched,
            false => {
                BLOB_FETCHES.with(|n| n.set(n.get() + 1));
                fetch_no += 1;
                if FAIL_BLOB_BATCH.with(std::cell::Cell::get) == Some(fetch_no - 1) {
                    Err(StoreError::Other("injected batch failure".into()))
                } else {
                    fetched.map(|mut b| {
                        DROP_FROM_BATCH.with(|d| d.borrow().iter().for_each(|o| drop(b.remove(o))));
                        b
                    })
                }
            }
        };
        let blobs = match fetched {
            Ok(b) => b,
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        for (row, prior) in rows[start..end].iter_mut().zip(&prior[start..end]) {
            let whole = match prior {
                Some(prev) => {
                    row.added = prev.added;
                    row.deleted = prev.deleted;
                    row.hunks = Arc::clone(&prev.hunks);
                    row.collapsed = prev.collapsed;
                    true
                }
                None => render_content(store, inputs, Some(&blobs), row, &mut notices)?,
            };
            if whole {
                rendered.insert(row.path.clone());
            }
        }
        // `blobs` is dropped here, before the next chunk's are fetched.
    }
    // A failed fetch, whichever chunk's, fails open the whole scan's content, the rows the
    // earlier chunks rendered and the reused ones too, exactly as one failed batch did:
    // what a broken store shows does not depend on where the chunks fell or on what the
    // last scan saw.
    if let Some(e) = failed {
        notices.truncate(notices_before);
        notices.push(format!(
            "blob contents unreadable ({e}); rows shown without counts or hunks"
        ));
        rendered.clear();
        for (row, collapsed) in rows.iter_mut().zip(materialised) {
            row.added = 0;
            row.deleted = 0;
            row.hunks = Arc::default();
            row.collapsed = collapsed;
            render_content(store, inputs, None, row, &mut notices)?;
        }
    }

    // 5. D5 rename pairing: only when the pile has both deletions and additions. The
    // candidates are the pile's own rows — a deletion's baseline may be an override blob
    // (or the path may not be a row at all: an accepted deletion), so the seen tree is
    // not the right "before" side.
    let deleted: Vec<(Vec<u8>, Entry)> = rows
        .iter()
        .filter(|r| r.change == Change::Deleted)
        .filter_map(|r| r.baseline.clone().map(|b| (r.path.clone(), b)))
        .collect();
    let added: Vec<Vec<u8>> = rows
        .iter()
        .filter(|r| r.change == Change::Added && r.current.is_some())
        .map(|r| r.path.clone())
        .collect();
    if !deleted.is_empty() && !added.is_empty() {
        match detect_renames(store, inputs.index_tmp, &deleted, &added) {
            Ok(pairs) => {
                for (from, to, similarity) in pairs {
                    if let Some(r) = rows
                        .iter_mut()
                        .find(|r| r.path == from && r.change == Change::Deleted)
                    {
                        r.rename = Some(Rename::To {
                            to: to.clone(),
                            similarity,
                        });
                    }
                    if let Some(r) = rows
                        .iter_mut()
                        .find(|r| r.path == to && r.change == Change::Added)
                    {
                        r.rename = Some(Rename::From {
                            from: from.clone(),
                            similarity,
                        });
                    }
                }
            }
            Err(e) => notices.push(format!("rename detection skipped: {e}")),
        }
    }

    rows.sort_by(|a, b| a.path.cmp(&b.path));
    if omitted > 0 {
        notices.push(format!(
            "{} files shown · {} more changed paths not scanned (first {} by path)",
            with_thousands(rows.len()),
            with_thousands(omitted),
            with_thousands(inputs.row_cap)
        ));
    }
    // One line per scan, with the count as it stands now: it replaces the last scan's
    // line rather than adding to it, and there is no line at all when nothing was skipped.
    if over_size > 0 {
        notices.push(format!(
            "{} file{} over {} not read",
            with_thousands(over_size),
            if over_size == 1 { "" } else { "s" },
            size_limit_label(inputs.collapse_size_bytes)
        ));
    }
    Ok(ScanOutput {
        pile: Pile {
            rows: Arc::new(rows),
            notices,
            omitted,
            // Stamped by the engine (`scan_root`), which owns the clock the expiry needs.
            undo: 0,
            snoozed_until: None,
            seen_branch: inputs.ledger.seen_branch.clone(),
        },
        nested_repos,
        hash_calls: store.git().hash_object_calls() - calls_before,
        refreshed,
        rendered,
    })
}

/// Hunks, counts and the collapse decision for one row, from the batch-fetched blobs
/// (`None`: the batch failed; the row keeps its change kind and nothing else).
///
/// `true` when the row was rendered with both of its blobs in hand, so the next scan may
/// take this rendering over ([`Reuse`]); `false` for a failed batch, a side missing from
/// the store, and the rows that never read blobs.
fn render_content<'b>(
    store: &Store,
    inputs: &ScanInputs<'_>,
    blobs: Option<&'b HashMap<Oid, Vec<u8>>>,
    row: &mut Row,
    notices: &mut Vec<String>,
) -> Result<bool, ScanError> {
    // An unread row already carries its verdict, and it is the truer one.
    if matches!(row.collapsed, Some(Collapsed::Unread { .. })) {
        return Ok(false);
    }
    if inputs
        .collapsed_globs
        .is_match(Path::new(OsStr::from_bytes(&row.path)))
    {
        row.collapsed = Some(Collapsed::Glob);
    }
    if matches!(row.change, Change::Typechange | Change::Unreadable) {
        return Ok(false);
    }
    let Some(blobs) = blobs else {
        return Ok(false);
    };
    // Borrowed from the batch's map, never copied out of it.
    let mut whole = true;
    let mut read = |e: &Option<Entry>, what: &str| -> &'b [u8] {
        match e {
            Some(entry) => match blobs.get(&entry.oid) {
                Some(b) => b,
                None => {
                    notices.push(format!(
                        "{}: {what} unreadable (object {} not in the store)",
                        row.path_lossy(),
                        entry.oid
                    ));
                    whole = false;
                    &[]
                }
            },
            None => &[],
        }
    };
    let old = read(&row.baseline, "baseline");
    let new = read(&row.current, "current");
    let same_content = row
        .baseline
        .as_ref()
        .zip(row.current.as_ref())
        .is_some_and(|(b, c)| b.oid == c.oid);
    if row.collapsed.is_none() {
        if is_binary(old) || is_binary(new) {
            row.collapsed = Some(Collapsed::Binary);
        } else if over_size(old.len(), new.len(), inputs.collapse_size_bytes) {
            row.collapsed = Some(Collapsed::Size);
        }
    }
    if row.collapsed.is_some() {
        if !same_content {
            let (a, d) = hunks::counts(old, new);
            row.added = a;
            row.deleted = d;
        }
        return Ok(whole);
    }
    // One diff gives both the hunks and the counts: the hunks' counts sum to
    // `hunks::counts` exactly (`hunks_counts_are_the_sum_of_the_diffs_hunk_counts`).
    let mut hunks = Vec::new();
    if !same_content {
        hunks = hunks::diff(old, new);
        (row.added, row.deleted) = hunks.iter().fold((0, 0), |(a, d), h| {
            let (ha, hd) = h.counts();
            (a + ha, d + hd)
        });
    }
    if let (Some(b), Some(c)) = (&row.baseline, &row.current)
        && b.mode != c.mode
        && (store.filemode() || b.mode == Mode::Symlink || c.mode == Mode::Symlink)
    {
        let mut h = Hunk::mode_change(b.mode.as_str(), c.mode.as_str());
        h.index = hunks.len();
        hunks.push(h);
        if same_content {
            row.added = 1;
            row.deleted = 1;
        }
    }
    row.hunks = Arc::new(hunks);
    Ok(whole)
}

/// Whether a pair of blobs collapses as `Size`: either side at or above the configured
/// limit, or the two together past what one hunk's `u32` offsets can address
/// ([`hunks::Lines`]), which only a limit raised above 4 GiB can reach.
fn over_size(old: usize, new: usize, limit: u64) -> bool {
    old as u64 >= limit || new as u64 >= limit || (old + new) as u64 > u64::from(u32::MAX)
}

/// `(from, to, similarity)` as reported by `diff -M`.
type RenamePair = (Vec<u8>, Vec<u8>, u8);

/// D5: a temp index holding exactly the pile's **deleted rows at their baselines**
/// (`read-tree --empty` + `update-index --index-info`), `add -N` the added paths, then
/// `diff -M -z --name-status`. Returns `(from, to, similarity)`.
///
/// The index is built from the rows, not copied from the private index: the private index
/// is the seen tree, and the pile is `diff(tree ⊕ overrides, worktree)`. A copy would let
/// `diff -M` pair an addition with a path the user already accepted as deleted (override
/// `null`, no row), or score a deleted row against the tree's blob instead of its accepted
/// one — and since compaction folds overrides into the tree, the pairing would then change
/// across a fold that changes no baseline.
fn detect_renames(
    store: &Store,
    index_tmp: &Path,
    deleted: &[(Vec<u8>, Entry)],
    added: &[Vec<u8>],
) -> Result<Vec<RenamePair>, ScanError> {
    let _ = std::fs::remove_file(index_tmp);
    let result = (|| -> Result<Vec<RenamePair>, ScanError> {
        store
            .git()
            .run_with_index(index_tmp, &["read-tree", "--empty"])?;
        let mut stdin = Vec::new();
        for (path, e) in deleted {
            stdin.extend_from_slice(e.mode.as_str().as_bytes());
            stdin.push(b' ');
            stdin.extend_from_slice(e.oid.as_str().as_bytes());
            stdin.push(b'\t');
            stdin.extend_from_slice(path);
            stdin.push(0);
        }
        store.git().run_stdin(
            Some(index_tmp),
            &["update-index", "-z", "--index-info"],
            &stdin,
        )?;
        let mut args: Vec<OsString> = vec!["add".into(), "-N".into(), "--".into()];
        args.extend(added.iter().map(|p| OsString::from_vec(p.clone())));
        store.git().run_with_index(index_tmp, &args)?;
        let out = store
            .git()
            .run_with_index(index_tmp, &["diff", "-M", "-z", "--name-status"])?;
        Ok(parse_name_status_renames(&out))
    })();
    let _ = std::fs::remove_file(index_tmp);
    result
}

/// Parse `R<score>\0old\0new\0` records out of `diff -z --name-status`.
fn parse_name_status_renames(bytes: &[u8]) -> Vec<RenamePair> {
    let fields = git::split_nul(bytes);
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let status = fields[i];
        let two_paths = status.first().is_some_and(|c| *c == b'R' || *c == b'C');
        if two_paths && i + 2 < fields.len() {
            let score: u8 = std::str::from_utf8(&status[1..])
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if status[0] == b'R' {
                out.push((fields[i + 1].to_vec(), fields[i + 2].to_vec(), score));
            }
            i += 3;
        } else {
            i += 2;
        }
    }
    out
}

/// Path → (mode, oid) view of a pile's current side (used by fold).
pub fn current_entries(pile: &Pile) -> BTreeMap<Vec<u8>, Option<Entry>> {
    pile.rows
        .iter()
        .map(|r| (r.path.clone(), r.current.clone()))
        .collect()
}

/// The `RootKind`-agnostic summary line the harness compares against:
/// `"path"`, `"path upstream"`, `"path mixed"`, sorted.
pub fn pile_lines(pile: &Pile) -> Vec<String> {
    let mut lines: Vec<String> = pile
        .rows
        .iter()
        .map(|r| match r.annotation {
            Some(Annotation::Upstream) => format!("{} upstream", r.path_lossy()),
            Some(Annotation::Mixed) => format!("{} mixed", r.path_lossy()),
            None => r.path_lossy(),
        })
        .collect();
    lines.sort();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::PrivateIndex;
    use crate::ledger::{Ledger, SeenAt};
    use crate::paths::RepoPaths;
    use crate::store::RootKind;
    use lastcall_testkit::tmp::TempDir;

    /// The size limit as the notices print it.
    #[test]
    fn scan_size_limit_label_reads_in_whole_kib_where_it_divides() {
        assert_eq!(size_limit_label(524_288), "512 KiB");
        assert_eq!(size_limit_label(1_048_576), "1,024 KiB");
        assert_eq!(size_limit_label(1_000), "1,000 bytes");
        assert_eq!(size_limit_label(1_500), "1,500 bytes");
        // One byte is one byte (verifier F7). The rest of the vocabulary is plural.
        assert_eq!(size_limit_label(1), "1 byte");
        assert_eq!(size_limit_label(2), "2 bytes");
        assert_eq!(size_limit_label(1_024), "1 KiB");
    }

    /// A watched folder, its record and its private index: the shape the size rule and the
    /// scope filter are read against (Amendment v1.13 R1, R2).
    struct Watched {
        dir: TempDir,
        root: PathBuf,
        store: Store,
        index: PrivateIndex,
        ledger: Ledger,
        tree: TreeEntries,
        globs: globset::GlobSet,
        paths: RepoPaths,
        max: u64,
    }

    impl Watched {
        fn new(max: u64) -> Self {
            let dir = TempDir::new("lc-scan-draft");
            let root = dir.mkdir("notes");
            let state = dir.mkdir("state");
            let env = crate::env::Env::empty(dir.path())
                .with_home(dir.mkdir("home"))
                .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
                .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
                .with_var("GIT_CONFIG_NOSYSTEM", "1");
            let paths = RepoPaths::under(state.join("repo"));
            let (store, _) = Store::open(&env, &root, RootKind::Draft, &paths, None).unwrap();
            let index = PrivateIndex::new(
                store.git().clone(),
                &paths,
                RootKind::Draft,
                None,
                Vec::new(),
            );
            let ledger = Ledger::new(
                &root,
                RootKind::Draft,
                None,
                SeenAt {
                    head_commit: None,
                    branch: None,
                    at: "2026-01-01T00:00:00Z".into(),
                },
            );
            Self {
                dir,
                root,
                store,
                index,
                ledger,
                tree: TreeEntries::new(),
                globs: globset::GlobSetBuilder::new().build().unwrap(),
                paths,
                max,
            }
        }

        fn write(&self, name: &str, bytes: usize) {
            let path = self.root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![b'x'; bytes]).unwrap();
        }

        /// A flag on a path and nothing else: the override carries no blob, exactly as
        /// `m` leaves it (verifier F1).
        fn flag(&mut self, name: &str, note: &str) {
            self.ledger.overrides.insert(
                name.to_owned(),
                crate::ledger::Override {
                    blob: None,
                    mode: None,
                    flags: vec![crate::ledger::Flag {
                        note: note.to_owned(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                        hunk: None,
                        summary: None,
                    }],
                    updated_at: "2026-01-01T00:00:00Z".into(),
                },
            );
        }

        /// First sight under `scope`: what the folder records is what it reads.
        fn first_sight(&mut self, scope: &DraftScope) {
            let seen = self.store.tree_of_disk(scope, &[]).unwrap();
            self.tree = self.store.ls_tree(&seen).unwrap();
            self.ledger.seen_tree = Some(seen);
        }

        fn scan(&self, scope: &DraftScope) -> ScanOutput {
            self.scan_skipping(scope, None)
        }

        fn scan_skipping(&self, scope: &DraftScope, skip: Option<DraftSkip<'_>>) -> ScanOutput {
            super::scan(&ScanInputs {
                store: &self.store,
                index: &self.index,
                repo: None,
                ledger: &self.ledger,
                seen_tree: self.ledger.seen_tree.as_ref(),
                tree: &self.tree,
                case_insensitive: false,
                collapsed_globs: &self.globs,
                scope: Some(scope),
                collapse_size_bytes: self.max,
                excluded_dirs: &[],
                index_tmp: &self.paths.index_tmp,
                row_cap: crate::engine::DEFAULT_ROW_CAP,
                retry_on_reseed: false,
                skip,
                reuse: None,
            })
            .unwrap()
        }
    }

    /// Amendment v1.15 (scenario A9): inside a watched folder, a file or a nested
    /// repository whose parent-relative path matches `skip_globs` is neither a candidate
    /// nor reported for promotion; the folder's other files are untouched.
    #[test]
    fn scan_watched_folder_drops_what_skip_globs_name() {
        let max = 1 << 20;
        let w = Watched::new(max);
        w.write("notes.md", 4);
        w.write("evals/run.log", 4);
        w.write("evals/sub/x.md", 4);
        let env = crate::env::Env::empty(w.dir.path())
            .with_home(w.dir.path().join("home"))
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1");
        for clone in ["evals/c1", "other/c2"] {
            assert!(
                crate::git::base_command(&env, &w.root)
                    .args(["init", "-q", clone])
                    .status()
                    .unwrap()
                    .success()
            );
            w.write(&format!("{clone}/x"), 2);
        }
        let tree = DraftScope::tree(max);
        let globs = crate::config::skip_set(&["*/z_ignore/**/evals/**".to_owned()]);
        // The folder is `proj/z_ignore` below its parent dir.
        let skip = DraftSkip {
            globs: &globs,
            prefix: b"proj/z_ignore",
        };

        let all = w.scan(&tree);
        assert_eq!(
            row_paths(&all),
            vec!["evals/run.log", "evals/sub/x.md", "notes.md"]
        );
        assert_eq!(
            all.nested_repos,
            vec![b"evals/c1".to_vec(), b"other/c2".to_vec()]
        );

        let out = w.scan_skipping(&tree, Some(skip));
        assert_eq!(row_paths(&out), vec!["notes.md"]);
        assert_eq!(out.nested_repos, vec![b"other/c2".to_vec()]);

        // A pattern that names the folder `evals` alone prunes everything below it too.
        let globs = crate::config::skip_set(&["proj/z_ignore/evals".to_owned()]);
        let out = w.scan_skipping(
            &tree,
            Some(DraftSkip {
                globs: &globs,
                prefix: b"proj/z_ignore",
            }),
        );
        assert_eq!(row_paths(&out), vec!["notes.md"]);
        assert_eq!(out.nested_repos, vec![b"other/c2".to_vec()]);

        // A folder watched on its own: unaffected unless its own files match.
        let plain = DraftScope::plain(max);
        let globs = crate::config::skip_set(&["*/z_ignore/evals/**".to_owned()]);
        let out = w.scan_skipping(
            &plain,
            Some(DraftSkip {
                globs: &globs,
                prefix: b"proj/z_ignore",
            }),
        );
        assert_eq!(row_paths(&out), vec!["notes.md"]);
        assert_eq!(row_paths(&w.scan(&plain)), vec!["notes.md"]);
        let globs = crate::config::skip_set(&["*/z_ignore/notes.md".to_owned()]);
        let out = w.scan_skipping(
            &plain,
            Some(DraftSkip {
                globs: &globs,
                prefix: b"proj/z_ignore",
            }),
        );
        assert!(row_paths(&out).is_empty(), "{:?}", row_paths(&out));

        // The folder filed as the parent dir itself: no prefix.
        let globs = crate::config::skip_set(&["evals/**".to_owned()]);
        let bare = DraftSkip {
            globs: &globs,
            prefix: b"",
        };
        assert!(bare.skips(b"evals/run.log"));
        assert!(bare.skips(b"evals/c1/"));
        assert!(!bare.skips(b"notes.md"));
    }

    fn row_paths(out: &ScanOutput) -> Vec<String> {
        out.pile
            .rows
            .iter()
            .map(|r| String::from_utf8_lossy(&r.path).into_owned())
            .collect()
    }

    fn unread_notices(out: &ScanOutput) -> Vec<String> {
        out.pile
            .notices
            .iter()
            .filter(|n| n.contains("not read"))
            .cloned()
            .collect()
    }

    /// Amendment v1.13 R2 with design review F1 and F3: a folder never reads a large file,
    /// counts the ones it has never recorded in one notice, and keeps a row for one its
    /// record already holds so a file that grew past the limit cannot vanish.
    #[test]
    fn scan_watched_folder_never_reads_a_large_file_and_never_hides_a_recorded_one() {
        let max = 1024u64;
        let mut w = Watched::new(max);
        w.write("a.md", 4);
        w.write("grown.bin", 10);
        w.write("big.bin", max as usize);
        w.write("sub/b.md", 4);
        w.first_sight(&DraftScope::plain(max));

        // What first sight recorded: the folder's own small files, and nothing else.
        let recorded: Vec<String> = w
            .tree
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect();
        assert_eq!(recorded, vec!["a.md".to_owned(), "grown.bin".to_owned()]);

        let out = w.scan(&DraftScope::plain(max));
        assert!(row_paths(&out).is_empty(), "nothing pending at first sight");
        assert_eq!(unread_notices(&out), vec!["1 file over 1 KiB not read"]);

        // A recorded file that grows past the limit keeps its row, and the row says the
        // content was not read: no current side, no hunks, and it is not counted.
        w.write("grown.bin", max as usize);
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["grown.bin".to_owned()]);
        let row = &out.pile.rows[0];
        assert_eq!(row.collapsed, Some(Collapsed::Unread { over_bytes: max }));
        assert!(row.current.is_none(), "no current-side hash");
        assert!(
            row.baseline.is_some(),
            "the record is what the row stands on"
        );
        assert!(row.hunks.is_empty());
        assert_eq!((row.added, row.deleted), (0, 0));
        assert_eq!(
            unread_notices(&out),
            vec!["1 file over 1 KiB not read"],
            "a row is not counted"
        );

        // One line per scan with the count as it stands, never two.
        w.write("big2.bin", max as usize + 5);
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(unread_notices(&out), vec!["2 files over 1 KiB not read"]);

        // A file that shrinks below the limit is read again, and the count falls.
        w.write("grown.bin", 12);
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["grown.bin".to_owned()]);
        assert!(
            out.pile.rows[0].collapsed.is_none(),
            "read as an edit again"
        );
        assert_eq!(unread_notices(&out), vec!["2 files over 1 KiB not read"]);

        // Nothing left over the limit: no line at all.
        std::fs::remove_file(w.root.join("big.bin")).unwrap();
        std::fs::remove_file(w.root.join("big2.bin")).unwrap();
        let out = w.scan(&DraftScope::plain(max));
        assert!(unread_notices(&out).is_empty());
    }

    /// A recorded path with nothing at it is a deletion, never a large file.
    #[test]
    fn scan_watched_folder_counts_no_deletion_as_a_large_file() {
        let max = 1024u64;
        let mut w = Watched::new(max);
        w.write("a.md", 4);
        w.first_sight(&DraftScope::plain(max));
        std::fs::remove_file(w.root.join("a.md")).unwrap();
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["a.md".to_owned()]);
        assert_eq!(out.pile.rows[0].change, Change::Deleted);
        assert!(unread_notices(&out).is_empty());
    }

    /// Verifier F1: an override of **any** kind means the record holds the path, so a file
    /// the reader has flagged keeps its row when it grows past the limit even though the
    /// folder never recorded its content. Without this the flag goes with the row, into a
    /// ledger nothing on screen can reach.
    #[test]
    fn scan_watched_folder_keeps_a_flagged_new_file_that_grows() {
        let max = 1024u64;
        let mut w = Watched::new(max);
        w.write("a.md", 4);
        w.first_sight(&DraftScope::plain(max));
        w.write("newf.txt", 10);
        w.flag("newf.txt", "agent says look");

        // Small: an ordinary added row, carrying the flag.
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["newf.txt".to_owned()]);
        assert_eq!(out.pile.rows[0].change, Change::Added);
        assert!(unread_notices(&out).is_empty());

        // Grown to the limit: still a row, now an unread one, still carrying the flag.
        w.write("newf.txt", max as usize);
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["newf.txt".to_owned()]);
        let row = &out.pile.rows[0];
        assert_eq!(row.change, Change::Added, "the record holds no content");
        assert_eq!(row.collapsed, Some(Collapsed::Unread { over_bytes: max }));
        assert!(row.baseline.is_none() && row.current.is_none());
        assert!(row.hunks.is_empty());
        assert_eq!((row.added, row.deleted), (0, 0));
        assert_eq!(
            row.flags
                .iter()
                .map(|f| f.note.as_str())
                .collect::<Vec<_>>(),
            vec!["agent says look"],
            "the flag is on the row, not stranded in the ledger"
        );
        assert!(
            unread_notices(&out).is_empty(),
            "a row is not counted, so the notice stays at zero"
        );

        // An unflagged file of the same size next to it is the counted case.
        w.write("big.bin", max as usize);
        let out = w.scan(&DraftScope::plain(max));
        assert_eq!(row_paths(&out), vec!["newf.txt".to_owned()]);
        assert_eq!(unread_notices(&out), vec!["1 file over 1 KiB not read"]);
    }

    /// The scope filter runs over every candidate list, whichever one named the path: a
    /// folder watched on its own never shows a file below it, and the tree scope does.
    #[test]
    fn scan_watched_folder_scope_filters_every_candidate_list() {
        let max = 1024u64;
        let mut w = Watched::new(max);
        w.write("a.md", 4);
        w.write("sub/b.md", 4);
        w.first_sight(&DraftScope::tree(max));

        // A record written under the wider scope: `sub/b.md` is in the tree, so
        // `diff-files` names it as well as the untracked listing.
        w.write("a.md", 6);
        w.write("sub/b.md", 6);
        assert_eq!(
            row_paths(&w.scan(&DraftScope::tree(max))),
            vec!["a.md".to_owned(), "sub/b.md".to_owned()]
        );
        assert_eq!(
            row_paths(&w.scan(&DraftScope::plain(max))),
            vec!["a.md".to_owned()],
            "one folder only, whatever list named the path"
        );
        assert!(w.dir.path().exists());
    }

    #[test]
    fn scan_parse_name_status_renames() {
        let bytes = b"R090\0old.rs\0new.rs\0M\0f1\0A\0x\0C075\0a\0b\0D\0gone\0";
        assert_eq!(
            parse_name_status_renames(bytes),
            vec![(b"old.rs".to_vec(), b"new.rs".to_vec(), 90)]
        );
    }

    #[test]
    fn scan_under_any_and_binary_probe() {
        let dirs = vec![b"vendor/lib".to_vec()];
        assert!(under_any(b"vendor/lib/x.c", &dirs));
        assert!(!under_any(b"vendor/lib", &dirs));
        assert!(!under_any(b"vendor/libx/y", &dirs));
        assert!(is_binary(b"ab\0c"));
        assert!(!is_binary(b"plain text\n"));
        let mut big = vec![b'a'; BINARY_PROBE + 10];
        big[BINARY_PROBE + 5] = 0;
        assert!(
            !is_binary(&big),
            "a NUL past the probe window is text to git"
        );
    }

    #[test]
    fn scan_case_probe_matches_the_platform() {
        let dir = lastcall_testkit::tmp::TempDir::new("lc-case");
        let root = dir.mkdir("Mixed");
        let probe = probe_case_insensitive(&root);
        let sibling = dir.join("mIXED");
        assert_eq!(probe, sibling.exists());
        let noletters = dir.mkdir("1234");
        assert_eq!(
            probe_case_insensitive(&noletters),
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn scan_exact_exists_is_byte_exact() {
        let dir = lastcall_testkit::tmp::TempDir::new("lc-exact");
        dir.write("d/f.txt", "x");
        let mut l = DirListings::default();
        assert!(l.exact_exists(dir.path(), b"d/f.txt"));
        assert!(!l.exact_exists(dir.path(), b"d/F.txt"));
        assert!(
            l.exact_exists(dir.path(), b"nodir/f"),
            "unreadable parent: let lstat decide"
        );
    }
}

/// A one-root harness shared by the scan and ops unit tests (integration tests use the
/// engine itself).
#[cfg(test)]
pub(crate) mod fixture_tests {
    use super::*;
    use crate::git::RepoGit;
    use crate::ledger::{FixedClock, Override, SeenAt};
    use crate::ops::Ops;
    use crate::paths::RepoPaths;
    use crate::store::tests::fixture_env;
    use crate::store::{RepoFacts, RootKind, TreeWrite};
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    pub(crate) struct Harness {
        pub(crate) store: Store,
        pub(crate) index: PrivateIndex,
        pub(crate) repo_git: RepoGit,
        pub(crate) ledger: Ledger,
        pub(crate) tree_entries: TreeEntries,
        pub(crate) globs: GlobSet,
        pub(crate) paths: RepoPaths,
        pub(crate) case_insensitive: bool,
        pub(crate) clock: FixedClock,
        pub(crate) compaction_threshold: usize,
        /// The fixture's `.git`, for the tests that drive the branch switch.
        pub(crate) git_dir: std::path::PathBuf,
    }

    impl Harness {
        /// [`Harness::ops`] with R5's two fields filled in: the branch the op stages its
        /// work under, and the git dir whose `HEAD` says which branch is really in force.
        pub(crate) fn ops_on(&mut self, branch: &str) -> Ops<'_> {
            let git_dir = self.git_dir.clone();
            let mut ops = self.ops();
            ops.branch = Some(branch.to_owned());
            ops.git_dir = Some(git_dir);
            ops
        }

        pub(crate) fn ops(&mut self) -> Ops<'_> {
            Ops {
                store: &self.store,
                index: &self.index,
                repo: Some(&self.repo_git),
                paths: &self.paths,
                branch: None,
                git_dir: None,
                ledger: &mut self.ledger,
                tree: &mut self.tree_entries,
                clock: &self.clock,
                compaction_threshold: self.compaction_threshold,
                case_insensitive: self.case_insensitive,
                staged: std::collections::BTreeMap::new(),
                pending_undo: None,
                lock: crate::ops::DEFAULT_LOCK,
            }
        }

        pub(crate) fn new(repo: &FixtureRepo, state: &TempDir) -> Self {
            let env = fixture_env(repo, state);
            let paths = RepoPaths::under(state.join("repo"));
            let repo_git = RepoGit::new(&env, repo.path());
            let config = repo_git.config_list().unwrap();
            let facts = RepoFacts::read(&repo_git, &config).unwrap();
            let (store, _) =
                Store::open(&env, repo.path(), RootKind::Git, &paths, Some(&facts)).unwrap();
            let exclude = repo_git.git_path("info/exclude").unwrap();
            let git_dir = repo_git
                .git_path("HEAD")
                .unwrap()
                .parent()
                .expect("a git dir above HEAD")
                .to_path_buf();
            let index = PrivateIndex::new(
                store.git().clone(),
                &paths,
                RootKind::Git,
                Some(exclude),
                Vec::new(),
            );
            let tree = Oid::parse(repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap().trim()).unwrap();
            let tree_entries = store.ls_tree(&tree).unwrap();
            let head_commit = Oid::parse(repo.git(&["rev-parse", "HEAD"]).unwrap().trim()).unwrap();
            let mut ledger = Ledger::new(
                repo.path(),
                RootKind::Git,
                Some(tree),
                SeenAt {
                    head_commit: None,
                    branch: None,
                    at: "2026-01-01T00:00:00Z".into(),
                },
            );
            // What `engine::first_sight` sets for a git root: the commit the root was first
            // sighted at, which R2's seen-state target asks about. The ops tests build their
            // ledger by hand, so without this every fixture would look like a state file
            // older than the field.
            ledger.first_sight_head = Some(head_commit);
            let mut b = globset::GlobSetBuilder::new();
            b.add(globset::Glob::new("**/Cargo.lock").unwrap());
            b.add(globset::Glob::new("Cargo.lock").unwrap());
            Self {
                case_insensitive: probe_case_insensitive(repo.path()),
                store,
                index,
                repo_git,
                ledger,
                tree_entries,
                globs: b.build().unwrap(),
                paths,
                clock: FixedClock::at_unix(1_800_000_000),
                compaction_threshold: 500,
                git_dir,
            }
        }

        /// Make the current disk state the seen tree.
        pub(crate) fn mark_seen(&mut self) {
            let seen = self
                .store
                .tree_of_disk(&DraftScope::tree(u64::MAX), &[])
                .unwrap();
            self.tree_entries = self.store.ls_tree(&seen).unwrap();
            self.ledger.seen_tree = Some(seen);
        }

        pub(crate) fn scan(&self) -> ScanOutput {
            self.scan_reusing(None)
        }

        /// [`Self::scan`] offered a last pile's renderings (what the engine passes, see
        /// [`reuse_of`]).
        pub(crate) fn scan_reusing(&self, reuse: Option<Reuse<'_>>) -> ScanOutput {
            let inputs = ScanInputs {
                store: &self.store,
                index: &self.index,
                repo: Some(&self.repo_git),
                ledger: &self.ledger,
                seen_tree: self.ledger.seen_tree.as_ref(),
                tree: &self.tree_entries,
                case_insensitive: self.case_insensitive,
                collapsed_globs: &self.globs,
                scope: None,
                collapse_size_bytes: 1024,
                excluded_dirs: &[],
                index_tmp: &self.paths.index_tmp,
                row_cap: crate::engine::DEFAULT_ROW_CAP,
                retry_on_reseed: false,
                skip: None,
                reuse,
            };
            super::scan(&inputs).unwrap()
        }
    }

    /// What the engine offers the next scan after `out`: its pile and its reusable paths.
    pub(crate) fn reuse_of(out: &ScanOutput) -> Reuse<'_> {
        Reuse {
            pile: &out.pile,
            rendered: &out.rendered,
        }
    }

    /// A limit raised past 4 GiB still collapses a pair one hunk's `u32` offsets could not
    /// hold, rather than panicking in `Lines::push` (code review F1).
    #[test]
    fn over_size_caps_a_pair_at_what_one_hunk_can_address() {
        let four_gib = u32::MAX as usize;
        let huge = u64::MAX;
        assert!(!over_size(10, 10, huge));
        assert!(!over_size(four_gib / 2, four_gib / 2, huge));
        assert!(over_size(four_gib / 2 + 1, four_gib / 2 + 1, huge));
        assert!(over_size(1024, 0, 1024));
        assert!(over_size(0, 1024, 1024));
        assert!(!over_size(1023, 1023, 1024));
    }

    /// `collapse_size_bytes` has read as at-or-above in `docs/config.md` since the key was
    /// added; the ladder compared it with `>`, so a file of exactly that size showed its
    /// hunks (design review F11). The boundary is the size itself.
    #[test]
    fn scan_a_file_of_exactly_collapse_size_bytes_collapses() {
        let repo = FixtureRepo::new("scan-ladder").unwrap();
        let state = TempDir::new("lc-scan-ladder");
        let h = Harness::new(&repo, &state);
        // The harness's limit is 1 KiB.
        repo.write("f1", "x".repeat(1023));
        let out = h.scan();
        assert_eq!(out.pile.rows.len(), 1);
        assert_eq!(
            out.pile.rows[0].collapsed, None,
            "one byte below the limit still shows its hunks"
        );
        repo.write("f1", "x".repeat(1024));
        let out = h.scan();
        assert_eq!(out.pile.rows.len(), 1);
        assert_eq!(
            out.pile.rows[0].collapsed,
            Some(Collapsed::Size),
            "at the limit the row collapses, as the config docs say"
        );
    }

    #[test]
    fn scan_rows_cover_every_change_kind() {
        let repo = FixtureRepo::new("scan").unwrap();
        let state = TempDir::new("lc-scan");
        let h = Harness::new(&repo, &state);
        assert!(h.scan().pile.is_empty(), "clean tree: empty pile");

        repo.write("f1", "changed\n");
        repo.remove("f2");
        repo.write("new.txt", "n\n");
        repo.chmod_x("f3", true);
        repo.write("bin.dat", b"\0\x01\x02");
        repo.write("Cargo.lock", "lock\n");
        repo.write("big.txt", "x".repeat(2000));
        repo.symlink("f1", "link");
        repo.git(&["init", "-q", "nested"]).unwrap();
        repo.write("nested/inner.txt", "i\n");
        let out = h.scan();
        assert_eq!(out.nested_repos, vec![b"nested".to_vec()]);
        let pile = &out.pile;
        assert_eq!(
            pile_lines(pile),
            vec![
                "Cargo.lock",
                "big.txt",
                "bin.dat",
                "f1",
                "f2",
                "f3",
                "link",
                "new.txt"
            ]
        );
        let f1 = pile.row(b"f1").unwrap();
        assert_eq!(f1.change, Change::Modified);
        assert_eq!(f1.added, 1);
        assert!(f1.deleted >= 1);
        assert_eq!(f1.hunks.len(), 1);
        assert_eq!(pile.row(b"f2").unwrap().change, Change::Deleted);
        assert!(pile.row(b"f2").unwrap().current.is_none());
        let f3 = pile.row(b"f3").unwrap();
        if h.store.filemode() {
            assert_eq!(f3.change, Change::Mode);
            assert_eq!(f3.hunks.len(), 1, "D1: a synthetic mode hunk");
            assert_eq!(f3.hunks[0].lines.line(0).1, b"mode 100644".to_vec());
            assert_eq!(f3.hunks[0].lines.line(1).1, b"mode 100755".to_vec());
        }
        assert_eq!(pile.row(b"new.txt").unwrap().change, Change::Added);
        assert_eq!(
            pile.row(b"bin.dat").unwrap().collapsed,
            Some(Collapsed::Binary)
        );
        assert_eq!(
            pile.row(b"Cargo.lock").unwrap().collapsed,
            Some(Collapsed::Glob)
        );
        assert_eq!(
            pile.row(b"big.txt").unwrap().collapsed,
            Some(Collapsed::Size)
        );
        assert!(pile.row(b"big.txt").unwrap().hunks.is_empty());
        assert_eq!(pile.row(b"big.txt").unwrap().added, 1);
        let link = pile.row(b"link").unwrap();
        assert_eq!(link.current.as_ref().unwrap().mode, Mode::Symlink);
        assert_eq!(
            link.hunks[0].lines.line(0).1,
            b"f1".to_vec(),
            "symlink diffs its target"
        );
        assert!(
            pile.row(b"nested/inner.txt").is_none(),
            "D9: nested repo excluded"
        );
    }

    #[test]
    fn scan_second_scan_without_changes_issues_zero_hashes() {
        let repo = FixtureRepo::new("scan-stat").unwrap();
        let state = TempDir::new("lc-scan");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "changed\n");
        repo.write("new.txt", "n\n");
        let first = h.scan();
        assert!(first.hash_calls >= 1);
        assert_eq!(pile_lines(&first.pile), vec!["f1", "new.txt"]);
        // Fold the pile into the seen tree with write_tree, the way accept-all does.
        let writes: Vec<TreeWrite> = first
            .pile
            .rows
            .iter()
            .map(|r| {
                let cur = r.current.clone().unwrap();
                TreeWrite::Set {
                    path: r.path.clone(),
                    mode: cur.mode,
                    oid: cur.oid,
                }
            })
            .collect();
        let seen = h
            .store
            .write_tree(h.ledger.seen_tree.as_ref(), &writes)
            .unwrap();
        h.tree_entries = h.store.ls_tree(&seen).unwrap();
        h.ledger.seen_tree = Some(seen);
        let second = h.scan();
        assert!(second.pile.is_empty());
        let third = h.scan();
        assert!(third.pile.is_empty());
        assert_eq!(third.hash_calls, 0, "stat cache: no hash-object calls");
    }

    #[test]
    fn scan_index_deleted_between_scans_yields_identical_pile() {
        let repo = FixtureRepo::new("scan-idx").unwrap();
        let state = TempDir::new("lc-scan");
        let h = Harness::new(&repo, &state);
        repo.write("f1", "changed\n");
        repo.remove("f3");
        repo.write("added.txt", "a\n");
        let a = h.scan().pile;
        std::fs::remove_file(&h.paths.index).unwrap();
        std::fs::remove_file(&h.paths.index_tree).unwrap();
        let b = h.scan().pile;
        assert_eq!(a, b);
        assert_eq!(pile_lines(&b), vec!["added.txt", "f1", "f3"]);
    }

    #[test]
    fn scan_ignores_restore_temp_files() {
        let repo = FixtureRepo::new("scan-restore-temp").unwrap();
        let state = TempDir::new("lc-scan");
        let h = Harness::new(&repo, &state);
        // A restore in flight in another lastcall process, and a ghost from one that was
        // killed between `create_new` and `rename` (F8). Neither is ever a row: a row is
        // something the user could accept, and these are lastcall's own scratch.
        repo.write(".f1.lastcall-restore-4242-0", "half-written\n");
        repo.write("d/.f2.lastcall-restore-4242-1", "half-written\n");
        // A file that merely looks similar is a real file and keeps its row.
        repo.write("f1.lastcall-restore-4242-0", "a real file\n");
        let pile = h.scan().pile;
        assert!(
            pile.row(b".f1.lastcall-restore-4242-0").is_none(),
            "{}",
            pile_lines(&pile).join(" ")
        );
        assert!(pile.row(b"d/.f2.lastcall-restore-4242-1").is_none());
        assert!(pile.row(b"f1.lastcall-restore-4242-0").is_some());
        assert_eq!(crate::ops::RESTORE_TEMP_GLOB, ".*.lastcall-restore-*");
    }

    #[test]
    fn scan_override_paths_are_candidates_and_absent_skip_worktree_is_not() {
        let repo = FixtureRepo::new("scan-ovr").unwrap();
        let state = TempDir::new("lc-scan");
        let mut h = Harness::new(&repo, &state);
        // A blob override on f1 with different content: pending although the worktree
        // equals the tree (stat-clean, so only the override rule reaches it).
        let other = h.store.hash_bytes(b"other\n").unwrap();
        h.ledger.overrides.insert(
            "f1".into(),
            Override {
                blob: Some(Some(other)),
                mode: Some(Mode::Regular),
                flags: vec![Flag::file("look", "t")],
                updated_at: "t".into(),
            },
        );
        // f2 edited and skip-worktree in the user's index: present, so it is a real edit
        // and is shown (over-show); once absent it is a sparse cone, not a deletion (D6).
        repo.write("f2", "edited\n");
        repo.git(&["update-index", "--skip-worktree", "f2"])
            .unwrap();
        let pile = h.scan().pile;
        assert_eq!(pile_lines(&pile), vec!["f1", "f2"]);
        let f1 = pile.row(b"f1").unwrap();
        assert_eq!(f1.change, Change::Modified);
        assert_eq!(f1.flags[0].note, "look");
        repo.remove("f2");
        assert_eq!(pile_lines(&h.scan().pile), vec!["f1"]);
    }

    #[test]
    fn scan_rename_pairs_delete_and_add() {
        let repo = FixtureRepo::new("scan-ren").unwrap();
        let state = TempDir::new("lc-scan");
        let mut h = Harness::new(&repo, &state);
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        repo.write("old.rs", &body);
        h.mark_seen();
        assert!(h.scan().pile.is_empty());
        repo.remove("old.rs");
        repo.write("new.rs", &body);
        let pile = h.scan().pile;
        assert_eq!(pile_lines(&pile), vec!["new.rs", "old.rs"]);
        assert!(matches!(
            pile.row(b"old.rs").unwrap().rename,
            Some(Rename::To { ref to, similarity }) if to == b"new.rs" && similarity == 100
        ));
        assert!(matches!(
            pile.row(b"new.rs").unwrap().rename,
            Some(Rename::From { ref from, .. }) if from == b"old.rs"
        ));
        assert!(!h.paths.index_tmp.exists(), "temp index unlinked");
    }

    #[test]
    fn scan_typechange_row_and_children() {
        let repo = FixtureRepo::new("scan-tc").unwrap();
        let state = TempDir::new("lc-scan");
        let h = Harness::new(&repo, &state);
        repo.remove("f1");
        std::fs::create_dir(repo.path().join("f1")).unwrap();
        repo.write("f1/inner", "x\n");
        let pile = h.scan().pile;
        let f1 = pile.row(b"f1").unwrap();
        assert_eq!(f1.change, Change::Typechange);
        assert!(f1.current.is_none() && f1.hunks.is_empty());
        assert!(pile.notices.iter().any(|n| n.contains("f1")));
        assert_eq!(pile.row(b"f1/inner").unwrap().change, Change::Added);
    }

    /// No diff counts this many lines: a row showing it was taken over from `sentinel`.
    const TAKEN: usize = 999_999;

    /// `pile` with every row's `added` set to [`TAKEN`].
    fn sentinel(pile: &Pile) -> Pile {
        let mut p = pile.clone();
        for r in p.rows_mut() {
            r.added = TAKEN;
        }
        p
    }

    fn taken(pile: &Pile, path: &str) -> bool {
        pile.row(path.as_bytes()).expect("a row").added == TAKEN
    }

    /// Phase 17 Part B fix 3: a row takes the last pile's rendering exactly when its path
    /// was rendered whole there and both of its sides (oid and mode) are the ones that
    /// pile's row had. The positive control takes every row; then one input per row moves
    /// and only the rows nothing moved under are taken.
    #[test]
    fn scan_reuse_takes_a_rendering_only_when_both_sides_hold() {
        let repo = FixtureRepo::new("scan-reuse").unwrap();
        let state = TempDir::new("lc-scan-reuse");
        let mut h = Harness::new(&repo, &state);
        repo.write("base.txt", "v1\n");
        h.mark_seen();
        repo.write("base.txt", "v2\n");
        repo.write("f1", "changed\n");
        repo.write("f2", "b2\n");
        repo.write("f3", "c2\n");
        repo.write("new.txt", "n\n");
        repo.write("Cargo.lock", "lock\n");
        let first = h.scan();
        let all = ["Cargo.lock", "base.txt", "f1", "f2", "f3", "new.txt"];
        assert_eq!(pile_lines(&first.pile), all);
        assert_eq!(first.rendered.len(), all.len(), "every row rendered whole");
        let prior = sentinel(&first.pile);

        let again = h.scan_reusing(Some(Reuse {
            pile: &prior,
            rendered: &first.rendered,
        }));
        for p in all {
            assert!(taken(&again.pile, p), "{p}: nothing moved");
        }
        assert!(Arc::ptr_eq(
            &again.pile.row(b"f1").unwrap().hunks,
            &first.pile.row(b"f1").unwrap().hunks
        ));
        assert_eq!(again.rendered, first.rendered);

        // The baseline oid: the seen tree records base.txt as v3; the disk still says v2.
        repo.write("base.txt", "v3\n");
        let v3 = h
            .scan()
            .pile
            .row(b"base.txt")
            .unwrap()
            .current
            .clone()
            .unwrap();
        repo.write("base.txt", "v2\n");
        let seen = h
            .store
            .write_tree(
                h.ledger.seen_tree.as_ref(),
                &[TreeWrite::Set {
                    path: b"base.txt".to_vec(),
                    mode: v3.mode,
                    oid: v3.oid,
                }],
            )
            .unwrap();
        h.tree_entries = h.store.ls_tree(&seen).unwrap();
        h.ledger.seen_tree = Some(seen);
        // The current oid, the current mode, and a path the last scan did not render whole.
        repo.write("f2", "b3\n");
        repo.chmod_x("f3", true);
        let mut rendered = first.rendered.clone();
        rendered.remove(b"new.txt".as_slice());
        let moved = h.scan_reusing(Some(Reuse {
            pile: &prior,
            rendered: &rendered,
        }));
        assert!(!taken(&moved.pile, "base.txt"), "the baseline moved");
        assert!(!taken(&moved.pile, "f2"), "the current oid moved");
        assert_eq!(
            taken(&moved.pile, "f3"),
            !h.store.filemode(),
            "the current mode moved (a change only where the store keeps the x bit)"
        );
        assert!(
            !taken(&moved.pile, "new.txt"),
            "not rendered whole last time"
        );
        assert!(taken(&moved.pile, "f1") && taken(&moved.pile, "Cargo.lock"));
        let fresh = h.scan();
        for p in ["base.txt", "f2", "f3", "new.txt"] {
            assert_eq!(
                moved.pile.row(p.as_bytes()),
                fresh.pile.row(p.as_bytes()),
                "{p}: rendered afresh, as a scan with nothing to reuse renders it"
            );
        }
    }

    /// A side missing from the store renders as empty with a notice: not a rendering to
    /// keep, so the path is left out of `rendered` and the next scan diffs it (and says so)
    /// again, while a whole row beside it is taken over. The resolver vets every baseline
    /// blob, so the side goes missing between that check and the fetch (a `git gc` racing
    /// the scan), which is [`DROP_FROM_BATCH`].
    #[test]
    fn scan_a_side_missing_from_the_store_is_never_taken_over() {
        let repo = FixtureRepo::new("scan-reuse-gone").unwrap();
        let state = TempDir::new("lc-scan-reuse-gone");
        let h = Harness::new(&repo, &state);
        repo.write("f1", "changed\n");
        repo.write("f2", "b2\n");
        let first = h.scan();
        let f2_base = first.pile.row(b"f2").unwrap().baseline.clone().unwrap().oid;
        assert_eq!(first.rendered.len(), 2);

        DROP_FROM_BATCH.with(|d| d.borrow_mut().push(f2_base));
        // Offered nothing for f2 to take, so its blobs are fetched, and one is gone.
        let mut rendered = first.rendered.clone();
        rendered.remove(b"f2".as_slice());
        let out = h.scan_reusing(Some(Reuse {
            pile: &sentinel(&first.pile),
            rendered: &rendered,
        }));
        let says = |o: &ScanOutput| {
            o.pile
                .notices
                .iter()
                .any(|n| n.starts_with("f2: baseline unreadable"))
        };
        assert!(says(&out), "{:?}", out.pile.notices);
        assert!(!out.rendered.contains(b"f2".as_slice()));
        assert!(
            out.rendered.contains(b"f1".as_slice()),
            "f1 taken over: still whole"
        );
        assert!(taken(&out.pile, "f1"));

        let next = h.scan_reusing(Some(Reuse {
            pile: &sentinel(&out.pile),
            rendered: &out.rendered,
        }));
        DROP_FROM_BATCH.with(|d| d.borrow_mut().clear());
        assert!(says(&next), "diffed again, so said again");
        assert!(!taken(&next.pile, "f2"));
        assert!(taken(&next.pile, "f1"));
    }

    /// A failed blob batch fails open every row, the ones the last pile could have given
    /// too, so a broken store shows what it shows with nothing to reuse; and a failed scan
    /// leaves nothing for the next one to take.
    #[test]
    fn scan_a_failed_blob_batch_fails_open_the_reusable_rows_too() {
        let repo = FixtureRepo::new("scan-reuse-fail").unwrap();
        let state = TempDir::new("lc-scan-reuse-fail");
        let h = Harness::new(&repo, &state);
        repo.write("f1", "changed\n");
        repo.write("f2", "b2\n");
        let first = h.scan();
        repo.write("f2", "b3\n");
        FAIL_BLOB_BATCH.with(|f| f.set(Some(0)));
        let failed = h.scan_reusing(Some(reuse_of(&first)));
        let failed_fresh = h.scan();
        FAIL_BLOB_BATCH.with(|f| f.set(None));
        assert_eq!(failed.pile, failed_fresh.pile);
        assert!(
            failed
                .pile
                .notices
                .iter()
                .any(|n| n.starts_with("blob contents unreadable"))
        );
        for r in failed.pile.rows.iter() {
            assert_eq!((r.added, r.deleted), (0, 0), "{r:?}");
            assert!(r.hunks.is_empty(), "{r:?}");
        }
        assert!(failed.rendered.is_empty());

        let next = h.scan_reusing(Some(reuse_of(&failed)));
        assert_eq!(next.pile, h.scan().pile);
        assert_eq!(next.pile.row(b"f1").unwrap().added, 1);
        assert_eq!(next.rendered.len(), 2);
    }

    /// Phase 17 Part B fix 4: rows whose blobs span several chunks render exactly as in
    /// one chunk (the rows, their order, the notices, the reusable paths) with one fetch
    /// per chunk; and a fetch failing in any later chunk fails open the whole scan as one
    /// failed batch does, the rows the earlier chunks rendered and their notices included.
    #[test]
    fn scan_blobs_in_several_chunks_render_as_one_chunk() {
        let repo = FixtureRepo::new("scan-chunks").unwrap();
        let state = TempDir::new("lc-scan-chunks");
        let h = Harness::new(&repo, &state);
        repo.write("f1", "changed\n");
        repo.write("f2", "b2\nmore\n");
        repo.write("f3", "\0bin");
        repo.write("Cargo.lock", "lock\n");
        repo.write("big.txt", "y\n".repeat(600));
        for (i, body) in ["one\n", "two\nlines\n", "three\n\n", "no newline"]
            .iter()
            .enumerate()
        {
            repo.write(&format!("new{i}.txt"), body);
        }
        // A side gone from the store, so a middle chunk says something.
        let f2_base = h
            .scan()
            .pile
            .row(b"f2")
            .unwrap()
            .baseline
            .clone()
            .unwrap()
            .oid;
        DROP_FROM_BATCH.with(|d| d.borrow_mut().push(f2_base));
        let scan_with = |budget: Option<u64>, fail: Option<usize>| {
            CHUNK_BYTES.with(|b| b.set(budget));
            FAIL_BLOB_BATCH.with(|f| f.set(fail));
            let before = BLOB_FETCHES.with(std::cell::Cell::get);
            let out = h.scan();
            CHUNK_BYTES.with(|b| b.set(None));
            FAIL_BLOB_BATCH.with(|f| f.set(None));
            (out, BLOB_FETCHES.with(std::cell::Cell::get) - before)
        };

        let (one, fetches) = scan_with(None, None);
        assert_eq!(fetches, 1);
        let rows = one.pile.rows.len();
        assert!(rows >= 8, "{:?}", one.pile.rows);
        assert!(
            one.pile
                .notices
                .iter()
                .any(|n| n.starts_with("f2: baseline unreadable")),
            "{:?}",
            one.pile.notices
        );
        let mut last = usize::MAX;
        for budget in [1, 8, 16, 32, 64, 1 << 20] {
            let (many, fetches) = scan_with(Some(budget), None);
            assert_eq!(many.pile, one.pile, "budget {budget}");
            assert_eq!(many.rendered, one.rendered, "budget {budget}");
            assert!(fetches <= last, "budget {budget}: {fetches} > {last}");
            last = fetches;
            if budget == 1 {
                assert_eq!(fetches, rows, "a row per chunk");
            }
        }
        assert_eq!(last, 1);

        let (failed, _) = scan_with(None, Some(0));
        assert!(failed.rendered.is_empty());
        assert_eq!(
            failed
                .pile
                .notices
                .iter()
                .filter(|n| n.contains("unreadable"))
                .collect::<Vec<_>>(),
            vec![
                "blob contents unreadable (store: injected batch failure); rows shown without counts or hunks"
            ]
        );
        for at in 0..rows {
            let (later, fetches) = scan_with(Some(1), Some(at));
            assert_eq!(fetches, at + 1, "stops at the failed chunk");
            assert_eq!(later.pile, failed.pile, "chunk {at} failed");
            assert!(later.rendered.is_empty());
        }
        DROP_FROM_BATCH.with(|d| d.borrow_mut().clear());
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: crate::env::proptest_cases(),
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        /// Text, more text, binary, over the harness's 1 KiB limit, no final newline, the
        /// committed `f3`: every length differs, so no rewrite hides behind a racy stat.
        const CONTENTS: [&[u8]; 6] = [
            b"a1\na2\nx\n",
            b"changed\n",
            b"\0bin",
            b"",
            b"no newline",
            b"c\n",
        ];
        const PATHS: [&str; 6] = ["f1", "f2", "f3", "new.txt", "Cargo.lock", "big.txt"];

        #[derive(Debug, Clone)]
        enum Step {
            Write(usize, usize),
            Remove(usize),
            Exec(usize, bool),
            MarkSeen,
            /// The seen tree records one path as `CONTENTS[c]`: its baseline moves alone.
            SeenAs(usize, usize),
        }

        fn step() -> impl Strategy<Value = Step> {
            prop_oneof![
                4 => (0..PATHS.len(), 0..CONTENTS.len()).prop_map(|(p, c)| Step::Write(p, c)),
                1 => (0..PATHS.len()).prop_map(Step::Remove),
                1 => (0..PATHS.len(), any::<bool>()).prop_map(|(p, x)| Step::Exec(p, x)),
                1 => Just(Step::MarkSeen),
                2 => (0..PATHS.len(), 0..CONTENTS.len()).prop_map(|(p, c)| Step::SeenAs(p, c)),
            ]
        }

        fn apply(repo: &FixtureRepo, h: &mut Harness, step: &Step) {
            let exists = |p: usize| repo.path().join(PATHS[p]).exists();
            match *step {
                Step::Write(p, 3) => {
                    repo.write(PATHS[p], "y\n".repeat(600));
                }
                Step::Write(p, c) => {
                    repo.write(PATHS[p], CONTENTS[c]);
                }
                Step::Remove(p) if exists(p) => repo.remove(PATHS[p]),
                Step::Exec(p, x) if exists(p) => repo.chmod_x(PATHS[p], x),
                Step::MarkSeen => h.mark_seen(),
                Step::SeenAs(p, c) => {
                    let oid = h.store.hash_bytes(CONTENTS[c]).unwrap();
                    let set = TreeWrite::Set {
                        path: PATHS[p].as_bytes().to_vec(),
                        mode: Mode::Regular,
                        oid,
                    };
                    let seen = h
                        .store
                        .write_tree(h.ledger.seen_tree.as_ref(), &[set])
                        .unwrap();
                    h.tree_entries = h.store.ls_tree(&seen).unwrap();
                    h.ledger.seen_tree = Some(seen);
                }
                Step::Remove(_) | Step::Exec(..) => {}
            }
        }

        /// Phase 17 Part B fixes 3 and 4: over any run of edits, a scan offered the last
        /// scan's renderings, and fetching its blobs in chunks of any budget, gives exactly
        /// the pile, and the reusable paths, of a scan with nothing to reuse and one chunk.
        /// Driven through `TestRunner` over one fixture: the property holds from whatever
        /// state the last case left, so nothing is reset.
        #[test]
        fn scan_reuse_changes_nothing_a_pile_shows() {
            let repo = FixtureRepo::new("scan-reuse-prop").unwrap();
            let state = TempDir::new("lc-scan-reuse-prop");
            let h = std::cell::RefCell::new(Harness::new(&repo, &state));
            let mut runner = proptest::test_runner::TestRunner::new(config());
            let budget = prop_oneof![Just(None), (1u64..1_400).prop_map(Some)];
            let rounds = prop::collection::vec((prop::collection::vec(step(), 0..4), budget), 1..4);
            let result = runner.run(&rounds, |rounds| {
                let mut h = h.borrow_mut();
                let mut last = h.scan();
                for (round, budget) in &rounds {
                    for s in round {
                        apply(&repo, &mut h, s);
                    }
                    CHUNK_BYTES.with(|b| b.set(*budget));
                    let reusing = h.scan_reusing(Some(reuse_of(&last)));
                    CHUNK_BYTES.with(|b| b.set(None));
                    let fresh = h.scan();
                    prop_assert_eq!(&reusing.pile, &fresh.pile);
                    prop_assert_eq!(&reusing.rendered, &fresh.rendered);
                    last = reusing;
                }
                Ok(())
            });
            if let Err(e) = result {
                panic!("{e}");
            }
        }
    }
}
