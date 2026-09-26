//! Root discovery and rescan (docs/spec/00-spec.md §6.5 last paragraph, §6.4 "Nested
//! git"/"Worktrees"; kickoff deliverable 10).
//!
//! For each resolved parent dir `P`: if `P` is inside a git repository that repository is a
//! root; otherwise every immediate child directory of `P` with a `.git` entry (directory or
//! file — a linked worktree's `.git` is a file) is a git root. Draft roots come from
//! `draft_dirs`: an absolute entry names a directory; a relative entry is a glob matched
//! against directories relative to each parent dir **and** relative to each discovered git
//! root. Nested repositories reported by a scan (`others()` → `dir/`) become git roots with
//! `badge: nested_in`. Roots are never forgotten by the state dir: a removed root keeps its
//! ledger.
//!
//! A watched folder carries three things discovery decides once (Amendment v1.13):
//!
//! - its **scope**: the folder on its own, or the tree below it when the entry ends `/**`,
//!   and either way the size at which it stops reading a file;
//! - its **name**: its path below the deepest base that contains it, prefixed with the
//!   last `draft_dir_parents` folders of that base, so two projects with a scratch folder
//!   of the same name never read as one row, whatever order the entries are written in;
//! - the folders another root looks after, so the two never list the same file.
//!
//! The walk that finds them reads exactly as many folder levels as an entry names, and
//! only a `**` component reads to the ceiling.
//!
//! `search_depth` (Amendment v1.11) switches on two further mechanisms, both at `2` and
//! above and neither at `1`:
//!
//! 1. **The plain-folder walk.** Below each parent, a level-1 child that is not a
//!    repository and is not a `WALK_SKIP` name is read, and so on down to level N: a
//!    directory with a `.git` entry is a root, a directory without one is read at the next
//!    level. The walk never enters a repository, so a submodule, a vendored repository or a
//!    test fixture inside a watched repository is never listed by it, and beyond level 1 it
//!    never follows a symlink.
//! 2. **Worktrees kept inside a repository.** One `worktree list --porcelain` per root
//!    whose `.git` is a directory; every linked worktree whose canonical path lies inside
//!    that root becomes a root too, badged `worktree_of`. This is what makes
//!    `R/.worktrees/wt` a row without walking inside `R`, from any launch directory.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};

use crate::env::Env;
use crate::git::RepoGit;
use crate::store::{DraftScope, ExcludedDir, RootKind};

/// How a root relates to another (status JSON `badge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Badge {
    /// A linked worktree of the main worktree at this path.
    WorktreeOf(PathBuf),
    /// A repository nested inside the git root at this path.
    NestedIn(PathBuf),
}

/// One discovered root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRoot {
    /// Canonical absolute path.
    pub path: PathBuf,
    pub kind: RootKind,
    /// The parent dir the state layout files this root under: the configured parent that
    /// contains it, else its own parent directory.
    pub parent: PathBuf,
    pub badge: Option<Badge>,
    /// `Some` for a watched folder: how much of it this root covers and the size at which
    /// it stops reading. `None` for a repository.
    pub scope: Option<DraftScope>,
    /// The name every surface shows for this root: a repository's own folder name, and a
    /// watched folder's name with as many folders above it as `draft_dir_parents` asks
    /// for, so two projects with a scratch folder of the same name never read as one row.
    pub name: String,
    /// Folders inside this root that another root looks after, with whether the whole tree
    /// below each belongs there. A repository hands over every watched folder inside it; a
    /// folder watched with its whole tree hands over the ones inside that; a folder watched
    /// on its own hands over nothing, because a subfolder is already outside what it covers.
    pub excluded_dirs: Vec<ExcludedDir>,
}

/// A repository root, named by its own folder.
fn git_root(path: PathBuf, parent: PathBuf, badge: Option<Badge>) -> DiscoveredRoot {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    DiscoveredRoot {
        path,
        kind: RootKind::Git,
        parent,
        badge,
        scope: None,
        name,
        excluded_dirs: Vec::new(),
    }
}

/// The result of one discovery pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Sorted by path.
    pub roots: Vec<DiscoveredRoot>,
    pub notices: Vec<String>,
}

impl Discovery {
    pub fn paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.path.clone()).collect()
    }

    pub fn get(&self, path: &Path) -> Option<&DiscoveredRoot> {
        self.roots.iter().find(|r| r.path == path)
    }

    /// Watched folders that lie inside `root`, as root-relative byte paths with whether
    /// the whole tree below each belongs to them (the enclosing root's `others` entries
    /// beneath them belong to the watched folder, not to it).
    pub fn excluded_inside(&self, root: &Path) -> Vec<ExcludedDir> {
        use std::os::unix::ffi::OsStrExt;
        self.roots
            .iter()
            .filter(|r| r.kind == RootKind::Draft)
            .filter_map(|r| {
                let rel = r.path.strip_prefix(root).ok()?;
                (!rel.as_os_str().is_empty()).then(|| {
                    (
                        rel.as_os_str().as_bytes().to_vec(),
                        r.scope.as_ref().is_some_and(|s| s.recursive),
                    )
                })
            })
            .collect()
    }
}

/// What changed between two discovery passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootsChanged {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    /// Set on the one `RootsChanged` the watcher emits for a config reload (Phase 14 D),
    /// which it emits even when both lists are empty: the TUI posts the reload's notice when
    /// this lands, with the counts it carries. `false` from [`diff`] and every other pass.
    pub reload: bool,
}

impl RootsChanged {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Inputs to a discovery pass.
pub struct DiscoverInputs<'a> {
    pub env: &'a Env,
    /// Resolved (normalized) parent dirs.
    pub parent_dirs: &'a [PathBuf],
    /// `Config::draft_dirs`.
    pub draft_dirs: &'a [String],
    /// Nested repositories reported by scans: `(outer git root, absolute nested dir)`.
    pub nested: &'a [(PathBuf, PathBuf)],
    /// `Config::search_depth`: how many folders below each parent dir the plain-folder walk
    /// reads, and the switch on the inside-a-repository worktree listing. `1` is level 1
    /// alone, which is what every release before Amendment v1.11 did.
    pub search_depth: u8,
    /// `Config::collapse_size_bytes`: the size at which a watched folder stops reading a
    /// file, carried on each watched folder's scope.
    pub collapse_size_bytes: u64,
    /// `Config::draft_dir_parents`: how many folders above a watched folder its name
    /// carries (Amendment v1.13).
    pub draft_dir_parents: u8,
    /// `Config::skip_globs` (Amendment v1.15): a repository or a watched folder whose path
    /// relative to the parent dir it is filed under matches one is never a root, and the
    /// walk never enters a plain folder that matches.
    pub skip_globs: &'a [String],
}

/// The `skip_globs` test discovery runs on every candidate directory, **before** anything
/// about it is read: no `.git` probe, no `rev-parse`, so a skipped repository costs no
/// spawn and a skipped tree of clones is never walked (Amendment v1.15).
struct Skip<'a> {
    set: globset::GlobSet,
    parents: &'a [PathBuf],
    /// Never skipped whatever matches: the parent dirs themselves and the launch
    /// directory. A root that **is** or **contains** one of these stays.
    protected: Vec<PathBuf>,
}

impl<'a> Skip<'a> {
    fn new(entries: &[String], parents: &'a [PathBuf], launch: &Path) -> Self {
        let mut protected = parents.to_vec();
        protected.push(std::fs::canonicalize(launch).unwrap_or_else(|_| launch.to_path_buf()));
        Skip {
            set: crate::config::skip_set(entries),
            parents,
            protected,
        }
    }

