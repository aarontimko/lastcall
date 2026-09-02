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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};

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
}

const WALK_DEPTH: usize = 4;
const WALK_SKIP: &[&str] = &[".git", "node_modules", "target", ".venv", "vendor"];

/// Run one discovery pass.
pub fn discover(inputs: &DiscoverInputs<'_>) -> Discovery {
    let mut roots: BTreeMap<PathBuf, DiscoveredRoot> = BTreeMap::new();
    let mut notices = Vec::new();
    let parents: Vec<PathBuf> = inputs
        .parent_dirs
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let parent_of = |path: &Path| -> PathBuf {
        parents
            .iter()
            .find(|p| path.starts_with(p))
            .cloned()
            .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default())
    };

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
            continue;
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            notices.push(format!("cannot list parent dir {}", p.display()));
            continue;
        };
        let mut children: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|c| c.is_dir() && c.join(".git").exists())
            .collect();
        children.sort();
        for child in children {
            match toplevel(inputs.env, &child) {
                Some(top) => {
                    roots.entry(top.clone()).or_insert(DiscoveredRoot {
                        path: top.clone(),
                        kind: RootKind::Git,
                        parent: parent_of(&top),
                        badge: None,
                    });
                }
                None => notices.push(format!(
                    "{}: has a .git entry but git cannot open it; skipped",
                    child.display()
                )),
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
        let matcher = match Glob::new(pattern) {
            Ok(g) => g.compile_matcher(),
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
    let dir = rg
        .run(&["rev-parse", "--path-format=absolute", "--git-dir"])
        .ok()?;
    let common = rg
        .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .ok()?;
    let dir = std::fs::canonicalize(String::from_utf8_lossy(&dir).trim()).ok()?;
    let common = std::fs::canonicalize(String::from_utf8_lossy(&common).trim()).ok()?;
    if dir == common {
        return None;
    }
    let list = rg.run(&["worktree", "list", "--porcelain"]).ok()?;
    let first = String::from_utf8_lossy(&list)
        .lines()
        .find_map(|l| l.strip_prefix("worktree ").map(str::to_owned))?;
    Some(std::fs::canonicalize(&first).unwrap_or_else(|_| PathBuf::from(first)))
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
        });
        assert!(missing.roots.is_empty());
        assert_eq!(missing.notices.len(), 1);
    }
}
