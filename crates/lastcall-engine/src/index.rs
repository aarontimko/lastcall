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
//!
//! **Seeding happens only under the ledger lock** (Phase 13, §11's entry on the index and
//! its marker). A seed is three steps, and two seeds that interleave leave the index
//! holding one tree under a marker that names another, which hides a file put back to the
//! first tree's content. There are exactly two ways in: [`PrivateIndex::seed_committed`],
//! for a fold that wrote the ledger under the lock it hands over, and
//! [`PrivateIndex::ensure`] / [`PrivateIndex::reseed`], which take the lock themselves,
//! check the ledger **on disk** still names the tree they are about to seed, and drop the
//! guard before they return. `refresh` is untouched: the rule is "seeded only under the
//! lock", not "written only under the lock", because git serialises index writes itself
//! through `index.lock`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::git::{self, DiffEntry, GitError, Oid, StoreGit};
use crate::ledger::{self, LedgerError, LedgerLock};
use crate::paths::RepoPaths;
use crate::store::{DraftScope, RootKind};

const LOCK_RETRIES: u32 = 3;
const LOCK_BACKOFF: Duration = Duration::from_millis(50);

/// How long a seed waits for the **ledger** lock: the house budget, 40 × 50 ms = 2 s.
/// Only a fold and a stale scan pay it; a scan that finds the marker fresh takes no lock.
const DEFAULT_LEDGER_LOCK: (u32, Duration) = (ledger::LOCK_RETRIES, ledger::LOCK_BACKOFF);

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
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// The ledger on disk names a seen tree this process does not hold, so the tree it was
    /// about to seed is out of date. Nothing was touched; the caller reloads and retries.
    #[error("the ledger on disk names another seen tree; reload and scan again")]
    LedgerMoved,
}

/// Identity of `index.tree` as a reader saw it: mtime, length and inode. Every seed writes
/// the marker by rename, so a different identity means the index was reseeded in between.
/// Identity and not content: a later fold can return to an earlier tree.
pub type MarkerId = (SystemTime, u64, u64);