    /// Whether `rel` (a folder's relative path) matches: itself or any folder above it
    /// below the parent dir, each tried as `dir` and as `dir/` (the second form is what
    /// lets `evals/**` prune the folder `evals` itself). The folders above matter for what
    /// the walk never reaches on its own, a nested repository a scan reported or a
    /// worktree kept inside a repository: a pattern naming a folder drops everything found
    /// under it, exactly as `scan::DraftSkip::skips` reads it inside a watched folder.
    fn matches_dir(&self, rel: &Path) -> bool {
        let mut so_far = PathBuf::new();
        for component in rel.components() {
            so_far.push(component);
            if self.set.is_match(&so_far) {
                return true;
            }
            let mut with_slash = so_far.as_os_str().to_os_string();
            with_slash.push("/");
            if self.set.is_match(Path::new(&with_slash)) {
                return true;
            }
        }
        false
    }

    /// Whether discovery leaves `dir` out. A protected directory never is; when a pattern
    /// would have matched it (against its own parent directory, since relative to the
    /// parent dir it *is* the base), `notices` says the pattern was ignored for it.
    fn skips(&self, dir: &Path, notices: &mut Vec<String>) -> bool {
        if self.set.is_empty() {
            return false;
        }
        if self.protected.iter().any(|p| p.starts_with(dir)) {
            let own = dir.file_name().map(Path::new).unwrap_or(Path::new(""));
            let filed = file_under(self.parents, dir);
            let rel = dir.strip_prefix(&filed).unwrap_or(Path::new(""));
            if self.matches_dir(own) || self.matches_dir(rel) {
                let notice = format!(
                    "skip_globs: {} is a parent dir or the launch directory, so the \
                     pattern is ignored for it",
                    dir.display()
                );
                if !notices.contains(&notice) {
                    notices.push(notice);
                }
            }
            return false;
        }
        let filed = file_under(self.parents, dir);
        dir.strip_prefix(&filed)
            .is_ok_and(|rel| self.matches_dir(rel))
    }
}

const WALK_DEPTH: usize = crate::config::MAX_SEARCH_DEPTH as usize;
const WALK_SKIP: &[&str] = &[".git", "node_modules", "target", ".venv", "vendor"];

/// Whether a directory holds a `.git` entry, of either shape: a directory in a repository's
/// main worktree, a file in a linked worktree or a submodule.
fn has_git_entry(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Run one discovery pass.
pub fn discover(inputs: &DiscoverInputs<'_>) -> Discovery {
    let mut roots: BTreeMap<PathBuf, DiscoveredRoot> = BTreeMap::new();
    let mut notices = Vec::new();
    let parents: Vec<PathBuf> = inputs
        .parent_dirs
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let parent_of = |path: &Path| -> PathBuf { file_under(&parents, path) };
    let depth = inputs.search_depth.clamp(1, WALK_DEPTH as u8);
    let skip = Skip::new(inputs.skip_globs, &parents, inputs.env.cwd());
    // How many candidates `skip_globs` dropped under each parent dir, so a parent left
    // with nothing can say why instead of showing an empty screen.
    let mut skipped_under: Vec<(PathBuf, usize)> = Vec::new();

    // Git roots under each parent.
    for p in &parents {
        if !p.is_dir() {
            notices.push(format!("parent dir {} does not exist", p.display()));
            continue;
        }
        if let Some(top) = toplevel(inputs.env, p) {
            // A parent inside a repository is that repository: protected, never skipped.
            skip.skips(&top, &mut notices);
            roots
                .entry(top.clone())
                .or_insert_with(|| git_root(top.clone(), parent_of(&top), None));
            // A parent inside a repository *is* that repository, and nothing is walked
            // under it: level 1 is what it always was. Its own linked worktrees still
            // arrive through mechanism 2 below.
            continue;
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            notices.push(format!("cannot list parent dir {}", p.display()));
            continue;
        };
        // Level 1 is exactly today's rule, symlinks included: `is_dir` follows, so a
        // symlink to a repository beside the others is a root, keyed by its canonical
        // path. `real` is the un-followed answer, which is what the walk below descends
        // on — a symlink is never walked through, at any level.
        let mut children: Vec<(PathBuf, bool)> = rd
            .filter_map(Result::ok)
            .filter_map(|e| {
                let path = e.path();
                if !path.is_dir() {
                    return None;
                }
                Some((path, e.file_type().is_ok_and(|t| t.is_dir())))
            })
            .collect();
        children.sort();
        // The skip decides first: a matching child is neither probed nor walked.
        let before = children.len();
        children.retain(|(child, _)| !skip.skips(child, &mut notices));
        let slot = skipped_under.len();
        skipped_under.push((p.clone(), before - children.len()));
        for (child, _) in &children {
            if has_git_entry(child) {
                record_git_root(inputs.env, &parents, &mut roots, &mut notices, child);
            }
        }
        if depth < 2 {
            continue;
        }
        // Levels 2 to N, plain folders only: a directory that is a repository is a root
        // and is never entered, so no submodule, vendored repository or test fixture
        // inside one is ever listed by this walk, and `.git` (a `WALK_SKIP` name) is
        // never read at all.
        let mut stack: Vec<(PathBuf, u8)> = children
            .iter()
            .filter(|(c, real)| *real && !has_git_entry(c) && !is_skipped(c))
            .map(|(c, _)| (c.clone(), 1))
            .collect();
        while let Some((dir, level)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<PathBuf> = rd
                .filter_map(Result::ok)
                // The draft walk's own rule: `file_type` does not follow, so a symlink is
                // neither a root nor a way further down.
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            entries.sort();
            for entry in entries {
                if skip.skips(&entry, &mut notices) {
                    skipped_under[slot].1 += 1;
                    continue;
                }
                if has_git_entry(&entry) {
                    record_git_root(inputs.env, &parents, &mut roots, &mut notices, &entry);
                } else if !is_skipped(&entry) && level + 1 < depth {
                    stack.push((entry, level + 1));
                }
            }
        }
    }

    // Nested repositories reported by scans.
    for (outer, dir) in inputs.nested {
        let Ok(canon) = std::fs::canonicalize(dir) else {
            continue;
        };
        if roots.contains_key(&canon) || skip.skips(&canon, &mut notices) {
            continue;
        }
        if toplevel(inputs.env, &canon).as_deref() != Some(canon.as_path()) {
            continue;
        }
        let parent = parent_of(&canon);
        roots.insert(
            canon.clone(),
            git_root(canon, parent, Some(Badge::NestedIn(outer.clone()))),
        );
    }

    // Mechanism 2: the linked worktrees a repository keeps **inside** itself
    // (`R/.worktrees/<name>`), which the plain-folder walk above will never reach because
    // it does not enter a repository. One `worktree list` per root whose `.git` is a
    // directory; a linked worktree's list is its main worktree's, so it is skipped.
    if depth >= 2 {
        let listed: Vec<PathBuf> = roots
            .values()
            .filter(|r| r.kind == RootKind::Git && r.path.join(".git").is_dir())
            .map(|r| r.path.clone())
            .collect();
        for main in listed {
            // Filed where its repository is filed, so the two rows sit under one parent
            // even when the launch directory is deep inside the repository (there the
            // repository falls back to its own parent, and the worktree follows it).
            let parent = roots[&main].parent.clone();
            for path in worktrees_inside(inputs.env, &main) {
                if skip.skips(&path, &mut notices) {
                    continue;
                }
                roots.entry(path.clone()).or_insert_with(|| {
                    git_root(path, parent.clone(), Some(Badge::WorktreeOf(main.clone())))
                });
            }
        }
    }

    // Whatever found a root, a matching one is not listed: the checks above keep the walk
    // and the promotion from spawning for one, and this pass is the backstop for the rest.
    roots.retain(|path, _| !skip.skips(path, &mut notices));

    // Linked-worktree badges.
    for root in roots.values_mut() {
        let rg = RepoGit::new(inputs.env, &root.path);
        if let Some(main) = main_worktree_if_linked(&rg) {
            root.badge = Some(Badge::WorktreeOf(main));
        }
    }

    // Watched folders. Every entry is collected first and the folders are recorded
    // afterwards, so a folder two entries both name gets one row with the wider of the two
    // scopes — and its name never depends on which entry the config happens to list first.
    let mut bases: Vec<PathBuf> = parents.clone();
    bases.extend(roots.keys().cloned());
    let mut watched: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for entry in inputs.draft_dirs {
        let pattern = crate::config::draft_entry_pattern(entry);
        let recursive = crate::config::draft_entry_is_recursive(entry);
        if Path::new(pattern).is_absolute() {
            let path = Path::new(pattern);
            if let Ok(canon) = std::fs::canonicalize(path)
                && canon.is_dir()
            {
                let wider = watched.entry(canon).or_insert(false);
                *wider |= recursive;
            } else {
                notices.push(format!("draft dir {} does not exist", path.display()));
            }
            continue;
        }
        let matcher = match draft_matcher(pattern) {
            Ok(m) => m,
            Err(e) => {
                notices.push(format!(
                    "draft_dirs entry {entry:?} is not a valid glob: {e}"
                ));
                continue;
            }
        };
        // As many folder levels as the entry names, no more: naming `notes` reads one
        // level below each base, and only a `**` component reads to the ceiling.
        let walk = crate::config::draft_entry_walk_depth(entry);
        for base in &bases {
            for dir in matching_dirs(base, &matcher, walk, &|d| skip.skips(d, &mut Vec::new())) {
                let wider = watched.entry(dir).or_insert(false);
                *wider |= recursive;
            }
        }
    }
    for (dir, recursive) in watched {
        if skip.skips(&dir, &mut notices) {
            // A watched folder whose own path matches is not a root.
            continue;
        }
        if roots.contains_key(&dir) {
            // A repository is already looking after it; its own rules win.
            continue;
        }
        let parent = parent_of(&dir);
        let name = watched_name(&bases, &dir, inputs.draft_dir_parents);
        roots.insert(
            dir.clone(),
            DiscoveredRoot {
                path: dir,
                kind: RootKind::Draft,
                parent,
                badge: None,
                scope: Some(DraftScope {
                    recursive,
                    max_bytes: inputs.collapse_size_bytes,
                }),
                name,
                excluded_dirs: Vec::new(),
            },
        );
    }

    // Sorted by path bytes (the status JSON order), not component-wise.
    let mut roots: Vec<DiscoveredRoot> = roots.into_values().collect();
    roots.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));

    // Who looks after what. A repository hands over every watched folder inside it, and a
    // folder watched with its whole tree hands over the ones inside that; a folder watched
    // on its own hands over nothing, since a subfolder is already outside what it covers.
    let claims: Vec<(PathBuf, bool)> = roots
        .iter()
        .filter(|r| r.kind == RootKind::Draft)
        .map(|r| {
            (
                r.path.clone(),
                r.scope.as_ref().is_some_and(|s| s.recursive),
            )
        })
        .collect();
    for root in &mut roots {
        let hands_over = match &root.scope {
            None => true,
            Some(scope) => scope.recursive,
        };
        if !hands_over {
            continue;
        }
        root.excluded_dirs = claims
            .iter()
            .filter(|(path, _)| *path != root.path && path.starts_with(&root.path))
            .filter_map(|(path, recursive)| {
                let rel = path.strip_prefix(&root.path).ok()?;
                let bytes = rel.as_os_str().as_bytes().to_vec();
                (!bytes.is_empty()).then_some((bytes, *recursive))
            })
            .collect();
    }
    // A parent dir whose every candidate was skipped lists nothing, and the screen would
    // be empty with nothing saying why: one notice per such parent, with the count.
    for (parent, skipped) in skipped_under {
        if skipped > 0 && !roots.iter().any(|r| r.parent == parent) {
            notices.push(format!(
                "skip_globs: nothing left under {} ({} skipped)",
                parent.display(),
                skipped
            ));
        }
    }

    Discovery { roots, notices }
}

