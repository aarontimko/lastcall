//! Accept operations — all compare-and-swap (docs/spec/00-spec.md §6.3) — plus compaction
//! and flags (kickoff deliverable 7).
//!
//! Every op takes **rendered tokens** from a scan's rows, never fresh reads: `Rendered`
//! pins the oid and mode the user looked at, and (for hunks) the baseline the hunks were
//! computed against. An accept whose live content, mode, or baseline moved is *refused*
//! and writes nothing; the next scan shows the new state.
//!
//! One fold drives accept-all and compaction: `read-tree <seen_tree>` ⊕ every override ⊕
//! every row of the snapshot → `write-tree` → the new seen tree; overrides lose their
//! `blob`/`mode`. Accept-all also moves `seen_at`; compaction never does.
//!
//! **Test seam (the only one in product code):** every op takes a `&dyn FaultInjector`
//! that is consulted after the object write and after the ledger temp write. Production
//! passes [`NoFault`]; the testkit's implementation SIGKILLs the process (E1).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use crate::git::{GitError, Mode, Oid, RepoGit};
use crate::headstate::current_head;
use crate::hunks::{self, Hunk};
use crate::index::{IndexError, PrivateIndex};
use crate::ledger::{
    self, Baseline, BaselineResolver, Clock, Flag, Ledger, LedgerError, LedgerLock, LoadResult,
    Override, SeenAt, TreeEntries,
};
use crate::paths::RepoPaths;
use crate::scan::{Entry, Pile, Row};
use crate::store::{Current, RootKind, Store, StoreError, TreeWrite};

/// Where a fault injector may fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// The new object is in the store; the ledger has not been touched.
    AfterObjectWrite,
    /// `ledger.json.tmp` is written and synced; the rename has not happened.
    AfterLedgerTmpWrite,
}

/// The E1 test seam. Production uses [`NoFault`].
pub trait FaultInjector {
    fn at(&self, point: FaultPoint);
}

/// Never fires.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFault;

impl FaultInjector for NoFault {
    fn at(&self, _point: FaultPoint) {}
}

/// What the user looked at when they pressed accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub path: Vec<u8>,
    /// `None` for a deletion row.
    pub oid: Option<Oid>,
    pub mode: Option<Mode>,
    /// The baseline oid the row's hunks were computed against (`None` = absent/empty).
    pub baseline: Option<Oid>,
    /// The baseline mode alongside it: a mode-only hunk (D1) moves the mode and not the
    /// oid, so the hunk CAS must cover both.
    pub baseline_mode: Option<Mode>,
}

impl Rendered {
    pub fn of(row: &Row) -> Self {
        Self {
            path: row.path.clone(),
            oid: row.current.as_ref().map(|e| e.oid.clone()),
            mode: row.current.as_ref().map(|e| e.mode),
            baseline: row.baseline.as_ref().map(|e| e.oid.clone()),
            baseline_mode: row.baseline.as_ref().map(|e| e.mode),
        }
    }
}

/// Why an accept did not happen. Never an error: the pile simply re-renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Live content or mode differs from what was rendered (A6).
    Moved { path: Vec<u8>, live: Option<Entry> },
    /// The baseline the hunks were computed against has changed (§6.3 hunk CAS).
    BaselineMoved { path: Vec<u8> },
    /// The path cannot be hashed (typechange, EACCES, failing filter).
    Unhashable { path: Vec<u8>, reason: String },
    /// `accept_deletion` on a path that still exists (A7).
    StillPresent { path: Vec<u8> },
    /// Non-UTF-8 paths cannot be keyed in the ledger in v1.
    NonUtf8Path { path: Vec<u8> },
    /// `accept_hunk` with an index the row does not have.
    NoSuchHunk { path: Vec<u8>, index: usize },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lossy = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
        match self {
            Refused::Moved { path, .. } => {
                write!(f, "{}: changed since rendered; not accepted", lossy(path))
            }
            Refused::BaselineMoved { path } => {
                write!(
                    f,
                    "{}: baseline moved since rendered; not accepted",
                    lossy(path)
                )
            }
            Refused::Unhashable { path, reason } => {
                write!(f, "{}: cannot hash ({reason}); not accepted", lossy(path))
            }
            Refused::StillPresent { path } => {
                write!(f, "{}: still present; deletion not accepted", lossy(path))
            }
            Refused::NonUtf8Path { path } => {
                write!(
                    f,
                    "{}: non-UTF-8 path; accept unsupported in v1",
                    lossy(path)
                )
            }
            Refused::NoSuchHunk { path, index } => {
                write!(f, "{}: no hunk {index}", lossy(path))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// The result of one op: refusals (empty on success) and whether a compaction ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub refused: Vec<Refused>,
    pub compacted: bool,
    /// Whether the ledger was written.
    pub written: bool,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.refused.is_empty()
    }
}

