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

/// A size limit as a reader's line: whole KiB where it divides, bytes otherwise.
pub fn size_limit_label(bytes: u64) -> String {
    if bytes.is_multiple_of(1024) {
        format!("{} KiB", with_thousands((bytes / 1024) as usize))
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
    pub hunks: Vec<Hunk>,
    pub annotation: Option<Annotation>,
    pub conflicted: bool,
    pub collapsed: Option<Collapsed>,
    /// Every flag on the path, oldest first (Amendment v1.7). A file flag has `hunk: None`.
    /// `#[serde(default)]` so recorded-pile fixtures written before v1.7 still load.
    #[serde(default)]
    pub flags: Vec<Flag>,
    pub rename: Option<Rename>,
}

impl Row {
    pub fn path_lossy(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }
}

/// A derived group row (§6.7 "upstream · N files").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub kind: Annotation,
    pub paths: Vec<Vec<u8>>,
}

/// The pending set of one root after a scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Pile {
    /// Sorted by path bytes.
    pub rows: Vec<Row>,
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

    pub fn row(&self, path: &[u8]) -> Option<&Row> {
        self.rows.iter().find(|r| r.path == path)
    }

    /// The rows tagged `upstream`, as one group (empty when none).
    pub fn groups(&self) -> Vec<Group> {
        let paths: Vec<Vec<u8>> = self
            .rows
            .iter()
            .filter(|r| r.annotation == Some(Annotation::Upstream))
            .map(|r| r.path.clone())
            .collect();
        if paths.is_empty() {
            Vec::new()
        } else {
            vec![Group {
                kind: Annotation::Upstream,
                paths,
            }]
        }
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

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE)].contains(&0)
}

/// Run one scan.
pub fn scan(inputs: &ScanInputs<'_>) -> Result<ScanOutput, ScanError> {
    let store = inputs.store;
    let root = store.root();
    let mut notices: Vec<String> = Vec::new();
    let calls_before = store.git().hash_object_calls();

    // 1. Index refresh and enumeration.
    inputs.index.ensure(inputs.seen_tree)?;
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
            inputs.index.seed(inputs.seen_tree)?;
            inputs.index.diff_files()?
        }
    };
    let others = inputs.index.others(inputs.scope)?;
    let mut nested_repos: Vec<Vec<u8>> = others
        .iter()
        .filter_map(|o| match o {
            Other::NestedRepo(d) => Some(d.clone()),
            Other::File(_) => None,
        })
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
                if !scope.admits_shape(&p) || under_excluded(&p, inputs.excluded_dirs) {
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
        for (path, current) in paths.iter().zip(currents) {
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
                    hunks: Vec::new(),
                    annotation: None,
                    conflicted: is_conflicted,
                    collapsed: Some(Collapsed::Unread { over_bytes }),
                    flags,
                    rename: None,
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
                hunks: Vec::new(),
                annotation: None,
                conflicted: is_conflicted,
                collapsed: None,
                flags,
                rename: None,
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

    // 4b. Content for every row through one `cat-file --batch`: rendering must not cost a
    // process per row (2,000 unseen files are 2,000 rows on every rescan tick).
    let mut wanted: Vec<Oid> = rows
        .iter()
        .filter(|r| !matches!(r.change, Change::Typechange | Change::Unreadable))
        // An unread row has no content to diff, and its recorded blob may itself be large.
        .filter(|r| !matches!(r.collapsed, Some(Collapsed::Unread { .. })))
        .flat_map(|r| [&r.baseline, &r.current])
        .flatten()
        .map(|e| e.oid.clone())
        .collect();
    wanted.sort();
    wanted.dedup();
    let blobs = match store.cat_blobs(&wanted) {
        Ok(b) => Some(b),
        Err(e) => {
            notices.push(format!(
                "blob contents unreadable ({e}); rows shown without counts or hunks"
            ));
            None
        }
    };
    for row in &mut rows {
        render_content(store, inputs, blobs.as_ref(), row, &mut notices)?;
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
            rows,
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
    })
}

/// Hunks, counts and the collapse decision for one row, from the batch-fetched blobs
/// (`None`: the batch failed; the row keeps its change kind and nothing else).
fn render_content(
    store: &Store,
    inputs: &ScanInputs<'_>,
    blobs: Option<&HashMap<Oid, Vec<u8>>>,
    row: &mut Row,
    notices: &mut Vec<String>,
) -> Result<(), ScanError> {
    // An unread row already carries its verdict, and it is the truer one.
    if matches!(row.collapsed, Some(Collapsed::Unread { .. })) {
        return Ok(());
    }
    if inputs
        .collapsed_globs
        .is_match(Path::new(OsStr::from_bytes(&row.path)))
    {
        row.collapsed = Some(Collapsed::Glob);
    }
    if matches!(row.change, Change::Typechange | Change::Unreadable) {
        return Ok(());
    }
    let Some(blobs) = blobs else {
        return Ok(());
    };
    let mut read = |e: &Option<Entry>, what: &str| -> Vec<u8> {
        match e {
            Some(entry) => match blobs.get(&entry.oid) {
                Some(b) => b.clone(),
                None => {
                    notices.push(format!(
                        "{}: {what} unreadable (object {} not in the store)",
                        row.path_lossy(),
                        entry.oid
                    ));
                    Vec::new()
                }
            },
            None => Vec::new(),
        }
    };
    let old = read(&row.baseline, "baseline");
    let new = read(&row.current, "current");
    let same_content = row
        .baseline
        .as_ref()
        .zip(row.current.as_ref())
        .is_some_and(|(b, c)| b.oid == c.oid);
    if !same_content {
        let (a, d) = hunks::counts(&old, &new);
        row.added = a;
        row.deleted = d;
    }
    if row.collapsed.is_none() {
        if is_binary(&old) || is_binary(&new) {
            row.collapsed = Some(Collapsed::Binary);
        } else if old.len() as u64 >= inputs.collapse_size_bytes
            || new.len() as u64 >= inputs.collapse_size_bytes
        {
            row.collapsed = Some(Collapsed::Size);
        }
    }
    if row.collapsed.is_some() {
        return Ok(());
    }
    if !same_content {
        row.hunks = hunks::diff(&old, &new);
    }
    if let (Some(b), Some(c)) = (&row.baseline, &row.current)
        && b.mode != c.mode
        && (store.filemode() || b.mode == Mode::Symlink || c.mode == Mode::Symlink)
    {
        let mut h = Hunk::mode_change(b.mode.as_str(), c.mode.as_str());
        h.index = row.hunks.len();
        row.hunks.push(h);
        if same_content {
            row.added = 1;
            row.deleted = 1;
        }
    }
    Ok(())
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
            let index = PrivateIndex::new(store.git().clone(), &paths, RootKind::Draft, None);
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
            })
            .unwrap()
        }
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
            let index =
                PrivateIndex::new(store.git().clone(), &paths, RootKind::Git, Some(exclude));
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
            };
            super::scan(&inputs).unwrap()
        }
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
            assert_eq!(f3.hunks[0].lines[0].1, b"mode 100644".to_vec());
            assert_eq!(f3.hunks[0].lines[1].1, b"mode 100755".to_vec());
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
            link.hunks[0].lines[0].1,
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
}