/// The name a watched folder shows: its path below the **deepest** base that contains it
/// (a parent dir, or a repository discovery found), prefixed with the last `parents`
/// folder names of that base and joined with `/`.
///
/// Reading the name off the deepest containing base, rather than off whichever entry
/// happened to match it, is what keeps the name the same however the entries and the
/// parent dirs are ordered. A folder no base contains is named by its own last
/// `1 + parents` folders, and one nearer the filesystem root than that shows what exists.
fn watched_name(bases: &[PathBuf], dir: &Path, parents: u8) -> String {
    use std::path::Component;
    let parents = usize::from(parents);
    let normal = |c: Component<'_>| match c {
        Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
        _ => None,
    };
    let last = |names: &[String], n: usize| -> Vec<String> {
        names[names.len().saturating_sub(n)..].to_vec()
    };
    let base = bases
        .iter()
        .filter(|b| b.as_path() != dir && dir.starts_with(b))
        .max_by_key(|b| b.as_os_str().len());
    let mut parts: Vec<String> = Vec::new();
    match base {
        Some(base) => {
            let head: Vec<String> = base.components().filter_map(normal).collect();
            parts.extend(last(&head, parents));
            if let Ok(rel) = dir.strip_prefix(base) {
                parts.extend(rel.components().filter_map(normal));
            }
        }
        None => {
            let own: Vec<String> = dir.components().filter_map(normal).collect();
            parts.extend(last(&own, parents + 1));
        }
    }
    if parts.is_empty() {
        return dir.display().to_string();
    }
    parts.join("/")
}

/// The configured parent dir that contains `path`, else `path`'s own parent directory —
/// which is what the state layout files a root under.
fn file_under(parents: &[PathBuf], path: &Path) -> PathBuf {
    parents
        .iter()
        .find(|p| path.starts_with(p))
        .cloned()
        .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// Whether this directory's own name is one the walk never enters. It gates *descending*
/// only: a directory that is itself a repository is a root at any level, exactly as a
/// level-1 child of a parent dir always has been.
fn is_skipped(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|n| WALK_SKIP.iter().any(|s| n == *s))
}

/// Record `dir` as a git root through [`toplevel`], or keep the notice that says why not.
fn record_git_root(
    env: &Env,
    parents: &[PathBuf],
    roots: &mut BTreeMap<PathBuf, DiscoveredRoot>,
    notices: &mut Vec<String>,
    dir: &Path,
) {
    match toplevel(env, dir) {
        Some(top) => {
            let parent = file_under(parents, &top);
            roots
                .entry(top.clone())
                .or_insert_with(|| git_root(top, parent, None));
        }
        None => notices.push(format!(
            "{}: has a .git entry but git cannot open it; skipped",
            dir.display()
        )),
    }
}