/// Everything the ops need for one root. The engine constructs one per call.
pub struct Ops<'a> {
    pub store: &'a Store,
    pub index: &'a PrivateIndex,
    /// `Some` for git roots (accept-all records the current commit in `seen_at`).
    pub repo: Option<&'a RepoGit>,
    pub paths: &'a RepoPaths,
    pub ledger: &'a mut Ledger,
    /// `ls-tree -r` of the effective seen tree; refreshed by the engine after a fold.
    pub tree: &'a mut TreeEntries,
    pub clock: &'a dyn Clock,
    /// Compaction runs after a write when more overrides than this carry a `blob`.
    pub compaction_threshold: usize,
    /// Override changes made in memory since the last commit, keyed by path (`None` =
    /// removed). Start empty: `commit` replays them onto the on-disk ledger under the lock
    /// and clears them, so two engines over one root never lose each other's writes.
    pub staged: BTreeMap<String, Option<Override>>,
}

impl Ops<'_> {
    fn key(path: &[u8]) -> Result<String, Refused> {
        std::str::from_utf8(path)
            .map(str::to_owned)
            .map_err(|_| Refused::NonUtf8Path {
                path: path.to_vec(),
            })
    }

    /// The seen-tree entry for `path`, as an `Entry`.
    fn tree_entry(&self, path: &[u8]) -> Option<Entry> {
        self.tree.get(path).map(|(mode, oid)| Entry {
            oid: oid.clone(),
            mode: *mode,
        })
    }

    /// Set `{blob, mode}` on `path`'s override, or drop them when that equals the seen
    /// tree (§6.2 clean-up rule). A `flag` survives; an override left with nothing is
    /// removed.
    fn set_override(&mut self, key: &str, blob: Option<Oid>, mode: Option<Mode>) {
        let path = key.as_bytes();
        let tree = self.tree_entry(path);
        let equals_tree = match (&blob, &tree) {
            (Some(b), Some(t)) => *b == t.oid && mode.is_none_or(|m| m == t.mode),
            (None, None) => true,
            _ => false,
        };
        let now = self.clock.now_iso8601();
        let entry = self
            .ledger
            .overrides
            .entry(key.to_owned())
            .or_insert(Override {
                blob: None,
                mode: None,
                flag: None,
                updated_at: now.clone(),
            });
        if equals_tree {
            entry.blob = None;
            entry.mode = None;
        } else {
            entry.blob = Some(blob);
            entry.mode = mode;
        }
        entry.updated_at = now;
        if entry.is_empty() {
            self.ledger.overrides.remove(key);
        }
        self.staged
            .insert(key.to_owned(), self.ledger.overrides.get(key).cloned());
    }

    /// Under the lock: reload the on-disk ledger (another process may have written since
    /// this one was loaded), replay the staged override changes onto it, and adopt it. A
    /// change to the same path by both sides is last-writer-wins; every other path keeps
    /// what the other side wrote. The seen-tree cache follows an on-disk seen tree that
    /// moved (the other side compacted).
    fn merge_from_disk(&mut self) -> Result<(), OpsError> {
        // `Missing` (the file was removed) and `Unreadable` (garbage, moved aside by
        // `load`) both mean the disk holds nothing worth merging: this engine's ledger is
        // written as is.
        let LoadResult::Loaded { ledger: disk, .. } = ledger::load(self.paths, self.clock)? else {
            return Ok(());
        };
        let mut merged = disk;
        for (key, o) in &self.staged {
            match o {
                Some(o) => {
                    merged.overrides.insert(key.clone(), o.clone());
                }
                None => {
                    merged.overrides.remove(key);
                }
            }
        }
        if merged.seen_tree != self.ledger.seen_tree {
            *self.tree = match &merged.seen_tree {
                Some(t) if self.store.exists(t) => self.store.ls_tree(t)?,
                Some(_) => {
                    // The on-disk seen tree is gone (gc?): the same rule as open — nothing
                    // seen — so this write persists `null` and the next fold seeds from
                    // the empty tree instead of failing on every accept.
                    merged.seen_tree = None;
                    TreeEntries::new()
                }
                None => TreeEntries::new(),
            };
        }
        *self.ledger = merged;
        Ok(())
    }

    /// Lock, merge with disk, tmp-write, (fault), rename; then compact when over the
    /// threshold.
    fn commit(&mut self, fault: &dyn FaultInjector) -> Result<bool, OpsError> {
        {
            let _lock = LedgerLock::acquire(self.paths)?;
            self.merge_from_disk()?;
            let tmp = ledger::write_tmp(self.paths, self.ledger)?;
            fault.at(FaultPoint::AfterLedgerTmpWrite);
            ledger::commit_tmp(self.paths, &tmp)?;
        }
        self.staged.clear();
        if self.ledger.blob_override_count() > self.compaction_threshold {
            self.compact(fault)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// CAS on the live path against the rendered oid/mode.
    fn cas_live(&self, rendered: &Rendered) -> Result<Entry, Refused> {
        match self.store.hash_path(&rendered.path) {
            Current::Absent => Err(Refused::Moved {
                path: rendered.path.clone(),
                live: None,
            }),
            Current::Unhashable(reason) => Err(Refused::Unhashable {
                path: rendered.path.clone(),
                reason,
            }),
            Current::Present { oid, mode } => {
                let live = Entry { oid, mode };
                if rendered.oid.as_ref() == Some(&live.oid) && rendered.mode == Some(live.mode) {
                    Ok(live)
                } else {
                    Err(Refused::Moved {
                        path: rendered.path.clone(),
                        live: Some(live),
                    })
                }
            }
        }
    }

    /// Stage one file accept in memory (no ledger write). `Ok(())` means the override is
    /// set; a refusal leaves the ledger untouched.
    fn stage_file(&mut self, rendered: &Rendered) -> Result<(), Refused> {
        let key = Self::key(&rendered.path)?;
        if rendered.oid.is_none() {
            return self.stage_deletion(rendered);
        }
        let live = self.cas_live(rendered)?;
        self.set_override(&key, Some(live.oid), Some(live.mode));
        Ok(())
    }

    fn stage_deletion(&mut self, rendered: &Rendered) -> Result<(), Refused> {
        let key = Self::key(&rendered.path)?;
        let full = self.store.root().join(OsStr::from_bytes(&rendered.path));
        if std::fs::symlink_metadata(&full).is_ok() {
            return Err(Refused::StillPresent {
                path: rendered.path.clone(),
            });
        }
        self.set_override(&key, None, None);
        Ok(())
    }

    /// Accept one file at its rendered content (A6 CAS). A rendered `oid: None` is a
    /// deletion and follows [`Ops::accept_deletion`].
    pub fn accept_file(
        &mut self,
        rendered: &Rendered,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        match self.stage_file(rendered) {
            Ok(()) => {
                let compacted = self.commit(fault)?;
                Ok(Outcome {
                    refused: Vec::new(),
                    compacted,
                    written: true,
                })
            }
            Err(r) => Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            }),
        }
    }

    /// `accept_file` over each row, collecting refusals, one ledger write (C2).
    pub fn accept_group(
        &mut self,
        rows: &[Rendered],
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let mut refused = Vec::new();
        let mut any = false;
        for r in rows {
            match self.stage_file(r) {
                Ok(()) => any = true,
                Err(e) => refused.push(e),
            }
        }
        let compacted = if any { self.commit(fault)? } else { false };
        Ok(Outcome {
            refused,
            compacted,
            written: any,
        })
    }

    /// Accept a deletion: refused while the path still exists (A7); else the override
    /// records `blob: null`.
    pub fn accept_deletion(
        &mut self,
        rendered: &Rendered,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        match self.stage_deletion(rendered) {
            Ok(()) => {
                let compacted = self.commit(fault)?;
                Ok(Outcome {
                    refused: Vec::new(),
                    compacted,
                    written: true,
                })
            }
            Err(r) => Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            }),
        }
    }

    /// Accept one hunk (A3): live CAS as `accept_file` **plus** the baseline CAS; the
    /// baseline with only that hunk applied becomes the override blob.
    pub fn accept_hunk(
        &mut self,
        rendered: &Rendered,
        hunks: &[Hunk],
        hunk_index: usize,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let refuse = |r: Refused| {
            Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            })
        };
        let key = match Self::key(&rendered.path) {
            Ok(k) => k,
            Err(r) => return refuse(r),
        };
        let Some(hunk) = hunks.get(hunk_index) else {
            return refuse(Refused::NoSuchHunk {
                path: rendered.path.clone(),
                index: hunk_index,
            });
        };
        let live = match self.cas_live(rendered) {
            Ok(l) => l,
            Err(r) => return refuse(r),
        };
        let baseline = {
            let mut resolver = BaselineResolver::new(
                self.ledger,
                self.tree,
                self.store,
                std::iter::once(rendered.path.as_slice()),
            );
            resolver.baseline(&rendered.path)
        };
        let (base_oid, base_mode) = match &baseline {
            Baseline::Present { oid, mode } => (Some(oid.clone()), Some(*mode)),
            Baseline::Absent | Baseline::Empty => (None, None),
        };
        // Baseline CAS over (oid, mode): a stale mode hunk must not apply twice.
        if base_oid != rendered.baseline || base_mode != rendered.baseline_mode {
            return refuse(Refused::BaselineMoved {
                path: rendered.path.clone(),
            });
        }
        let (blob, mode) = if hunk.is_mode_change() {
            // D1: the synthetic mode hunk moves only the mode.
            (base_oid, Some(live.mode))
        } else {
            let base_bytes = match &base_oid {
                Some(o) => self.store.cat_blob(o)?,
                None => Vec::new(),
            };
            let spliced = hunks::apply_hunks(&base_bytes, hunks, &[hunk_index]);
            let oid = self.store.hash_bytes(&spliced)?;
            fault.at(FaultPoint::AfterObjectWrite);
            // A content hunk leaves the mode where the baseline had it; without a mode
            // hunk the modes agree and the live mode is that mode.
            let mode = if hunks.iter().any(Hunk::is_mode_change) {
                base_mode.or(Some(live.mode))
            } else {
                Some(live.mode)
            };
            (Some(oid), mode)
        };
        self.set_override(&key, blob, mode);
        let compacted = self.commit(fault)?;
        Ok(Outcome {
            refused: Vec::new(),
            compacted,
            written: true,
        })
    }

    /// Accept everything in `snapshot` at its rendered content (A5), stamp `seen_at`
    /// with the current commit, and reseed the index.
    pub fn accept_all(
        &mut self,
        snapshot: &Pile,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let seen_at = self.head_now();
        self.fold(snapshot, Some(seen_at), fault)?;
        Ok(Outcome {
            refused: Vec::new(),
            compacted: false,
            written: true,
        })
    }

    /// Fold every override into the seen tree; `seen_at` unchanged (§6.2). Pile before ==
    /// pile after.
    pub fn compact(&mut self, fault: &dyn FaultInjector) -> Result<(), OpsError> {
        self.fold(&Pile::empty(), None, fault)
    }

    fn head_now(&self) -> SeenAt {
        let (head_commit, branch) = match self.repo {
            Some(rg) if self.ledger.kind == RootKind::Git => current_head(rg),
            _ => (None, None),
        };
        SeenAt {
            head_commit,
            branch,
            at: self.clock.now_iso8601(),
        }
    }

    fn fold(
        &mut self,
        snapshot: &Pile,
        seen_at: Option<SeenAt>,
        fault: &dyn FaultInjector,
    ) -> Result<(), OpsError> {
        // The lock spans the whole fold: the tree is built from the on-disk ledger's
        // overrides, so another process's accepts are folded in, never dropped.
        let _lock = LedgerLock::acquire(self.paths)?;
        self.merge_from_disk()?;
        // Order matters: later writes win. Overrides first, then the snapshot's rows.
        let mut writes: BTreeMap<Vec<u8>, TreeWrite> = BTreeMap::new();
        for (key, o) in &self.ledger.overrides {
            let path = key.as_bytes().to_vec();
            match &o.blob {
                Some(Some(oid)) => {
                    let mode = o
                        .mode
                        .or_else(|| self.tree.get(&path).map(|(m, _)| *m))
                        .unwrap_or(Mode::Regular);
                    writes.insert(
                        path.clone(),
                        TreeWrite::Set {
                            path,
                            mode,
                            oid: oid.clone(),
                        },
                    );
                }
                Some(None) => {
                    writes.insert(path.clone(), TreeWrite::Remove { path });
                }
                None => {}
            }
        }
        for row in &snapshot.rows {
            let path = row.path.clone();
            let w = match &row.current {
                Some(e) => TreeWrite::Set {
                    path: path.clone(),
                    mode: e.mode,
                    oid: e.oid.clone(),
                },
                None => TreeWrite::Remove { path: path.clone() },
            };
            writes.insert(path, w);
        }
        let writes: Vec<TreeWrite> = writes.into_values().collect();
        let new_tree = self
            .store
            .write_tree(self.ledger.seen_tree.as_ref(), &writes)?;
        fault.at(FaultPoint::AfterObjectWrite);

        self.ledger.seen_tree = Some(new_tree.clone());
        let now = self.clock.now_iso8601();
        self.ledger.overrides.retain(|_, o| {
            if o.blob.is_some() || o.mode.is_some() {
                o.blob = None;
                o.mode = None;
                o.updated_at = now.clone();
            }
            !o.is_empty()
        });
        if let Some(sa) = seen_at {
            self.ledger.seen_at = sa;
        }
        let tmp = ledger::write_tmp(self.paths, self.ledger)?;
        fault.at(FaultPoint::AfterLedgerTmpWrite);
        ledger::commit_tmp(self.paths, &tmp)?;
        drop(_lock);
        self.staged.clear();
        *self.tree = self.store.ls_tree(&new_tree)?;
        self.index.seed(Some(&new_tree))?;
        Ok(())
    }

    /// Set a flag on `path` (A8's Phase 2 slice). Never touches `blob`.
    pub fn flag(
        &mut self,
        path: &[u8],
        note: &str,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let key = match Self::key(path) {
            Ok(k) => k,
            Err(r) => {
                return Ok(Outcome {
                    refused: vec![r],
                    ..Default::default()
                });
            }
        };
        let now = self.clock.now_iso8601();
        let entry = self
            .ledger
            .overrides
            .entry(key.clone())
            .or_insert(Override {
                blob: None,
                mode: None,
                flag: None,
                updated_at: now.clone(),
            });
        entry.flag = Some(Flag {
            note: note.to_owned(),
            created_at: now.clone(),
        });
        entry.updated_at = now;
        let staged = entry.clone();
        self.staged.insert(key, Some(staged));
        let compacted = self.commit(fault)?;
        Ok(Outcome {
            refused: Vec::new(),
            compacted,
            written: true,
        })
    }

    /// Clear the flag on `path`; an override left with nothing is removed.
    pub fn unflag(&mut self, path: &[u8], fault: &dyn FaultInjector) -> Result<Outcome, OpsError> {
        let key = match Self::key(path) {
            Ok(k) => k,
            Err(r) => {
                return Ok(Outcome {
                    refused: vec![r],
                    ..Default::default()
                });
            }
        };
        let Some(entry) = self.ledger.overrides.get_mut(&key) else {
            return Ok(Outcome::default());
        };
        entry.flag = None;
        entry.updated_at = self.clock.now_iso8601();
        if entry.is_empty() {
            self.ledger.overrides.remove(&key);
        }
        self.staged
            .insert(key.clone(), self.ledger.overrides.get(&key).cloned());
        let compacted = self.commit(fault)?;
        Ok(Outcome {
            refused: Vec::new(),
            compacted,
            written: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::fixture_tests::Harness;
    use crate::scan::{Change, pile_lines};
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    fn rendered(h: &Harness, path: &[u8]) -> Rendered {
        Rendered::of(h.scan().pile.row(path).expect("row is pending"))
    }

    #[test]
    fn ops_commit_merges_with_a_ledger_written_by_another_engine() {
        let repo = FixtureRepo::new("ops-merge").unwrap();
        let state = TempDir::new("lc-ops");
        // Two engines over one root: each holds its own (soon stale) copy of the ledger.
        let mut a = Harness::new(&repo, &state);
        let mut b = Harness::new(&repo, &state);
        repo.write("f1", "one\n");
        repo.write("f2", "two\n");
        let r1 = rendered(&a, b"f1");
        let r2 = rendered(&b, b"f2");
        assert!(a.ops().accept_file(&r1, &NoFault).unwrap().ok());
        assert!(b.ops().accept_file(&r2, &NoFault).unwrap().ok());
        let disk = match ledger::load(&b.paths, &b.clock).unwrap() {
            LoadResult::Loaded { ledger, .. } => ledger,
            other => panic!("{other:?}"),
        };
        assert!(
            disk.overrides.contains_key("f1") && disk.overrides.contains_key("f2"),
            "b's write kept a's accept: {:?}",
            disk.overrides.keys().collect::<Vec<_>>()
        );
        assert_eq!(b.ledger, disk, "b adopted the merged ledger");
        assert!(b.scan().pile.is_empty());
        // A fold by the stale engine folds both accepts.
        a.ops().compact(&NoFault).unwrap();
        assert!(a.ledger.overrides.is_empty());
        assert!(a.scan().pile.is_empty(), "f2's accept survived a's fold");
        // A flag set by one side survives an accept by the other.
        assert!(b.ops().flag(b"f3", "look", &NoFault).unwrap().ok());
        repo.write("f1", "one more\n");
        let r1 = rendered(&a, b"f1");
        assert!(a.ops().accept_file(&r1, &NoFault).unwrap().ok());
        assert!(a.ledger.overrides["f3"].flag.is_some());
    }

    #[test]
    fn ops_accept_file_sets_override_and_cleanup_rule_removes_it() {
        let repo = FixtureRepo::new("ops-file").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let original = std::fs::read(repo.path().join("f1")).unwrap();
        repo.write("f1", "changed\n");
        let r = rendered(&h, b"f1");
        let out = h.ops().accept_file(&r, &NoFault).unwrap();
        assert!(out.ok() && out.written && !out.compacted);
        assert!(h.scan().pile.is_empty(), "accepted content is the baseline");
        let o = h.ledger.overrides.get("f1").unwrap();
        assert_eq!(o.blob.as_ref().unwrap().as_ref(), r.oid.as_ref());
        assert!(
            ledger::load(&h.paths, &h.clock).is_ok(),
            "ledger written to disk"
        );

        // Back to the tree's content: pending again (baseline is the override), and
        // accepting removes the override instead of storing the tree's blob (§6.2).
        repo.write("f1", &original);
        assert_eq!(pile_lines(&h.scan().pile), vec!["f1"]);
        let r = rendered(&h, b"f1");
        h.ops().accept_file(&r, &NoFault).unwrap();
        assert!(!h.ledger.overrides.contains_key("f1"));
        assert!(h.scan().pile.is_empty());
    }

    #[test]
    fn ops_accept_file_refuses_when_live_moved() {
        let repo = FixtureRepo::new("ops-moved").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "v1\n");
        let stale = rendered(&h, b"f1");
        repo.write("f1", "v2\n");
        let out = h.ops().accept_file(&stale, &NoFault).unwrap();
        assert!(matches!(
            out.refused[0],
            Refused::Moved { live: Some(_), .. }
        ));
        assert!(!out.written);
        assert!(
            h.ledger.overrides.is_empty(),
            "no ledger write on refusal (A6)"
        );
        assert!(!h.paths.ledger.exists());
        assert_eq!(pile_lines(&h.scan().pile), vec!["f1"]);
        // A deleted-since-render file is Moved { live: None }; an unhashable one is refused.
        repo.remove("f1");
        let out = h.ops().accept_file(&stale, &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::Moved { live: None, .. }));
        std::fs::create_dir(repo.path().join("f1")).unwrap();
        let out = h.ops().accept_file(&stale, &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::Unhashable { .. }));
    }

    #[test]
    fn ops_accept_deletion_and_group() {
        let repo = FixtureRepo::new("ops-del").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let r2 = Rendered {
            path: b"f2".to_vec(),
            oid: None,
            mode: None,
            baseline: h.tree_entries.get(&b"f2"[..]).map(|(_, o)| o.clone()),
            baseline_mode: h.tree_entries.get(&b"f2"[..]).map(|(m, _)| *m),
        };
        let out = h.ops().accept_deletion(&r2, &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::StillPresent { .. }), "A7");
        repo.remove("f2");
        repo.write("f1", "x\n");
        repo.write("f3", "y\n");
        let pile = h.scan().pile;
        assert_eq!(pile_lines(&pile), vec!["f1", "f2", "f3"]);
        let rows: Vec<Rendered> = pile.rows.iter().map(Rendered::of).collect();
        repo.write("f3", "moved\n");
        let out = h.ops().accept_group(&rows, &NoFault).unwrap();
        assert_eq!(out.refused.len(), 1);
        assert!(matches!(&out.refused[0], Refused::Moved { path, .. } if path == b"f3"));
        assert!(out.written, "one write for the rows that passed");
        assert_eq!(h.ledger.overrides.get("f2").unwrap().blob, Some(None));
        assert_eq!(pile_lines(&h.scan().pile), vec!["f3"]);
        assert_eq!(h.scan().pile.row(b"f3").unwrap().change, Change::Modified);
    }

    #[test]
    fn ops_accept_hunk_splices_one_and_cas_on_baseline() {
        let repo = FixtureRepo::new("ops-hunk").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let base: String = (0..30).map(|i| format!("line {i}\n")).collect();
        repo.write("f1", &base);
        h.mark_seen();
        let edited = base
            .replace("line 2\n", "LINE 2\n")
            .replace("line 27\n", "LINE 27\n");
        repo.write("f1", &edited);
        let row = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(row.hunks.len(), 2);
        let r = Rendered::of(&row);
        let out = h.ops().accept_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok());
        let after = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(after.hunks.len(), 1, "only the second hunk remains");
        assert!(after.hunks[0].lines.iter().any(|(_, l)| l == b"LINE 27\n"));
        // The stale render (old baseline) is refused; the fresh one accepts.
        let out = h.ops().accept_hunk(&r, &row.hunks, 1, &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::BaselineMoved { .. }));
        let r2 = Rendered::of(&after);
        let out = h.ops().accept_hunk(&r2, &after.hunks, 0, &NoFault).unwrap();
        assert!(out.ok());
        assert!(h.scan().pile.is_empty());
        let out = h.ops().accept_hunk(&r2, &after.hunks, 5, &NoFault).unwrap();
        assert!(matches!(
            out.refused[0],
            Refused::NoSuchHunk { index: 5, .. }
        ));
    }

    #[test]
    fn ops_accept_mode_hunk_moves_only_the_mode() {
        let repo = FixtureRepo::new("ops-mode").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        if !h.store.filemode() {
            return;
        }
        repo.write("f1", "changed\n");
        repo.chmod_x("f1", true);
        let row = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(row.hunks.len(), 2);
        assert!(row.hunks[1].is_mode_change());
        let r = Rendered::of(&row);
        h.ops().accept_hunk(&r, &row.hunks, 1, &NoFault).unwrap();
        // The same rendered mode hunk again: the baseline oid is unchanged, the mode
        // moved — refused, never re-applied (R12).
        let stale = h.ops().accept_hunk(&r, &row.hunks, 1, &NoFault).unwrap();
        assert!(
            matches!(stale.refused.first(), Some(Refused::BaselineMoved { .. })),
            "{stale:?}"
        );
        let after = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(after.change, Change::Modified);
        assert_eq!(after.hunks.len(), 1, "content hunk remains, mode accepted");
        assert_eq!(after.baseline.as_ref().unwrap().mode, Mode::Executable);
        let r2 = Rendered::of(&after);
        h.ops().accept_hunk(&r2, &after.hunks, 0, &NoFault).unwrap();
        assert!(h.scan().pile.is_empty());
    }

    #[test]
    fn ops_accept_all_folds_snapshot_overrides_and_moves_seen_at() {
        let repo = FixtureRepo::new("ops-all").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let before_tree = h.ledger.seen_tree.clone().unwrap();
        // An accepted override on f3 that is no longer pending must survive the fold.
        repo.write("f3", "accepted\n");
        let r3 = rendered(&h, b"f3");
        h.ops().accept_file(&r3, &NoFault).unwrap();
        h.ops().flag(b"f3", "keep me", &NoFault).unwrap();
        repo.write("f1", "one\n");
        repo.remove("f2");
        repo.write("new.txt", "n\n");
        let snapshot = h.scan().pile;
        assert_eq!(pile_lines(&snapshot), vec!["f1", "f2", "new.txt"]);
        // A5: disk moves after the snapshot; the fold uses the rendered oid.
        repo.write("f1", "one more\n");
        h.ops().accept_all(&snapshot, &NoFault).unwrap();
        let after = h.scan().pile;
        assert_eq!(
            pile_lines(&after),
            vec!["f1"],
            "only the post-snapshot delta"
        );
        let f1 = after.row(b"f1").unwrap();
        assert_eq!(
            f1.hunks[0]
                .lines
                .iter()
                .filter(|(t, _)| *t == hunks::Tag::Delete)
                .count(),
            1
        );
        assert_ne!(h.ledger.seen_tree.as_ref().unwrap(), &before_tree);
        assert_eq!(h.ledger.blob_override_count(), 0);
        let o = h.ledger.overrides.get("f3").unwrap();
        assert_eq!(o.flag.as_ref().unwrap().note, "keep me");
        assert!(o.blob.is_none() && o.mode.is_none());
        let head = Oid::parse(repo.head().unwrap().trim()).unwrap();
        assert_eq!(h.ledger.seen_at.head_commit, Some(head));
        assert_eq!(h.ledger.seen_at.branch.as_deref(), Some("main"));
        assert_eq!(h.ledger.seen_at.at, h.clock.now_iso8601());
        assert!(h.tree_entries.contains_key(&b"new.txt"[..]));
        assert!(!h.tree_entries.contains_key(&b"f2"[..]));
        assert_eq!(h.tree_entries.get(&b"f3"[..]).unwrap().1, r3.oid.unwrap());
        assert_eq!(h.index.recorded_tree(), Some(h.ledger.seen_tree.clone()));
    }

    #[test]
    fn ops_compaction_triggers_over_threshold_and_keeps_the_pile() {
        let repo = FixtureRepo::new("ops-compact").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        h.compaction_threshold = 2;
        let seen_at = h.ledger.seen_at.clone();
        let mut old_seen_at = seen_at.clone();
        old_seen_at.head_commit = Some(Oid::parse(repo.head().unwrap().trim()).unwrap());
        h.ledger.seen_at = old_seen_at.clone();
        repo.write("still.txt", "pending\n");
        for (i, p) in ["f1", "f2", "f3"].iter().enumerate() {
            repo.write(p, format!("edit {i}\n"));
            let r = rendered(&h, p.as_bytes());
            let before = h.scan().pile;
            let out = h.ops().accept_file(&r, &NoFault).unwrap();
            let expect_compact = i == 2;
            assert_eq!(
                out.compacted, expect_compact,
                "{p}: compaction at > threshold"
            );
            let mut expected = before;
            expected.rows.retain(|row| row.path != p.as_bytes());
            let after = h.scan().pile;
            assert_eq!(pile_lines(&after), pile_lines(&expected));
        }
        assert_eq!(h.ledger.blob_override_count(), 0);
        assert_eq!(
            h.ledger.seen_at, old_seen_at,
            "compaction never moves seen_at"
        );
        assert_eq!(pile_lines(&h.scan().pile), vec!["still.txt"]);
        let _ = seen_at;
    }

    #[test]
    fn ops_flag_and_unflag_never_touch_blob() {
        let repo = FixtureRepo::new("ops-flag").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        h.ops().flag(b"f1", "note", &NoFault).unwrap();
        let o = h.ledger.overrides.get("f1").unwrap();
        assert!(o.blob.is_none());
        assert!(h.scan().pile.is_empty(), "a flag alone is not pending");
        repo.write("f1", "x\n");
        assert_eq!(
            h.scan()
                .pile
                .row(b"f1")
                .unwrap()
                .flag
                .as_ref()
                .unwrap()
                .note,
            "note"
        );
        h.ops().unflag(b"f1", &NoFault).unwrap();
        assert!(!h.ledger.overrides.contains_key("f1"));
        let out = h.ops().unflag(b"nope", &NoFault).unwrap();
        assert!(!out.written);
        let out = h.ops().flag(b"bad\xff", "n", &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::NonUtf8Path { .. }));
    }
}
