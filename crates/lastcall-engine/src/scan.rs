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

use crate::git::{self, GitError, Mode, Oid, RepoGit};
use crate::hunks::{self, Hunk};
use crate::index::{IndexError, Other, PrivateIndex};
use crate::ledger::{Baseline, BaselineResolver, Flag, Ledger, TreeEntries};
use crate::store::{Current, Store, StoreError};

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
    pub flag: Option<Flag>,
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
    /// Root-relative directories owned by other roots (draft roots inside this one); only
    /// `others` entries beneath them are excluded.
    pub excluded_dirs: &'a [Vec<u8>],
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
    let others = inputs.index.others()?;
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
            && !under_any(p, inputs.excluded_dirs)
        {
            candidates.insert(p.clone());
        }
    }
    // D6: a skip-worktree path is excluded only while it is absent from the worktree (the
    // sparse cone); one that is present is a real file and stays a candidate. The filter
    // applies to every candidate whichever list it came from: an absent cone path is a
    // cone even when an override or `diff-files` names it.
    let root_dir = inputs.store.root();
    let candidates: Vec<Vec<u8>> = candidates
        .into_iter()
        .filter(|p| {
            !skip.contains(p)
                || root_dir
                    .join(OsStr::from_bytes(p))
                    .symlink_metadata()
                    .is_ok()
        })
        .collect();

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
    let mut materialise = |paths: &[Vec<u8>], rows: &mut Vec<Row>| {
        let currents = store.hash_paths(paths);
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
            let flag = std::str::from_utf8(path)
                .ok()
                .and_then(|s| inputs.ledger.overrides.get(s))
                .and_then(|o| o.flag.clone());
            let is_conflicted = conflicted.contains(path);
            let lossy = String::from_utf8_lossy(path).into_owned();
            if std::str::from_utf8(path).is_err() {
                notices.push(format!(
                    "{lossy}: non-UTF-8 path; shown pending, accept is refused in v1"
                ));
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
                flag,
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

    // 5. D5 rename pairing: only when the pile has both deletions and additions.
    let has_del = rows.iter().any(|r| r.change == Change::Deleted);
    let added: Vec<Vec<u8>> = rows
        .iter()
        .filter(|r| r.change == Change::Added && r.current.is_some())
        .map(|r| r.path.clone())
        .collect();
    if has_del && !added.is_empty() {
        match detect_renames(store, inputs.index, inputs.index_tmp, &added) {
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
    Ok(ScanOutput {
        pile: Pile {
            rows,
            notices,
            omitted,
        },
        nested_repos,
        hash_calls: store.git().hash_object_calls() - calls_before,
        refreshed,
    })
}

/// Hunks, counts and the collapse decision for one row, from the batch-fetched blobs
/// (`None`: the batch failed; the row keeps its change kind and nothing else).
/// `10000` → `10,000`: the row-cap notice quotes numbers the way the ruling spells them.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn render_content(
    store: &Store,
    inputs: &ScanInputs<'_>,
    blobs: Option<&HashMap<Oid, Vec<u8>>>,
    row: &mut Row,
    notices: &mut Vec<String>,
) -> Result<(), ScanError> {
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
        } else if old.len() as u64 > inputs.collapse_size_bytes
            || new.len() as u64 > inputs.collapse_size_bytes
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

/// D5: a temp index that is a **copy of the refreshed private index**, `add -N` the added
/// paths, then `diff -M -z --name-status`. Returns `(from, to, similarity)`.
fn detect_renames(
    store: &Store,
    index: &PrivateIndex,
    index_tmp: &Path,
    added: &[Vec<u8>],
) -> Result<Vec<RenamePair>, ScanError> {
    let _ = std::fs::remove_file(index_tmp);
    std::fs::copy(index.path(), index_tmp).map_err(|e| StoreError::Io {
        path: index_tmp.to_path_buf(),
        source: e,
    })?;
    let result = (|| -> Result<Vec<RenamePair>, ScanError> {
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
    #[test]
    fn scan_with_thousands_groups_digits_like_the_ruling() {
        for (n, want) in [
            (0, "0"),
            (3, "3"),
            (999, "999"),
            (1000, "1,000"),
            (10_000, "10,000"),
            (49_997, "49,997"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(super::with_thousands(n), want);
        }
    }

    use super::*;

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
    use crate::store::{RootKind, TreeWrite};
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
    }

    impl Harness {
        pub(crate) fn ops(&mut self) -> Ops<'_> {
            Ops {
                store: &self.store,
                index: &self.index,
                repo: Some(&self.repo_git),
                paths: &self.paths,
                ledger: &mut self.ledger,
                tree: &mut self.tree_entries,
                clock: &self.clock,
                compaction_threshold: self.compaction_threshold,
                staged: std::collections::BTreeMap::new(),
            }
        }

        pub(crate) fn new(repo: &FixtureRepo, state: &TempDir) -> Self {
            let env = fixture_env(repo, state);
            let paths = RepoPaths::under(state.join("repo"));
            let repo_git = RepoGit::new(&env, repo.path());
            let (store, _) =
                Store::open(&env, repo.path(), RootKind::Git, &paths, Some(&repo_git)).unwrap();
            let exclude = repo_git.git_path("info/exclude").unwrap();
            let index =
                PrivateIndex::new(store.git().clone(), &paths, RootKind::Git, Some(exclude));
            let tree = Oid::parse(repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap().trim()).unwrap();
            let tree_entries = store.ls_tree(&tree).unwrap();
            let ledger = Ledger::new(
                repo.path(),
                RootKind::Git,
                Some(tree),
                SeenAt {
                    head_commit: None,
                    branch: None,
                    at: "2026-01-01T00:00:00Z".into(),
                },
            );
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
            }
        }

        /// Make the current disk state the seen tree.
        pub(crate) fn mark_seen(&mut self) {
            let seen = self.store.tree_of_disk().unwrap();
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
                collapse_size_bytes: 1024,
                excluded_dirs: &[],
                index_tmp: &self.paths.index_tmp,
                row_cap: crate::engine::DEFAULT_ROW_CAP,
            };
            super::scan(&inputs).unwrap()
        }
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
                flag: Some(Flag {
                    note: "look".into(),
                    created_at: "t".into(),
                }),
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
        assert_eq!(f1.flag.as_ref().unwrap().note, "look");
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