/// The linked worktrees of `main` whose canonical path lies **inside** `main`, from one
/// `worktree list --porcelain`. Entries outside it are somebody else's business: a second
/// `parent_dirs` entry, or the plain-folder walk, already reaches those, and adding them
/// here would list a worktree from a directory the user never pointed lastcall at.
fn worktrees_inside(env: &Env, main: &Path) -> Vec<PathBuf> {
    let rg = RepoGit::new(env, main);
    let Ok(list) = rg.run(&["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&list)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        // The first entry is the main worktree, which is this root.
        .skip(1)
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .filter(|p| p != main && p.starts_with(main))
        .collect()
}

/// `rev-parse --show-toplevel` from `dir`, canonicalized. `None` outside any repository
/// (or inside a bare one).
pub fn toplevel(env: &Env, dir: &Path) -> Option<PathBuf> {
    let rg = RepoGit::new(env, dir);
    let out = rg
        .run_raw(&["rev-parse", "--show-toplevel"], None)
        .ok()
        .filter(|o| o.success())?;
    let s = out.stdout_trimmed();
    if s.is_empty() {
        return None;
    }
    std::fs::canonicalize(&s).ok()
}

/// The main worktree's path when `root` is a linked worktree (`--git-dir` ≠
/// `--git-common-dir`), from the first `worktree list --porcelain` entry.
pub fn main_worktree_if_linked(rg: &RepoGit) -> Option<PathBuf> {
    // Both flags are infallible, so one batched call answers both (deliverable 1c).
    let lines = rg
        .rev_parse_batch(&["--git-dir", "--git-common-dir"], 2)
        .ok()?;
    let dir = std::fs::canonicalize(&lines[0]).ok()?;
    let common = std::fs::canonicalize(&lines[1]).ok()?;
    if dir == common {
        return None;
    }
    let list = rg.run(&["worktree", "list", "--porcelain"]).ok()?;
    let first = String::from_utf8_lossy(&list)
        .lines()
        .find_map(|l| l.strip_prefix("worktree ").map(str::to_owned))?;
    Some(std::fs::canonicalize(&first).unwrap_or_else(|_| PathBuf::from(first)))
}

/// The matcher for a `draft_dirs` pattern. `literal_separator(true)` keeps a `*` inside
/// one folder name, so `notes/*` picks the folders directly inside `notes` and never
/// something further down; only a `**` component crosses folders.
fn draft_matcher(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

/// Directories under `base` whose base-relative path matches `glob`, reading exactly
/// `depth` folder levels and skipping git internals and dependency folders.
///
/// `depth` is the entry's own reach (one level per component, the ceiling for a `**`
/// component), so naming a folder costs a `read_dir` of each base and nothing more.
fn matching_dirs(
    base: &Path,
    glob: &GlobMatcher,
    depth: usize,
    skipped: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let depth = depth.clamp(1, WALK_DEPTH);
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(base.to_path_buf(), 0)];
    while let Some((dir, level)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if WALK_SKIP.iter().any(|s| name == *s) {
                continue;
            }
            // A folder `skip_globs` names is neither a watched folder nor walked through.
            if skipped(&path) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(base)
                && glob.is_match(rel)
                && let Ok(canon) = std::fs::canonicalize(&path)
            {
                out.push(canon);
            }
            if level + 1 < depth {
                stack.push((path, level + 1));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Which roots appeared or disappeared between two passes.
pub fn diff(prev: &Discovery, next: &Discovery) -> RootsChanged {
    let added = next
        .roots
        .iter()
        .filter(|r| prev.get(&r.path).is_none())
        .map(|r| r.path.clone())
        .collect();
    let removed = prev
        .roots
        .iter()
        .filter(|r| next.get(&r.path).is_none())
        .map(|r| r.path.clone())
        .collect();
    RootsChanged {
        added,
        removed,
        reload: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

    /// The size limit the tests hand discovery; nothing here turns on its value.
    const TEST_MAX: u64 = crate::config::DEFAULT_COLLAPSE_SIZE_BYTES;

    /// Amendment v1.13: a `*` stays inside one folder name, so naming `notes/*` picks the
    /// folders directly inside `notes` and nothing deeper. Only a `**` component crosses.
    #[test]
    fn roots_draft_matcher_keeps_a_star_inside_one_folder_name() {
        let m = draft_matcher("notes/*").unwrap();
        assert!(m.is_match("notes/a"));
        assert!(!m.is_match("notes/a/b"));
        assert!(!m.is_match("notes"));
        let m = draft_matcher("*_drafts").unwrap();
        assert!(m.is_match("_drafts"));
        assert!(m.is_match("mail_drafts"));
        assert!(!m.is_match("a/_drafts"));
        let m = draft_matcher("**/notes").unwrap();
        assert!(m.is_match("notes"));
        assert!(m.is_match("a/notes"));
        assert!(m.is_match("a/b/notes"));
        assert!(!m.is_match("notes/a"));
    }

    fn git(env: &Env, cwd: &Path, args: &[&str]) {
        let out = crate::git::base_command(env, cwd)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn test_env(dir: &TempDir) -> Env {
        Env::empty(dir.path())
            .with_home(dir.mkdir("home"))
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1")
    }

    fn init_repo(env: &Env, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(env, dir, &["init", "-q", "-b", "main"]);
        git(env, dir, &["config", "user.email", "t@x"]);
        git(env, dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "a\n").unwrap();
        git(env, dir, &["add", "a"]);
        git(env, dir, &["commit", "-qm", "init"]);
    }

    #[test]
    fn roots_discovers_children_worktrees_drafts_and_nested() {
        let dir = TempDir::new("lc-roots");
        let env = test_env(&dir);
        let parent = dir.mkdir("code");
        init_repo(&env, &parent.join("alpha"));
        init_repo(&env, &parent.join("beta"));
        dir.mkdir("code/plain");
        dir.mkdir("code/_drafts");
        dir.mkdir("code/alpha/_drafts");
        git(
            &env,
            &parent.join("alpha"),
            &["worktree", "add", "-q", "../alpha-wt", "-b", "feat-w"],
        );
        init_repo(&env, &parent.join("beta/vendor/lib"));
        let canon = |p: &str| std::fs::canonicalize(parent.join(p)).unwrap();

        let d = discover(&DiscoverInputs {
            env: &env,
            parent_dirs: std::slice::from_ref(&parent),
            draft_dirs: &["_drafts/**".to_owned()],
            nested: &[(canon("beta"), parent.join("beta/vendor/lib"))],
            search_depth: 1,
            collapse_size_bytes: TEST_MAX,
            draft_dir_parents: 1,
            skip_globs: &[],
        });
        assert_eq!(d.notices, Vec::<String>::new());
        assert_eq!(
            d.paths(),
            vec![
                canon("_drafts"),
                canon("alpha"),
                canon("alpha-wt"),
                canon("alpha/_drafts"),
                canon("beta"),
                canon("beta/vendor/lib"),
            ]
        );
        let cp = std::fs::canonicalize(&parent).unwrap();
        for r in &d.roots {
            assert_eq!(
                r.parent,
                cp,
                "{}: filed under the configured parent",
                r.path.display()
            );
        }
        assert_eq!(d.get(&canon("_drafts")).unwrap().kind, RootKind::Draft);
        assert_eq!(
            d.get(&canon("alpha/_drafts")).unwrap().kind,
            RootKind::Draft
        );
        assert_eq!(
            d.get(&canon("alpha-wt")).unwrap().badge,
            Some(Badge::WorktreeOf(canon("alpha")))
        );
        assert_eq!(d.get(&canon("alpha")).unwrap().badge, None);
        assert_eq!(
            d.get(&canon("beta/vendor/lib")).unwrap().badge,
            Some(Badge::NestedIn(canon("beta")))
        );
        assert_eq!(
            d.excluded_inside(&canon("alpha")),
            vec![(b"_drafts".to_vec(), true)]
        );
        assert!(d.excluded_inside(&canon("beta")).is_empty());
    }

    /// Scenario D12's fixture, as a discovery pass at each depth: the plain-folder walk
    /// reads N folders down and nothing else. Level 1 does not move; level 2 adds the
    /// folder of clones and worktrees; level 3 adds one more; a dependency folder and a
    /// symlink are never a way in; and the walk never enters a repository, so the
    /// submodule inside `a` is not a root at any depth.
    #[test]
    fn roots_search_depth_walks_plain_folders_only() {
        let dir = TempDir::new("lc-roots-depth");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        init_repo(&env, &parent.join("a"));
        // A committed gitlink inside `a`: what a submodule is on disk, and a directory
        // with a `.git` file that the walk must never reach.
        init_repo(&env, &parent.join("a/sub"));
        git(&env, &parent.join("a"), &["add", "sub"]);
        git(&env, &parent.join("a"), &["commit", "-qm", "sub"]);
        // A clone and a linked worktree, one plain folder down.
        git(
            &env,
            &parent,
            &[
                "clone",
                "-q",
                parent.join("a").to_str().unwrap(),
                parent.join("worktrees/b").to_str().unwrap(),
            ],
        );
        git(
            &env,
            &parent.join("a"),
            &[
                "worktree",
                "add",
                "-q",
                parent.join("worktrees/a-wt").to_str().unwrap(),
                "-b",
                "feat-w",
            ],
        );
        init_repo(&env, &parent.join("deep/er/c"));
        init_repo(&env, &parent.join("node_modules/pkg"));
        std::os::unix::fs::symlink(parent.join("deep"), parent.join("link")).unwrap();
        let canon = |p: &str| std::fs::canonicalize(parent.join(p)).unwrap();

        let at = |depth: u8| -> Discovery {
            discover(&DiscoverInputs {
                env: &env,
                parent_dirs: std::slice::from_ref(&parent),
                draft_dirs: &[],
                nested: &[],
                search_depth: depth,
                collapse_size_bytes: TEST_MAX,
                draft_dir_parents: 1,
                skip_globs: &[],
            })
        };

        assert_eq!(at(1).paths(), vec![canon("a")], "level 1 does not move");
        let two = at(2);
        assert_eq!(
            two.paths(),
            vec![canon("a"), canon("worktrees/a-wt"), canon("worktrees/b")],
        );
        assert_eq!(
            two.get(&canon("worktrees/a-wt")).unwrap().badge,
            Some(Badge::WorktreeOf(canon("a"))),
            "a linked worktree is badged wherever the walk found it"
        );
        assert_eq!(two.get(&canon("worktrees/b")).unwrap().badge, None);
        let cp = std::fs::canonicalize(&parent).unwrap();
        for r in &two.roots {
            assert_eq!(r.parent, cp, "{} is filed under P", r.path.display());
        }
        assert_eq!(
            at(3).paths(),
            vec![
                canon("a"),
                canon("deep/er/c"),
                canon("worktrees/a-wt"),
                canon("worktrees/b"),
            ],
            "one more folder down"
        );
        for depth in 1..=4 {
            let d = at(depth);
            assert!(
                !d.paths().contains(&canon("a/sub")),
                "the walk never enters a repository (depth {depth})"
            );
            assert!(
                !d.paths().contains(&canon("node_modules/pkg")),
                "a dependency folder is never entered (depth {depth})"
            );
            assert!(
                !d.paths().iter().any(|p| p.starts_with(parent.join("link"))),
                "a symlink to a plain folder is not descended (depth {depth})"
            );
        }
    }

    /// Mechanism 2: the worktree a repository keeps inside itself. The plain-folder walk
    /// cannot reach it (it never enters a repository), so one `worktree list` per root
    /// does, from a parent above `R` and from `R` itself alike. A worktree that lies
    /// *outside* its repository is somebody else's business and is not added by it.
    #[test]
    fn roots_search_depth_lists_a_worktree_kept_inside_a_repository() {
        let dir = TempDir::new("lc-roots-inside");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        let repo = parent.join("R");
        init_repo(&env, &repo);
        std::fs::write(repo.join(".gitignore"), ".worktrees/\n").unwrap();
        git(&env, &repo, &["add", ".gitignore"]);
        git(&env, &repo, &["commit", "-qm", "ignore"]);
        git(
            &env,
            &repo,
            &["worktree", "add", "-q", ".worktrees/wt", "-b", "feat-w"],
        );
        // And one kept outside it, which mechanism 2 must not claim.
        git(
            &env,
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                parent.join("outside-wt").to_str().unwrap(),
                "-b",
                "feat-o",
            ],
        );
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let wt = canon(&repo.join(".worktrees/wt"));

        let at = |parents: &[PathBuf], depth: u8| -> Discovery {
            discover(&DiscoverInputs {
                env: &env,
                parent_dirs: parents,
                draft_dirs: &[],
                nested: &[],
                search_depth: depth,
                collapse_size_bytes: TEST_MAX,
                draft_dir_parents: 1,
                skip_globs: &[],
            })
        };

        let one = at(std::slice::from_ref(&parent), 1);
        assert_eq!(
            one.paths(),
            vec![canon(&repo), canon(&parent.join("outside-wt"))],
            "at depth 1 the one inside R is invisible; the one beside it is a level-1 child"
        );

        let two = at(std::slice::from_ref(&parent), 2);
        assert!(two.paths().contains(&wt), "{:?}", two.paths());
        assert_eq!(
            two.get(&wt).unwrap().badge,
            Some(Badge::WorktreeOf(canon(&repo)))
        );
        assert_eq!(
            two.get(&wt).unwrap().parent,
            canon(&parent),
            "filed under the configured parent, not under R"
        );

        // Launched from inside the repository: the parent *is* R, so the plain-folder walk
        // reads nothing at all, and the worktree still shows up.
        let inside = at(std::slice::from_ref(&repo), 2);
        assert_eq!(inside.paths(), vec![canon(&repo), wt.clone()]);
        assert_eq!(
            inside.get(&wt).unwrap().badge,
            Some(Badge::WorktreeOf(canon(&repo)))
        );
        // The one outside R is not dragged in by mechanism 2 from a parent that is R.
        assert!(
            !inside.paths().contains(&canon(&parent.join("outside-wt"))),
            "a worktree outside the repository needs its own parent_dirs entry"
        );

        // Launched from a subdirectory of R: R is filed under its own parent by the
        // fallback rule, and the worktree is filed where R is, not under `R/.worktrees`.
        let src = repo.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let deep = at(std::slice::from_ref(&src), 2);
        assert!(deep.paths().contains(&wt), "{:?}", deep.paths());
        assert_eq!(
            deep.get(&wt).unwrap().parent,
            deep.get(&canon(&repo)).unwrap().parent,
            "filed where its repository is filed"
        );
        assert_eq!(deep.get(&wt).unwrap().parent, canon(&parent));
    }

    #[test]
    fn roots_parent_inside_a_repo_is_that_repo_and_outside_roots_use_own_parent() {
        let dir = TempDir::new("lc-roots2");
        let env = test_env(&dir);
        let repo = dir.path().join("repo");
        init_repo(&env, &repo);
        let inner = dir.mkdir("repo/src/deep");
        let elsewhere = dir.mkdir("elsewhere/notes");
        let d = discover(&DiscoverInputs {
            env: &env,
            parent_dirs: std::slice::from_ref(&inner),
            draft_dirs: &[elsewhere.to_string_lossy().into_owned()],
            nested: &[],
            search_depth: 1,
            collapse_size_bytes: TEST_MAX,
            draft_dir_parents: 1,
            skip_globs: &[],
        });
        let repo_c = std::fs::canonicalize(&repo).unwrap();
        let notes_c = std::fs::canonicalize(&elsewhere).unwrap();
        assert_eq!(d.paths(), vec![notes_c.clone(), repo_c.clone()]);
        // The repo is not *under* the configured parent (the parent is inside it): the
        // ad-hoc rule applies and it files under its own parent directory.
        assert_eq!(
            d.get(&repo_c).unwrap().parent,
            repo_c.parent().unwrap().to_path_buf()
        );
        assert_eq!(d.get(&notes_c).unwrap().parent, notes_c.parent().unwrap());
        // Rescan diff.
        let mut next = d.clone();
        next.roots.retain(|r| r.path != notes_c);
        let changed = diff(&d, &next);
        assert_eq!(changed.removed, vec![notes_c]);
        assert!(changed.added.is_empty());
        let missing = discover(&DiscoverInputs {
            env: &env,
            parent_dirs: &[dir.path().join("nope")],
            draft_dirs: &[],
            nested: &[],
            search_depth: 1,
            collapse_size_bytes: TEST_MAX,
            draft_dir_parents: 1,
            skip_globs: &[],
        });
        assert!(missing.roots.is_empty());
        assert_eq!(missing.notices.len(), 1);
    }

    /// The fixture every name and scope test below reads: a parent dir called `git` with a
    /// repository `repo1` in it, a watched folder inside the repository and another folder
    /// inside that, and a `notes` folder directly under the parent.
    struct NameFixture {
        dir: TempDir,
        env: Env,
        parent: PathBuf,
    }

    impl NameFixture {
        fn new() -> Self {
            let dir = TempDir::new("lc-roots-name");
            let env = test_env(&dir);
            let parent = dir.mkdir("git");
            init_repo(&env, &parent.join("repo1"));
            init_repo(&env, &parent.join("repo2"));
            dir.mkdir("git/repo1/z_ignore/research");
            dir.mkdir("git/repo2/z_ignore");
            dir.mkdir("git/notes/a/b");
            dir.mkdir("git/deep/down/notes");
            Self { dir, env, parent }
        }

        fn canon(&self, rel: &str) -> PathBuf {
            std::fs::canonicalize(self.parent.join(rel)).unwrap()
        }

        fn discover(&self, entries: &[&str], parents: u8) -> Discovery {
            let owned: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
            discover(&DiscoverInputs {
                env: &self.env,
                parent_dirs: std::slice::from_ref(&self.parent),
                draft_dirs: &owned,
                nested: &[],
                search_depth: 1,
                collapse_size_bytes: TEST_MAX,
                draft_dir_parents: parents,
                skip_globs: &[],
            })
        }

        /// `(name, recursive)` per watched folder, keyed by the parent-relative path.
        fn watched(&self, d: &Discovery) -> BTreeMap<String, (String, bool)> {
            d.roots
                .iter()
                .filter(|r| r.kind == RootKind::Draft)
                .map(|r| {
                    let rel = r
                        .path
                        .strip_prefix(std::fs::canonicalize(&self.parent).unwrap())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| r.path.display().to_string());
                    (
                        rel,
                        (
                            r.name.clone(),
                            r.scope.as_ref().is_some_and(|s| s.recursive),
                        ),
                    )
                })
                .collect()
        }
    }

    /// Amendment v1.13 R4's worked examples: the name is the folder's path below the
    /// **deepest** base that contains it, with `draft_dir_parents` folders of that base in
    /// front, so a scratch folder of the same name in two projects reads as two rows.
    #[test]
    fn roots_draft_names_come_from_the_deepest_containing_base() {
        let fx = NameFixture::new();
        let d = fx.discover(&["**/z_ignore", "z_ignore/research", "notes"], 1);
        let got = fx.watched(&d);
        assert_eq!(
            got.keys().cloned().collect::<Vec<_>>(),
            vec![
                "notes".to_owned(),
                "repo1/z_ignore".to_owned(),
                "repo1/z_ignore/research".to_owned(),
                "repo2/z_ignore".to_owned(),
            ]
        );
        // Found under the parent dir as `repo1/z_ignore` and under the repository as
        // `z_ignore`: the repository is the deeper base, so the name reads the same way
        // whichever entry matched it (design review F4).
        assert_eq!(got["repo1/z_ignore"].0, "repo1/z_ignore");
        assert_eq!(got["repo1/z_ignore/research"].0, "repo1/z_ignore/research");
        assert_eq!(got["repo2/z_ignore"].0, "repo2/z_ignore");
        // No special case for the parent dir: its own last folder is the prefix.
        assert_eq!(got["notes"].0, "git/notes");

        // `0` is the path below the base alone, `2` reaches one folder further up.
        let flat = fx.watched(&fx.discover(&["**/z_ignore", "notes"], 0));
        assert_eq!(flat["repo1/z_ignore"].0, "z_ignore");
        assert_eq!(flat["notes"].0, "notes");
        let wide = fx.watched(&fx.discover(&["notes"], 2));
        let parent_name = std::fs::canonicalize(&fx.parent).unwrap();
        let grand = parent_name
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(wide["notes"].0, format!("{grand}/git/notes"));
    }

    /// A glob names the folder it matched, not its pattern text.
    #[test]
    fn roots_draft_name_of_a_glob_is_the_matched_folder() {
        let fx = NameFixture::new();
        fx.dir.mkdir("git/repo1/_drafts");
        let got = fx.watched(&fx.discover(&["_dr*"], 1));
        assert_eq!(got["repo1/_drafts"].0, "repo1/_drafts");
    }

    /// An absolute entry for a folder no base contains is named by its own last
    /// `1 + draft_dir_parents` folders.
    #[test]
    fn roots_draft_name_of_a_folder_outside_every_base() {
        let fx = NameFixture::new();
        let outside = fx.dir.mkdir("elsewhere/scratch");
        let canon = std::fs::canonicalize(&outside).unwrap();
        let entry = canon.to_string_lossy().into_owned();
        let d = fx.discover(&[&entry], 1);
        assert_eq!(d.get(&canon).unwrap().name, "elsewhere/scratch");
        let d = fx.discover(&[&entry], 0);
        assert_eq!(d.get(&canon).unwrap().name, "scratch");
    }

    /// Two entries naming one folder: one row, the wider scope, the same name whichever
    /// order the config lists them in (design review F4).
    #[test]
    fn roots_draft_entry_order_changes_neither_the_name_nor_the_scope() {
        let fx = NameFixture::new();
        let forward = fx.watched(&fx.discover(&["**/z_ignore", "z_ignore/**"], 1));
        let backward = fx.watched(&fx.discover(&["z_ignore/**", "**/z_ignore"], 1));
        assert_eq!(forward, backward);
        assert_eq!(
            forward["repo1/z_ignore"],
            ("repo1/z_ignore".to_owned(), true),
            "the wider of the two scopes wins"
        );
        let plain = fx.watched(&fx.discover(&["**/z_ignore"], 1));
        assert!(!plain["repo1/z_ignore"].1, "a plain entry is one folder");
    }

    /// Amendment v1.13 R3 and design review F9: the walk reads exactly as many folder
    /// levels as the entry names, and only a `**` component reads to the ceiling.
    #[test]
    fn roots_draft_walk_reads_only_the_levels_the_entry_names() {
        let fx = NameFixture::new();
        let deep = fx.canon("deep/down/notes");
        let shallow = fx.canon("notes");

        let one = fx.discover(&["notes"], 1);
        assert!(one.get(&shallow).is_some());
        assert!(
            one.get(&deep).is_none(),
            "a one-component entry reads one level below each base"
        );

        let three = fx.discover(&["*/*/notes"], 1);
        assert!(three.get(&deep).is_some());
        assert!(
            three.get(&shallow).is_none(),
            "the pattern has to match the whole relative path"
        );

        let any = fx.discover(&["**/notes"], 1);
        assert!(
            any.get(&deep).is_some(),
            "a ** component reads to the ceiling"
        );
        assert!(any.get(&shallow).is_some());

        // `notes/*` is the folders directly inside `notes`, never something deeper.
        let inside = fx.discover(&["notes/*"], 1);
        let watched: Vec<PathBuf> = inside
            .roots
            .iter()
            .filter(|r| r.kind == RootKind::Draft)
            .map(|r| r.path.clone())
            .collect();
        assert_eq!(watched, vec![fx.canon("notes/a")]);
        assert!(inside.get(&fx.canon("notes/a/b")).is_none());
    }

    /// The exclusion rule (design review F2): a repository hands over every watched folder
    /// inside it, a folder watched with its whole tree hands over the ones inside that, and
    /// a folder watched on its own hands over nothing.
    #[test]
    fn roots_excluded_dirs_carry_whether_the_whole_tree_is_handed_over() {
        let fx = NameFixture::new();
        let repo1 = fx.canon("repo1");
        let outer = fx.canon("repo1/z_ignore");
        let inner = fx.canon("repo1/z_ignore/research");

        let d = fx.discover(&["**/z_ignore/**", "z_ignore/research"], 1);
        assert_eq!(
            d.get(&repo1).unwrap().excluded_dirs,
            vec![
                (b"z_ignore".to_vec(), true),
                (b"z_ignore/research".to_vec(), false),
            ],
            "a repository hands over every watched folder inside it"
        );
        assert_eq!(
            d.excluded_inside(&repo1),
            d.get(&repo1).unwrap().excluded_dirs
        );
        assert_eq!(
            d.get(&outer).unwrap().excluded_dirs,
            vec![(b"research".to_vec(), false)],
            "the tree scope hands over the folder inside it"
        );
        assert!(d.get(&inner).unwrap().excluded_dirs.is_empty());

        // The same two folders, the outer one watched on its own: it hands over nothing,
        // because a subfolder is already outside what it covers.
        let d = fx.discover(&["**/z_ignore", "z_ignore/research"], 1);
        assert!(d.get(&outer).unwrap().excluded_dirs.is_empty());
        assert_eq!(
            d.get(&repo1).unwrap().excluded_dirs,
            vec![
                (b"z_ignore".to_vec(), false),
                (b"z_ignore/research".to_vec(), false),
            ]
        );
    }

    /// A discovery pass with `skip_globs`, everything else at its plainest.
    fn discover_skipping(
        env: &Env,
        parents: &[PathBuf],
        draft_dirs: &[String],
        nested: &[(PathBuf, PathBuf)],
        depth: u8,
        skip: &[&str],
    ) -> Discovery {
        let skip: Vec<String> = skip.iter().map(|s| (*s).to_owned()).collect();
        discover(&DiscoverInputs {
            env,
            parent_dirs: parents,
            draft_dirs,
            nested,
            search_depth: depth,
            collapse_size_bytes: TEST_MAX,
            draft_dir_parents: 1,
            skip_globs: &skip,
        })
    }

    /// The git processes this thread spawns while `f` runs.
    fn spawns<T>(f: impl FnOnce() -> T) -> (T, u64) {
        let before = crate::git::thread_spawn_count();
        let out = f();
        (out, crate::git::thread_spawn_count() - before)
    }

    /// Amendment v1.15 (scenario A9): a repository `skip_globs` names is never listed, and
    /// the match runs before the `.git` probe and `rev-parse`, so it costs no spawn: the
    /// pass spends exactly what the same tree without the skipped repositories costs.
    #[test]
    fn roots_skip_globs_leave_a_matching_repository_unopened() {
        let dir = TempDir::new("lc-roots-skip");
        let env = test_env(&dir);
        let full = dir.mkdir("P");
        init_repo(&env, &full.join("keep"));
        init_repo(&env, &full.join("c1"));
        init_repo(&env, &full.join("evals/c2"));
        init_repo(&env, &full.join("evals/deeper/c3"));
        // The twin: the same tree without the three repositories the skip names.
        let twin = dir.mkdir("Q");
        init_repo(&env, &twin.join("keep"));
        dir.mkdir("Q/evals/deeper");
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let p = std::slice::from_ref(&full);
        let q = std::slice::from_ref(&twin);

        let (plain, plain_cost) = spawns(|| discover_skipping(&env, p, &[], &[], 3, &[]));
        assert_eq!(plain.roots.len(), 4, "{:?}", plain.paths());

        let (base, base_cost) = spawns(|| discover_skipping(&env, q, &[], &[], 3, &[]));
        assert_eq!(base.paths(), vec![canon(&twin.join("keep"))]);

        // A level-1 repository and a depth-2 one, each by its own pattern.
        let (d, cost) =
            spawns(|| discover_skipping(&env, p, &[], &[], 3, &["c1", "evals/*", "evals/*/c3"]));
        assert_eq!(d.paths(), vec![canon(&full.join("keep"))]);
        assert!(d.notices.is_empty(), "{:?}", d.notices);
        assert_eq!(cost, base_cost, "a skipped repository costs no spawn");
        assert!(plain_cost > cost, "{plain_cost} vs {cost}");

        // A folder matching `evals/**` is not entered: `evals` itself matches as `evals/`,
        // so nothing below it is probed, however deep.
        let (d, cost) = spawns(|| discover_skipping(&env, p, &[], &[], 3, &["c1", "evals/**"]));
        assert_eq!(d.paths(), vec![canon(&full.join("keep"))]);
        assert_eq!(cost, base_cost);
        let skip = Skip::new(&["evals/**".to_owned()], &[], dir.path());
        assert!(skip.matches_dir(Path::new("evals")), "`rel/` is tried too");
        assert!(skip.matches_dir(Path::new("evals/c2")));
        assert!(!skip.matches_dir(Path::new("other")));

        // An empty list changes nothing.
        assert_eq!(discover_skipping(&env, p, &[], &[], 3, &[]), plain);
    }

    /// A scan's nested report that matches is never promoted, and never `rev-parse`d.
    #[test]
    fn roots_skip_globs_drop_a_nested_report_before_it_is_opened() {
        let dir = TempDir::new("lc-roots-skip-nested");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        let proj = parent.join("proj");
        init_repo(&env, &proj);
        init_repo(&env, &proj.join("z_ignore/evals/c1"));
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let ps = std::slice::from_ref(&parent);
        let nested = vec![(canon(&proj), proj.join("z_ignore/evals/c1"))];

        let promoted = discover_skipping(&env, ps, &[], &nested, 1, &[]);
        assert_eq!(
            promoted.paths(),
            vec![canon(&proj), canon(&proj.join("z_ignore/evals/c1"))]
        );
        let (_, base_cost) =
            spawns(|| discover_skipping(&env, ps, &[], &[], 1, &["*/z_ignore/**"]));
        let (d, cost) = spawns(|| discover_skipping(&env, ps, &[], &nested, 1, &["*/z_ignore/**"]));
        assert_eq!(d.paths(), vec![canon(&proj)]);
        assert_eq!(
            cost, base_cost,
            "the nested promotion's `toplevel` never ran"
        );
        // The sponsor's own line.
        let d = discover_skipping(&env, ps, &[], &nested, 1, &["*/z_ignore/**/evals/**"]);
        assert_eq!(d.paths(), vec![canon(&proj)]);
    }

    /// Verification F1: a pattern naming a folder drops what promotion finds under it,
    /// the way the walk never enters a matching folder. `app/vendored` and
    /// `app/vendored/*` agree, and the promotion's `toplevel` never runs for either.
    #[test]
    fn roots_skip_globs_a_folder_pattern_drops_what_promotion_finds_under_it() {
        let dir = TempDir::new("lc-roots-skip-above");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        let app = parent.join("app");
        init_repo(&env, &app);
        init_repo(&env, &app.join("vendored/inner"));
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let ps = std::slice::from_ref(&parent);
        let nested = vec![(canon(&app), app.join("vendored/inner"))];

        let promoted = discover_skipping(&env, ps, &[], &nested, 1, &[]);
        assert_eq!(
            promoted.paths(),
            vec![canon(&app), canon(&app.join("vendored/inner"))]
        );
        let (_, base_cost) = spawns(|| discover_skipping(&env, ps, &[], &[], 1, &[]));
        for pattern in [
            "app/vendored",
            "app/vendored/",
            "app/vendored/*",
            "*/vendored",
        ] {
            let (d, cost) = spawns(|| discover_skipping(&env, ps, &[], &nested, 1, &[pattern]));
            assert_eq!(d.paths(), vec![canon(&app)], "{pattern}");
            assert_eq!(
                cost, base_cost,
                "{pattern}: the promotion's `toplevel` never ran"
            );
        }
    }

    /// Verification F3: a pattern that skips every candidate under a parent dir leaves the
    /// screen empty, so the pass says so, once per such parent, with the count. A parent
    /// that keeps a root says nothing.
    #[test]
    fn roots_skip_globs_say_when_a_parent_is_left_with_nothing() {
        let dir = TempDir::new("lc-roots-skip-empty");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        for name in ["app", "evals", "keep"] {
            init_repo(&env, &parent.join(name));
        }
        let ps = std::slice::from_ref(&parent);
        let d = discover_skipping(&env, ps, &[], &[], 1, &["*"]);
        assert!(d.paths().is_empty());
        let canon = std::fs::canonicalize(&parent).unwrap();
        assert_eq!(
            d.notices,
            vec![format!(
                "skip_globs: nothing left under {} (3 skipped)",
                canon.display()
            )]
        );
        let d = discover_skipping(&env, ps, &[], &[], 1, &["app", "evals"]);
        assert_eq!(d.paths(), vec![canon.join("keep")]);
        assert!(d.notices.is_empty(), "{:?}", d.notices);
    }

    /// F9: a parent dir entry and the launch directory are never skipped, and a notice says
    /// the pattern was ignored for them. The same pattern under a parent above `r` skips it.
    #[test]
    fn roots_skip_globs_never_skip_a_parent_dir_or_the_launch_directory() {
        let dir = TempDir::new("lc-roots-skip-parent");
        let env = test_env(&dir);
        let w = dir.mkdir("W");
        let r = w.join("r");
        init_repo(&env, &r);
        std::fs::create_dir_all(r.join("src")).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let says_ignored = |d: &Discovery| {
            d.notices
                .iter()
                .any(|n| n.contains("skip_globs") && n.contains("ignored"))
        };

        let under_w = discover_skipping(&env, std::slice::from_ref(&w), &[], &[], 1, &["r"]);
        assert!(under_w.roots.is_empty(), "{:?}", under_w.paths());
        assert!(!says_ignored(&under_w), "{:?}", under_w.notices);

        // `parent_dirs = [W/r]`: the entry itself.
        let entry = discover_skipping(&env, std::slice::from_ref(&r), &[], &[], 1, &["r"]);
        assert_eq!(entry.paths(), vec![canon(&r)]);
        assert!(says_ignored(&entry), "{:?}", entry.notices);

        // A parent inside `r` is `r`.
        let inside = discover_skipping(&env, &[r.join("src")], &[], &[], 1, &["r"]);
        assert_eq!(inside.paths(), vec![canon(&r)]);
        assert!(says_ignored(&inside), "{:?}", inside.notices);

        // Launched inside `r` with `parent_dirs = [W]`.
        let launched = env.clone().with_cwd(r.join("src"));
        let d = discover_skipping(&launched, std::slice::from_ref(&w), &[], &[], 1, &["r"]);
        assert_eq!(d.paths(), vec![canon(&r)]);
        assert!(says_ignored(&d), "{:?}", d.notices);
    }

    /// A watched folder whose own path matches is not a root; one beside it is.
    #[test]
    fn roots_skip_globs_drop_a_matching_watched_folder() {
        let dir = TempDir::new("lc-roots-skip-draft");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        dir.mkdir("P/a_notes");
        dir.mkdir("P/b_notes");
        let proj = parent.join("proj");
        init_repo(&env, &proj);
        dir.mkdir("P/proj/z_ignore");
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let ps = std::slice::from_ref(&parent);
        let drafts = vec!["*_notes".to_owned(), "z_ignore/**".to_owned()];

        let all = discover_skipping(&env, ps, &drafts, &[], 1, &[]);
        assert_eq!(
            all.paths(),
            vec![
                canon(&parent.join("a_notes")),
                canon(&parent.join("b_notes")),
                canon(&proj),
                canon(&proj.join("z_ignore")),
            ]
        );
        let d = discover_skipping(&env, ps, &drafts, &[], 1, &["b_notes", "proj/z_ignore"]);
        assert_eq!(
            d.paths(),
            vec![canon(&parent.join("a_notes")), canon(&proj)]
        );
        assert!(
            d.get(&canon(&proj)).unwrap().excluded_dirs.is_empty(),
            "a skipped watched folder hands nothing over"
        );
    }

    /// A linked worktree is skipped only when it matches on its own: skipping its main
    /// leaves it listed, and skipping it leaves the main listed.
    #[test]
    fn roots_skip_globs_take_a_linked_worktree_on_its_own_path() {
        let dir = TempDir::new("lc-roots-skip-wt");
        let env = test_env(&dir);
        let parent = dir.mkdir("P");
        let m = parent.join("m");
        init_repo(&env, &m);
        std::fs::write(m.join(".gitignore"), ".worktrees/\n").unwrap();
        git(&env, &m, &["add", ".gitignore"]);
        git(&env, &m, &["commit", "-qm", "ignore"]);
        git(
            &env,
            &m,
            &["worktree", "add", "-q", "../m-wt", "-b", "feat-w"],
        );
        git(
            &env,
            &m,
            &["worktree", "add", "-q", ".worktrees/in", "-b", "feat-i"],
        );
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let ps = std::slice::from_ref(&parent);

        let d = discover_skipping(&env, ps, &[], &[], 1, &["m"]);
        assert_eq!(d.paths(), vec![canon(&parent.join("m-wt"))]);
        assert_eq!(
            d.roots[0].badge,
            Some(Badge::WorktreeOf(canon(&m))),
            "still badged by its main"
        );
        let d = discover_skipping(&env, ps, &[], &[], 1, &["m-wt"]);
        assert_eq!(d.paths(), vec![canon(&m)]);
        // Mechanism 2: the one kept inside `m`.
        let d = discover_skipping(&env, ps, &[], &[], 2, &[]);
        assert!(d.paths().contains(&canon(&m.join(".worktrees/in"))));
        let d = discover_skipping(&env, ps, &[], &[], 2, &["m/.worktrees/*"]);
        assert_eq!(d.paths(), vec![canon(&m), canon(&parent.join("m-wt"))]);
    }
}