/// What [`PrivateIndex::ensure`] did, and the identity of the marker it decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensured {
    /// Whether this call reseeded the index.
    pub seeded: bool,
    /// The marker as it stood when `ensure` returned. A scan stats it again after its last
    /// read of the index and treats any difference as "reseeded under me".
    pub marker: Option<MarkerId>,
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
    paths: RepoPaths,
    kind: RootKind,
    /// The user's `info/exclude` (common dir), passed as `--exclude-from` for git roots.
    exclude_from: Option<PathBuf>,
    /// How long a seed waits for the ledger lock. The shipping value is
    /// [`DEFAULT_LEDGER_LOCK`]; a test that wants the busy path shortens it rather than
    /// sleeping for two seconds.
    lock_budget: (u32, Duration),
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
            paths: paths.clone(),
            kind,
            exclude_from,
            lock_budget: DEFAULT_LEDGER_LOCK,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_lock_budget(&mut self, budget: (u32, Duration)) {
        self.lock_budget = budget;
    }

    pub fn path(&self) -> &Path {
        &self.paths.index
    }

    /// The tree recorded by the last successful seed (`None` when missing/unreadable).
    pub fn recorded_tree(&self) -> Option<Option<Oid>> {
        let text = std::fs::read_to_string(&self.paths.index_tree).ok()?;
        let text = text.trim();
        if text == "empty" {
            Some(None)
        } else {
            Oid::parse(text).map(Some)
        }
    }

    /// The marker file's identity, or `None` when it is not there.
    pub fn marker_id(&self) -> Option<MarkerId> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&self.paths.index_tree).ok()?;
        Some((m.modified().ok()?, m.len(), m.ino()))
    }

    /// A quick sanity check of the cache file: the `DIRC` magic and room for the header
    /// and the trailing checksum. An empty or garbage file fails it and is reseeded.
    fn looks_like_an_index(&self) -> bool {
        let Ok(mut f) = std::fs::File::open(&self.paths.index) else {
            return false;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut magic = [0u8; 4];
        len >= 32 && std::io::Read::read_exact(&mut f, &mut magic).is_ok() && &magic == b"DIRC"
    }

    /// Whether the index exists, looks like an index, and `index.tree` matches `seen_tree`.
    fn is_fresh(&self, seen_tree: Option<&Oid>) -> bool {
        self.looks_like_an_index() && self.recorded_tree().as_ref() == Some(&seen_tree.cloned())
    }

    /// Reseed unless the index is already the one `seen_tree` asks for.
    ///
    /// A fresh index costs no lock: the marker is read, the index's header is checked, and
    /// that is the whole of the common path. A stale one takes the ledger lock, looks at
    /// the marker once more (another pane's fold may have seeded it while this call
    /// waited, and a redundant `read-tree` zeroes git's stat data and forces a whole-tree
    /// content refresh), checks the ledger on disk, seeds, and releases the lock before it
    /// returns, so no guard ever outlives the call.
    ///
    /// The marker's identity is read **before** its content, so a reseed that lands
    /// between the two makes the caller's later check disagree rather than agree.
    pub fn ensure(&self, seen_tree: Option<&Oid>) -> Result<Ensured, IndexError> {
        let marker = self.marker_id();
        if self.is_fresh(seen_tree) {
            return Ok(Ensured {
                seeded: false,
                marker,
            });
        }
        let _lock = self.ledger_lock()?;
        if self.is_fresh(seen_tree) {
            return Ok(Ensured {
                seeded: false,
                marker: self.marker_id(),
            });
        }
        self.seed_checked(seen_tree)?;
        Ok(Ensured {
            seeded: true,
            marker: self.marker_id(),
        })
    }

    /// Seed unconditionally, under the lock and against the ledger on disk: the scan's
    /// retry after an unreadable index, where the marker can be fresh and the index still
    /// unusable, so [`PrivateIndex::ensure`]'s second look would decline the work. Returns
    /// the new marker's identity.
    pub fn reseed(&self, seen_tree: Option<&Oid>) -> Result<Option<MarkerId>, IndexError> {
        let _lock = self.ledger_lock()?;
        self.seed_checked(seen_tree)?;
        Ok(self.marker_id())
    }

    /// The fold's seed. The caller wrote `tree` into the ledger under the very lock it
    /// passes here, so the tree is the one the disk names by construction and there is
    /// nothing to check; taking the guard by reference is what makes that a compile-time
    /// fact rather than a comment.
    pub fn seed_committed(&self, _lock: &LedgerLock, tree: Option<&Oid>) -> Result<(), IndexError> {
        self.seed(tree)
    }

    /// Remove the marker, best effort. A fold whose seed failed leaves **no** marker
    /// rather than one naming a tree the index does not hold.
    pub fn forget_marker(&self) {
        let _ = std::fs::remove_file(&self.paths.index_tree);
    }

    fn ledger_lock(&self) -> Result<LedgerLock, IndexError> {
        Ok(LedgerLock::acquire_with(
            &self.paths,
            self.lock_budget.0,
            self.lock_budget.1,
        )?)
    }

    /// Seed, but only the tree the ledger **on disk** names. Callers hold the ledger lock.
    ///
    /// The comparison is the tree and never the stamp: the stamp moves on every flag,
    /// snooze and plain accept, none of which move the tree, and `reload_ledger_if_changed`
    /// can hold a new stamp over old content. The read is `std::fs::read` plus
    /// [`ledger::parse`] and never [`ledger::load`], which moves an unparsable file aside.
    ///
    /// An empty index (`None`) is always allowed: it over-shows, and it is what a process
    /// whose store lacks the tree the disk names has to fall back to.
    fn seed_checked(&self, seen_tree: Option<&Oid>) -> Result<(), IndexError> {
        if let Some(tree) = seen_tree {
            match std::fs::read(&self.paths.ledger) {
                Ok(bytes) => {
                    let (disk, _) = ledger::parse(&bytes).map_err(|_| IndexError::LedgerMoved)?;
                    if disk.seen_tree.as_ref() != Some(tree) {
                        return Err(IndexError::LedgerMoved);
                    }
                }
                // No ledger at all: every writer of a seen tree saves `ledger.json` under
                // this same lock before it seeds, so an absent file can only mean nothing
                // has ever been folded for this root and there is no newer tree to be
                // stale against. (A ledger being moved aside by another process is the one
                // other way to see this, and that process is about to open the root with
                // nothing seen, which over-shows and reseeds again.)
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(IndexError::LedgerMoved),
            }
        }
        self.seed(seen_tree)
    }

    /// `read-tree <tree>` (or `--empty`) into the private index, then record the marker.
    /// Private: every seed goes through one of the two entry points above, so none can
    /// happen without the ledger lock.
    fn seed(&self, seen_tree: Option<&Oid>) -> Result<(), IndexError> {
        // A removal that fails for any reason other than "not there" aborts before
        // `read-tree`: otherwise "removal failed, read-tree succeeded, marker write
        // failed" leaves the new index under the old marker, which is the defect's own
        // end state.
        match std::fs::remove_file(&self.paths.index_tree) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&self.paths.index_tree, e)),
        }
        let args: Vec<&str> = match seen_tree {
            Some(t) => vec!["read-tree", t.as_str()],
            None => vec!["read-tree", "--empty"],
        };
        self.run_with_lock_retry(&args)?;
        #[cfg(test)]
        SEED_HOOK.with(|h| h.fire(seen_tree.cloned()));
        self.write_marker(seen_tree)
    }

    /// Record the tree the index was seeded from: temp file, then `rename`.
    fn write_marker(&self, seen_tree: Option<&Oid>) -> Result<(), IndexError> {
        let marker = match seen_tree {
            Some(t) => format!("{t}\n"),
            None => "empty\n".to_string(),
        };
        // One name per process and per write: two seeds of one root at the same moment
        // must never share a temp file, or the second `rename` finds it already moved.
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let n = WRITES.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .paths
            .index_tree
            .with_extension(format!("tree.{}-{n}.tmp", std::process::id()));
        // A failed write leaves nothing behind: the name is new each time, so a leftover
        // would not be overwritten by the next attempt (a full disk, one orphan per scan).
        let written = std::fs::write(&tmp, marker)
            .and_then(|()| std::fs::rename(&tmp, &self.paths.index_tree))
            .map_err(|e| io_err(&tmp, e));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
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

#[cfg(test)]
thread_local! {
    /// Between `read-tree` and the marker write: the window the two-seed interleave of
    /// §11 opens (Phase 13 deliverable B). The argument is the tree being seeded.
    pub(crate) static SEED_HOOK: crate::testhook::TestHook<Option<Oid>> =
        const { crate::testhook::TestHook::new() };
}

fn lock_held(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    s.contains("index.lock") || s.contains("Unable to create") || s.contains("File exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoGit;
    use crate::ledger::{Ledger, SeenAt};
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

    /// Two seeds of one root at the same moment (two lastcall processes, or two engines in
    /// one) each write the marker. With one shared temp name the second `rename` found its
    /// temp file already moved and the scan failed with "No such file or directory".
    #[test]
    fn index_marker_writes_at_the_same_moment_both_succeed() {
        let repo = FixtureRepo::new("idx-marker").unwrap();
        let state = TempDir::new("lc-index-marker");
        let (_store, index, tree) = setup(&repo, &state);
        std::thread::scope(|s| {
            let writers: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        for _ in 0..500 {
                            index.write_marker(Some(&tree)).unwrap();
                        }
                    })
                })
                .collect();
            for w in writers {
                w.join().unwrap();
            }
        });
        assert_eq!(index.recorded_tree(), Some(Some(tree.clone())));

        // No temp file outlives its write, and a write that fails removes its own.
        let leftovers = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        };
        let dir = index.paths.index_tree.parent().unwrap().to_path_buf();
        assert_eq!(leftovers(&dir), Vec::<String>::new());
        std::fs::remove_file(&index.paths.index_tree).unwrap();
        std::fs::create_dir(&index.paths.index_tree).unwrap();
        std::fs::write(index.paths.index_tree.join("in-the-way"), "x").unwrap();
        index
            .write_marker(Some(&tree))
            .expect_err("rename over a non-empty folder fails");
        assert_eq!(leftovers(&dir), Vec::<String>::new());
    }

    /// Phase 13 deliverable B, item 3: under the lock, a seed happens only for the tree
    /// the ledger **on disk** names. An empty index is always allowed (it over-shows), an
    /// unparsable ledger is never read through [`ledger::load`] (which would move it
    /// aside), and a refusal touches nothing.
    #[test]
    fn index_seeds_only_the_tree_the_ledger_on_disk_names() {
        let repo = FixtureRepo::new("idx-ledger").unwrap();
        let state = TempDir::new("lc-index-ledger");
        let (store, index, tree_x) = setup(&repo, &state);
        let paths = RepoPaths::under(state.join("repo"));
        repo.write("f1", "another tree\n");
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-q", "-m", "y"]).unwrap();
        let tree_y = Oid::parse(repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap().trim()).unwrap();
        assert!(store.exists(&tree_y));

        let save = |tree: Option<&Oid>| {
            let l = Ledger::new(
                repo.path(),
                RootKind::Git,
                tree.cloned(),
                SeenAt {
                    head_commit: None,
                    branch: None,
                    at: "2026-01-01T00:00:00Z".into(),
                },
            );
            ledger::save(&paths, &l).unwrap();
        };

        // The disk says Y; a process that still believes in X may not seed X.
        save(Some(&tree_y));
        assert!(
            matches!(index.ensure(Some(&tree_x)), Err(IndexError::LedgerMoved)),
            "a stale tree is refused"
        );
        assert_eq!(index.recorded_tree(), None, "and nothing was touched");
        assert!(
            matches!(index.reseed(Some(&tree_x)), Err(IndexError::LedgerMoved)),
            "the scan's retry is refused on the same terms"
        );

        // The tree the disk names, and the empty index, are both allowed.
        assert!(index.ensure(Some(&tree_y)).unwrap().seeded);
        assert_eq!(index.recorded_tree(), Some(Some(tree_y.clone())));
        assert!(index.ensure(None).unwrap().seeded, "empty over-shows");

        // An unparsable ledger refuses too, and is left exactly where it is: `load` would
        // have moved it aside, which is a write no scan is entitled to make.
        std::fs::write(&paths.ledger, b"{ not json").unwrap();
        assert!(matches!(
            index.ensure(Some(&tree_y)),
            Err(IndexError::LedgerMoved)
        ));
        assert_eq!(std::fs::read(&paths.ledger).unwrap(), b"{ not json");
        assert_eq!(index.recorded_tree(), Some(None), "still the empty marker");

        // No ledger at all: nothing has ever been folded here, so there is no newer tree
        // to be stale against and the seed goes ahead.
        std::fs::remove_file(&paths.ledger).unwrap();
        assert!(index.ensure(Some(&tree_y)).unwrap().seeded);
    }

    /// G5: a marker that turned fresh while the scan waited for the ledger lock is not
    /// seeded again. A redundant `read-tree` zeroes git's stat data and forces a
    /// whole-tree content refresh, under the lock, which is the slowness Phase 12 removed.
    ///
    /// No sleep and no fourth seam. This thread takes the lock **before** the waiter
    /// starts, so the waiter either reads the marker while it is still absent and then
    /// blocks on the lock (the case under test), or reads it after the seed and takes the
    /// fast path with no lock at all. Both interleavings have the same answer, and it is
    /// the one asserted: the waiter seeds nothing and the marker is still the one written
    /// here, which a reseed would have replaced by `rename` and so given a new identity.
    #[test]
    fn index_a_marker_that_turned_fresh_while_the_scan_waited_is_not_reseeded() {
        let repo = FixtureRepo::new("idx-turned-fresh").unwrap();
        let state = TempDir::new("lc-index-turned-fresh");
        let (_store, index, tree) = setup(&repo, &state);
        let paths = RepoPaths::under(state.join("repo"));
        assert!(index.marker_id().is_none(), "the premise: no marker yet");

        let (ensured, seeded) = std::thread::scope(|s| {
            let lock = LedgerLock::acquire_with(&paths, ledger::LOCK_RETRIES, ledger::LOCK_BACKOFF)
                .unwrap();
            let waiter = s.spawn(|| index.ensure(Some(&tree)));
            index.seed_committed(&lock, Some(&tree)).unwrap();
            let seeded = index.marker_id();
            drop(lock);
            (waiter.join().unwrap().unwrap(), seeded)
        });
        assert!(
            !ensured.seeded,
            "the second look under the lock declined the work"
        );
        assert_eq!(
            ensured.marker, seeded,
            "and it reported the marker this thread wrote"
        );
        assert_eq!(index.marker_id(), seeded, "which is still the one on disk");
    }

    /// G6: a marker that cannot be removed aborts the seed **before** `read-tree`.
    /// Otherwise "removal failed, read-tree succeeded, marker write failed" leaves the new
    /// index under the old marker, which is the defect's own end state.
    #[test]
    fn index_seed_aborts_when_the_marker_cannot_be_removed() {
        let repo = FixtureRepo::new("idx-marker-stuck").unwrap();
        let state = TempDir::new("lc-index-marker-stuck");
        let (_store, index, tree) = setup(&repo, &state);
        index.reseed(Some(&tree)).unwrap();
        let before = std::fs::read(index.path()).unwrap();

        std::fs::remove_file(&index.paths.index_tree).unwrap();
        std::fs::create_dir(&index.paths.index_tree).unwrap();
        std::fs::write(index.paths.index_tree.join("in-the-way"), "x").unwrap();
        let err = index
            .reseed(None)
            .expect_err("a directory cannot be removed");
        assert!(matches!(err, IndexError::Io { .. }), "{err:?}");
        assert_eq!(
            std::fs::read(index.path()).unwrap(),
            before,
            "the index is untouched: no `read-tree` ran"
        );
    }

    #[test]
    fn index_seed_marker_and_reseed_rules() {
        let repo = FixtureRepo::new("idx").unwrap();
        let state = TempDir::new("lc-index");
        let (_store, index, tree) = setup(&repo, &state);
        assert!(
            index.ensure(Some(&tree)).unwrap().seeded,
            "first ensure seeds"
        );
        assert_eq!(index.recorded_tree(), Some(Some(tree.clone())));
        assert!(
            !index.ensure(Some(&tree)).unwrap().seeded,
            "fresh: no reseed"
        );
        assert!(index.ensure(None).unwrap().seeded, "tree changed: reseed");
        assert_eq!(index.recorded_tree(), Some(None));
        assert!(index.entries().unwrap().is_empty());
        std::fs::remove_file(index.path()).unwrap();
        assert!(index.ensure(None).unwrap().seeded, "missing index: reseed");
        index.reseed(Some(&tree)).unwrap();
        assert_eq!(index.entries().unwrap().len(), 3);
        for garbage in [&b""[..], b"not an index at all, but long enough to pass"] {
            std::fs::write(index.path(), garbage).unwrap();
            assert!(
                index.ensure(Some(&tree)).unwrap().seeded,
                "garbage index: reseed"
            );
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
        index.reseed(Some(&tree)).unwrap();
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
