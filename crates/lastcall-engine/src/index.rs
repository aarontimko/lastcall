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
use crate::store::RootKind;

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

    /// Reseed unless the index exists and `index.tree` matches `seen_tree`.
    pub fn ensure(&self, seen_tree: Option<&Oid>) -> Result<bool, IndexError> {
        let fresh =
            self.index.is_file() && self.recorded_tree().as_ref() == Some(&seen_tree.cloned());
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
    /// when files differ). Returns `false` when the refresh was skipped (lock contention).
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
                Ok(_) | Err(_) => return true,
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
    pub fn others(&self) -> Result<Vec<Other>, IndexError> {
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
        let out = self.git.run(&args)?;
        Ok(git::split_nul(&out)
            .into_iter()
            .map(|rec| {
                if rec.ends_with(b"/") {
                    Other::NestedRepo(rec[..rec.len() - 1].to_vec())
                } else {
                    Other::File(rec.to_vec())
                }
            })
            .collect())
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
    use crate::store::{RootKind, Store};
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    fn setup(repo: &FixtureRepo, state: &TempDir) -> (Store, PrivateIndex, Oid) {
        let env = fixture_env(repo, state);
        let paths = RepoPaths::under(state.join("repo"));
        let rg = RepoGit::new(&env, repo.path());
        let (store, _) = Store::open(&env, repo.path(), RootKind::Git, &paths, Some(&rg)).unwrap();
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
    }

    #[test]
    fn index_diff_files_and_others_with_user_excludes() {
        let repo = FixtureRepo::new("idx2").unwrap();
        let state = TempDir::new("lc-index");
        let (_store, index, tree) = setup(&repo, &state);
        index.seed(Some(&tree)).unwrap();
        assert!(index.refresh());
        assert!(index.diff_files().unwrap().is_empty());
        assert!(index.others().unwrap().is_empty());

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
        let others = index.others().unwrap();
        assert_eq!(
            others,
            vec![
                Other::NestedRepo(b"nested".to_vec()),
                Other::File(b"new.txt".to_vec()),
            ],
            "user info/exclude honored, nested repo reported as a dir"
        );
    }
}
