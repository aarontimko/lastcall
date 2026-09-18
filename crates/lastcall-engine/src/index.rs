//! The private index: a cache and never truth (docs/spec/00-spec.md §6.5; kickoff
//! deliverable 4).
//!
//! `<repo>/index` is seeded from the seen tree (`read-tree`), refreshed each scan so git's
//! stat cache makes the common case a stat-only pass, and consulted through `diff-files`
//! (changed/deleted vs the seen tree) and `ls-files --others` (new files). `<repo>/index.tree`
//! records the tree the index was seeded from and is written **after** the seed; on open a
//! mismatch with the ledger's seen tree, or a missing/unreadable index, reseeds. Deleting
//! both files between two scans yields an identical pile.
//!
//! A held `index.lock` (another lastcall process) is retried three times with a 50 ms
//! backoff, then the scan proceeds **unrefreshed**: `diff-files` still lists every
//! candidate, at worst over-reporting.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::git::{self, DiffEntry, GitError, Oid, StoreGit};
use crate::paths::RepoPaths;
use crate::store::{DraftScope, RootKind};

const LOCK_RETRIES: u32 = 3;
const LOCK_BACKOFF: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("index io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> IndexError {
    IndexError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn parse_err(args: &[&str], message: String) -> IndexError {
    GitError::Parse {
        argv: args.iter().map(|s| s.to_string()).collect(),
        message,
    }
    .into()
}

/// One `ls-files --others` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Other {
    /// A new file (root-relative path bytes).
    File(Vec<u8>),
    /// A nested repository, reported by git as `dir/` (D9): another root, never a candidate.
    NestedRepo(Vec<u8>),
}

/// The private index for one root.
#[derive(Debug)]
pub struct PrivateIndex {
    git: Arc<StoreGit>,
    index: PathBuf,
    index_tree: PathBuf,
    kind: RootKind,
    /// The user's `info/exclude` (common dir), passed as `--exclude-from` for git roots.
    exclude_from: Option<PathBuf>,
}

impl PrivateIndex {
    pub fn new(
        git: Arc<StoreGit>,
        paths: &RepoPaths,
        kind: RootKind,
        exclude_from: Option<PathBuf>,
    ) -> Self {
        Self {
            git,
            index: paths.index.clone(),
            index_tree: paths.index_tree.clone(),
            kind,
            exclude_from,
        }
    }

    pub fn path(&self) -> &Path {
        &self.index
    }

    /// The tree recorded by the last successful seed (`None` when missing/unreadable).
    pub fn recorded_tree(&self) -> Option<Option<Oid>> {
        let text = std::fs::read_to_string(&self.index_tree).ok()?;
        let text = text.trim();
        if text == "empty" {
            Some(None)
        } else {
            Oid::parse(text).map(Some)
        }
    }

    /// A quick sanity check of the cache file: the `DIRC` magic and room for the header
    /// and the trailing checksum. An empty or garbage file fails it and is reseeded.
    fn looks_like_an_index(&self) -> bool {
        let Ok(mut f) = std::fs::File::open(&self.index) else {
            return false;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut magic = [0u8; 4];
        len >= 32 && std::io::Read::read_exact(&mut f, &mut magic).is_ok() && &magic == b"DIRC"
    }

    /// Reseed unless the index exists, looks like an index, and `index.tree` matches
    /// `seen_tree`.
    pub fn ensure(&self, seen_tree: Option<&Oid>) -> Result<bool, IndexError> {
        let fresh = self.looks_like_an_index()
            && self.recorded_tree().as_ref() == Some(&seen_tree.cloned());
        if fresh {
            return Ok(false);
        }
        self.seed(seen_tree)?;
        Ok(true)
    }

    /// `read-tree <tree>` (or `--empty`) into the private index, then record the marker.
    pub fn seed(&self, seen_tree: Option<&Oid>) -> Result<(), IndexError> {
        let _ = std::fs::remove_file(&self.index_tree);
        let args: Vec<&str> = match seen_tree {
            Some(t) => vec!["read-tree", t.as_str()],
            None => vec!["read-tree", "--empty"],
        };
        self.run_with_lock_retry(&args)?;
        let marker = match seen_tree {
            Some(t) => format!("{t}\n"),
            None => "empty\n".to_string(),
        };
        let tmp = self.index_tree.with_extension("tree.tmp");
        std::fs::write(&tmp, marker).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, &self.index_tree).map_err(|e| io_err(&tmp, e))?;
        Ok(())
    }

