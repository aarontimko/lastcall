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
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};

use crate::env::Env;
use crate::git::RepoGit;
use crate::store::RootKind;

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

    /// Draft roots that lie inside `git_root`, as root-relative byte paths (the git root's
    /// `others` entries beneath them belong to the draft root).
    pub fn draft_dirs_inside(&self, git_root: &Path) -> Vec<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        self.roots
            .iter()
            .filter(|r| r.kind == RootKind::Draft)
            .filter_map(|r| r.path.strip_prefix(git_root).ok())
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(|rel| rel.as_os_str().as_bytes().to_vec())
            .collect()
    }
}

/// What changed between two discovery passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootsChanged {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
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

    // Git roots under each parent.
    for p in &parents {
        if !p.is_dir() {
            notices.push(format!("parent dir {} does not exist", p.display()));
            continue;
        }
        if let Some(top) = toplevel(inputs.env, p) {
            roots.entry(top.clone()).or_insert(DiscoveredRoot {
                path: top.clone(),
                kind: RootKind::Git,
                parent: parent_of(&top),
                badge: None,
            });
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
        if roots.contains_key(&canon) {
            continue;
        }
        if toplevel(inputs.env, &canon).as_deref() != Some(canon.as_path()) {
            continue;
        }
        let parent = parent_of(&canon);
        roots.insert(
            canon.clone(),
            DiscoveredRoot {
                path: canon,
                kind: RootKind::Git,
                parent,
                badge: Some(Badge::NestedIn(outer.clone())),
            },
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
                roots.entry(path.clone()).or_insert(DiscoveredRoot {
                    path,
                    kind: RootKind::Git,
                    parent: parent.clone(),
                    badge: Some(Badge::WorktreeOf(main.clone())),
                });
            }
        }
    }

    // Linked-worktree badges.
    for root in roots.values_mut() {
        let rg = RepoGit::new(inputs.env, &root.path);
        if let Some(main) = main_worktree_if_linked(&rg) {
            root.badge = Some(Badge::WorktreeOf(main));
        }
    }

    // Draft roots.
    let git_roots: Vec<PathBuf> = roots.keys().cloned().collect();
    for entry in inputs.draft_dirs {
        let pattern = entry.strip_suffix("/**").unwrap_or(entry);
        if Path::new(pattern).is_absolute() {
            let path = Path::new(pattern);
            if let Ok(canon) = std::fs::canonicalize(path)
                && canon.is_dir()
            {
                let parent = parent_of(&canon);
                roots.entry(canon.clone()).or_insert(DiscoveredRoot {
                    path: canon,
                    kind: RootKind::Draft,
                    parent,
                    badge: None,
                });
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
        let mut bases: Vec<PathBuf> = parents.clone();
        bases.extend(git_roots.iter().cloned());
        for base in bases {
            for dir in matching_dirs(&base, &matcher) {
                let parent = parent_of(&dir);
                roots.entry(dir.clone()).or_insert(DiscoveredRoot {
                    path: dir,
                    kind: RootKind::Draft,
                    parent,
                    badge: None,
                });
            }
        }
    }

    // Sorted by path bytes (the status JSON order), not component-wise.
    let mut roots: Vec<DiscoveredRoot> = roots.into_values().collect();
    roots.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
    Discovery { roots, notices }
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
            roots.entry(top.clone()).or_insert(DiscoveredRoot {
                path: top,
                kind: RootKind::Git,
                parent,
                badge: None,
            });
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

/// Directories under `base` (bounded depth, skipping git internals and dependency dirs)
/// whose base-relative path matches `glob`.
fn matching_dirs(base: &Path, glob: &GlobMatcher) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(base.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
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
            if let Ok(rel) = path.strip_prefix(base)
                && glob.is_match(rel)
                && let Ok(canon) = std::fs::canonicalize(&path)
            {
                out.push(canon);
            }
            if depth + 1 < WALK_DEPTH {
                stack.push((path, depth + 1));
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
    RootsChanged { added, removed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

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
            d.draft_dirs_inside(&canon("alpha")),
            vec![b"_drafts".to_vec()]
        );
        assert!(d.draft_dirs_inside(&canon("beta")).is_empty());
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
        });
        assert!(missing.roots.is_empty());
        assert_eq!(missing.notices.len(), 1);
    }
}