    /// `update-index -q --refresh --ignore-submodules`; the exit status is ignored (non-zero
    /// when files differ). Returns `false` when the refresh did not run (lock contention,
    /// or git could not be spawned): the scan then proceeds unrefreshed and over-shows.
    pub fn refresh(&self) -> bool {
        for attempt in 0..=LOCK_RETRIES {
            match self.git.run_raw(
                None,
                &["update-index", "-q", "--refresh", "--ignore-submodules"],
                None,
            ) {
                Ok(out) if lock_held(&out.stderr) => {
                    if attempt < LOCK_RETRIES {
                        std::thread::sleep(LOCK_BACKOFF);
                    }
                }
                Ok(_) => return true,
                Err(_) => return false,
            }
        }
        false
    }

    fn run_with_lock_retry(&self, args: &[&str]) -> Result<(), GitError> {
        let mut last = None;
        for attempt in 0..=LOCK_RETRIES {
            match self.git.run_raw(None, args, None) {
                Ok(out) if out.success() => return Ok(()),
                Ok(out) if lock_held(&out.stderr) => {
                    last = Some(GitError::Failed {
                        argv: args.iter().map(|s| s.to_string()).collect(),
                        cwd: self.git.root().to_path_buf(),
                        status: out.status,
                        stderr: out.stderr_lossy(),
                    });
                    if attempt < LOCK_RETRIES {
                        std::thread::sleep(LOCK_BACKOFF);
                    }
                }
                Ok(out) => {
                    return Err(GitError::Failed {
                        argv: args.iter().map(|s| s.to_string()).collect(),
                        cwd: self.git.root().to_path_buf(),
                        status: out.status,
                        stderr: out.stderr_lossy(),
                    });
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("retried at least once"))
    }

    /// `diff-files -z --ignore-submodules=all`: every path whose content or mode differs
    /// from the index (i.e. from the seen tree), with the index side's mode and oid.
    pub fn diff_files(&self) -> Result<Vec<DiffEntry>, IndexError> {
        let args = ["diff-files", "-z", "--ignore-submodules=all"];
        let out = self.git.run(&args)?;
        git::parse_diff_z(&out).map_err(|m| parse_err(&args, m))
    }

    /// `ls-files -c core.ignorecase=false --others -z` with the user's excludes for git
    /// roots (`--exclude-standard` + `--exclude-from=<user info/exclude>`), none for drafts.
    pub fn others(&self, scope: Option<&DraftScope>) -> Result<Vec<Other>, IndexError> {
        let args = self.others_args(scope);
        let out = self.git.run(&args)?;
        Ok(git::split_nul(&out)
            .into_iter()
            .filter_map(|rec| {
                if let Some(dir) = rec.strip_suffix(b"/") {
                    // Only a folder with a `.git` entry is another repository. Under
                    // `--directory` git names ordinary folders the same way, so without
                    // this check an untracked scratch folder would be reported as a
                    // repository and then opened as one.
                    self.is_repo_dir(dir)
                        .then(|| Other::NestedRepo(dir.to_vec()))
                } else {
                    Some(Other::File(rec.to_vec()))
                }
            })
            .collect())
    }

    /// The arguments `others` runs, kept apart so a test can read them.
    fn others_args(&self, scope: Option<&DraftScope>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-c".into(),
            "core.ignorecase=false".into(),
            "ls-files".into(),
            "--others".into(),
            "-z".into(),
        ];
        if self.kind == RootKind::Git {
            args.push("--exclude-standard".into());
            if let Some(f) = &self.exclude_from
                && f.is_file()
            {
                args.push(format!("--exclude-from={}", f.display()));
            }
        }
        // A folder watched on its own: `--directory` makes git answer with the folder
        // names it finds rather than opening them, so the listing costs one `read_dir`
        // however much sits below. Deliberately **without** `--no-empty-directory`: that
        // flag is what would make git look inside each one to see whether it is empty.
        if scope.is_some_and(|s| !s.recursive) {
            args.push("--directory".into());
        }
        args
    }

    /// Whether the root-relative folder `dir` holds a `.git` entry of either shape. An
    /// `lstat` that fails for any reason other than "not there" answers `true`: reporting
    /// a folder we cannot read as another repository leaves it alone, which is the safe
    /// direction.
    fn is_repo_dir(&self, dir: &[u8]) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let full = self
            .git
            .root()
            .join(std::ffi::OsStr::from_bytes(dir))
            .join(".git");
        match std::fs::symlink_metadata(&full) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }

    /// `ls-files --stage -z` of the private index (the case rule needs the index's names).
    pub fn entries(&self) -> Result<Vec<git::StageEntry>, IndexError> {
        let args = ["ls-files", "--stage", "-z"];
        let out = self.git.run(&args)?;
        git::parse_ls_files_stage_z(&out).map_err(|m| parse_err(&args, m))
    }
}

fn lock_held(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    s.contains("index.lock") || s.contains("Unable to create") || s.contains("File exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoGit;
    use crate::store::tests::fixture_env;
    use crate::store::{RepoFacts, RootKind, Store};
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    fn setup(repo: &FixtureRepo, state: &TempDir) -> (Store, PrivateIndex, Oid) {
        let env = fixture_env(repo, state);
        let paths = RepoPaths::under(state.join("repo"));
        let rg = RepoGit::new(&env, repo.path());
        let config = rg.config_list().unwrap();
        let facts = RepoFacts::read(&rg, &config).unwrap();
        let (store, _) =
            Store::open(&env, repo.path(), RootKind::Git, &paths, Some(&facts)).unwrap();
        let exclude = rg.git_path("info/exclude").unwrap();
        let index = PrivateIndex::new(store.git().clone(), &paths, RootKind::Git, Some(exclude));
        let tree = Oid::parse(repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap().trim()).unwrap();
        (store, index, tree)
    }

    #[test]
    fn index_seed_marker_and_reseed_rules() {
        let repo = FixtureRepo::new("idx").unwrap();
        let state = TempDir::new("lc-index");
        let (_store, index, tree) = setup(&repo, &state);
        assert!(index.ensure(Some(&tree)).unwrap(), "first ensure seeds");
        assert_eq!(index.recorded_tree(), Some(Some(tree.clone())));
        assert!(!index.ensure(Some(&tree)).unwrap(), "fresh: no reseed");
        assert!(index.ensure(None).unwrap(), "tree changed: reseed");
        assert_eq!(index.recorded_tree(), Some(None));
        assert!(index.entries().unwrap().is_empty());
        std::fs::remove_file(index.path()).unwrap();
        assert!(index.ensure(None).unwrap(), "missing index: reseed");
        index.seed(Some(&tree)).unwrap();
        assert_eq!(index.entries().unwrap().len(), 3);
        for garbage in [&b""[..], b"not an index at all, but long enough to pass"] {
            std::fs::write(index.path(), garbage).unwrap();
            assert!(index.ensure(Some(&tree)).unwrap(), "garbage index: reseed");
            assert_eq!(index.entries().unwrap().len(), 3);
            index.refresh();
            assert!(index.diff_files().unwrap().is_empty());
        }
    }

    #[test]
    fn index_diff_files_and_others_with_user_excludes() {
        let repo = FixtureRepo::new("idx2").unwrap();
        let state = TempDir::new("lc-index");
        let (_store, index, tree) = setup(&repo, &state);
        index.seed(Some(&tree)).unwrap();
        assert!(index.refresh());
        assert!(index.diff_files().unwrap().is_empty());
        assert!(index.others(None).unwrap().is_empty());

        repo.write("f1", "changed\n");
        repo.remove("f3");
        repo.write("new.txt", "n\n");
        repo.write("ignored.log", "x\n");
        repo.write(".git/info/exclude", "*.log\n");
        repo.git(&["init", "-q", "nested"]).unwrap();
        repo.write("nested/x", "x\n");
        index.refresh();
        let diff = index.diff_files().unwrap();
        let mut got: Vec<(char, &[u8])> =
            diff.iter().map(|d| (d.status, d.path.as_slice())).collect();
        got.sort();
        assert_eq!(got, vec![('D', &b"f3"[..]), ('M', &b"f1"[..])]);
        let others = index.others(None).unwrap();
        assert_eq!(
            others,
            vec![
                Other::NestedRepo(b"nested".to_vec()),
                Other::File(b"new.txt".to_vec()),
            ],
            "user info/exclude honored, nested repo reported as a dir"
        );
    }

    /// Amendment v1.13 R1, R2 and design review F6: a folder watched on its own is listed
    /// with `--directory`, so git names the folders it finds instead of opening them, and
    /// never with `--no-empty-directory`, which is the flag that would make it look inside.
    /// A named folder is another repository only when it holds a `.git`.
    #[test]
    fn index_others_for_a_watched_folder_names_folders_and_checks_for_a_dot_git() {
        let dir = TempDir::new("lc-index-draft");
        let root = dir.mkdir("notes");
        dir.write("notes/a.md", "a\n");
        dir.write("notes/sub/b.md", "b\n");
        dir.write("notes/sub/deep/c.md", "c\n");
        let state = dir.mkdir("state");
        let env = crate::env::Env::empty(dir.path())
            .with_home(dir.mkdir("home"))
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1");
        assert!(
            crate::git::base_command(&env, &root)
                .args(["init", "-q", "nested"])
                .status()
                .unwrap()
                .success()
        );
        dir.write("notes/nested/x", "x\n");
        let paths = RepoPaths::under(state.join("repo"));
        let (store, _) = Store::open(&env, &root, RootKind::Draft, &paths, None).unwrap();
        let index = PrivateIndex::new(store.git().clone(), &paths, RootKind::Draft, None);
        index.ensure(None).unwrap();
        index.refresh();

        let plain = DraftScope::plain(1 << 20);
        let tree = DraftScope::tree(1 << 20);
        let args = index.others_args(Some(&plain));
        assert!(
            args.iter().any(|a| a == "--directory"),
            "a folder watched on its own is named, not opened: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--no-empty-directory"),
            "that flag is what would open each folder: {args:?}"
        );
        assert!(
            !index
                .others_args(Some(&tree))
                .iter()
                .any(|a| a == "--directory"),
            "the tree scope keeps the existing walk"
        );
        assert!(
            !index.others_args(None).iter().any(|a| a == "--directory"),
            "a repository keeps the existing walk"
        );

        assert_eq!(
            index.others(Some(&plain)).unwrap(),
            vec![
                Other::File(b"a.md".to_vec()),
                Other::NestedRepo(b"nested".to_vec()),
            ],
            "`sub/` is an ordinary folder outside the scope, never a repository"
        );
        assert_eq!(
            index.others(Some(&tree)).unwrap(),
            vec![
                Other::File(b"a.md".to_vec()),
                Other::NestedRepo(b"nested".to_vec()),
                Other::File(b"sub/b.md".to_vec()),
                Other::File(b"sub/deep/c.md".to_vec()),
            ]
        );
    }
}
