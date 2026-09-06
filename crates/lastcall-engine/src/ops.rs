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
use std::time::Duration;

use crate::git::{GitError, Mode, Oid, RepoGit};
use crate::headstate::current_head;
use crate::hunks::{self, Hunk};
use crate::index::{IndexError, PrivateIndex};
use crate::ledger::{
    self, Baseline, BaselineResolver, Clock, Flag, FlagHunk, FlagSummary, Ledger, LedgerError,
    LedgerLock, LoadResult, Override, SeenAt, TreeEntries,
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
    /// A restore's temp file is written and synced; the second live CAS and the rename
    /// have not happened. Unlike the other two this point is also *recoverable*: a test
    /// that wants to see the cleanup rather than a dead process answers
    /// [`FaultInjector::fails_at`] instead of killing at [`FaultInjector::at`].
    AfterTempWrite,
}

/// The E1 test seam. Production uses [`NoFault`].
pub trait FaultInjector {
    fn at(&self, point: FaultPoint);

    /// Whether the op should abort at `point` and unwind normally.
    ///
    /// [`FaultInjector::at`] is the hard seam — the testkit's implementation SIGKILLs, and
    /// a killed process cannot then assert that the temp file was removed. Deliverable 1
    /// has to prove exactly that, so the restore path also asks this soft question, which
    /// defaults to "no fault" and leaves every existing injector unchanged.
    fn fails_at(&self, _point: FaultPoint) -> bool {
        false
    }
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
    /// The baseline blob does not come back through the store's eol conversion as the bytes
    /// on disk, so writing it would rewrite the user's line endings behind their back
    /// (verifier F2's guard). Its own variant rather than an `Unhashable` reason because
    /// the file hashed perfectly well — hashing it is how the guard knows (verifier (b) F7).
    NotRoundTrippable { path: Vec<u8> },
    /// `accept_deletion` on a path that still exists (A7), or a deletion **restore** whose
    /// name is taken. `collides_with` names the directory entry that is in the way when it
    /// is not the path's own name byte-for-byte — on a case-folding root `f1` is refused
    /// because `F1` exists, and saying so is the difference between a usable message and a
    /// baffling one (F6, D4).
    StillPresent {
        path: Vec<u8>,
        collides_with: Option<Vec<u8>>,
    },
    /// Non-UTF-8 paths cannot be keyed in the ledger in v1.
    NonUtf8Path { path: Vec<u8> },
    /// `accept_hunk` with an index the row does not have.
    NoSuchHunk { path: Vec<u8>, index: usize },
    /// The path is in a merge conflict (`ls-files -u`): restoring it would write over one
    /// side of a merge git is still holding open (gate item 4; C4).
    Conflicted { path: Vec<u8> },
    /// The row cannot be edited at all: it is a deletion, a symlink, or (deliverable 8)
    /// content the inline editor will not hold — binary, or over the collapse cap.
    ///
    /// `why` is a short noun phrase the UI can also render on its own (`use shift-i:
    /// <why>`), so it never repeats the path.
    NotEditable { path: Vec<u8>, why: String },
    /// The hunk list handed to `restore_hunk` does not reassemble the rendered content, so
    /// "everything but hunk k" would silently drop whatever is missing from it (verifier
    /// F3). The one refusal that is about the *caller's* view rather than the file.
    Incomplete { path: Vec<u8> },
}

impl Refused {
    /// The refusal in words, with `verb` as the past participle of the operation that did
    /// not happen (`"accepted"`, `"restored"`).
    ///
    /// One vocabulary, two operations: the reasons are identical — the file moved, the
    /// baseline moved, the path cannot be hashed — and only the sentence's verb differs, so
    /// the verb is a parameter rather than a second enum. [`std::fmt::Display`] passes
    /// `"accepted"`, which is what every pre-Phase-7 caller printed.
    pub fn message(&self, verb: &str) -> String {
        let lossy = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
        match self {
            Refused::Moved { path, .. } => {
                format!("{}: changed since rendered; not {verb}", lossy(path))
            }
            Refused::BaselineMoved { path } => {
                format!("{}: baseline moved since rendered; not {verb}", lossy(path))
            }
            Refused::Unhashable { path, reason } => {
                format!("{}: cannot hash ({reason}); not {verb}", lossy(path))
            }
            Refused::NotRoundTrippable { path } => {
                format!(
                    "{}: eol conversion is not round-trippable; not {verb}",
                    lossy(path)
                )
            }
            Refused::StillPresent {
                path,
                collides_with,
            } => match collides_with {
                Some(other) if other != path => format!(
                    "{}: {} is in the way; deletion not {verb}",
                    lossy(path),
                    lossy(other)
                ),
                _ => format!("{}: still present; deletion not {verb}", lossy(path)),
            },
            Refused::NonUtf8Path { path } => {
                format!("{}: non-UTF-8 path; accept unsupported in v1", lossy(path))
            }
            // The one refusal whose verb is fixed: editing is the only operation that can
            // raise it, so `Display` (which passes `"accepted"`) still reads correctly.
            Refused::NotEditable { path, why } => format!("{}: {why}; not saved", lossy(path)),
            Refused::NoSuchHunk { path, index } => format!("{}: no hunk {index}", lossy(path)),
            Refused::Conflicted { path } => {
                format!("{}: unresolved merge conflict; not {verb}", lossy(path))
            }
            Refused::Incomplete { path } => {
                format!(
                    "{}: only part of the diff is loaded; not {verb}",
                    lossy(path)
                )
            }
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message("accepted"))
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
    /// A restore's write path failed for a reason that is not a refusal: the temp file
    /// could not be created, written, or renamed. Distinct from every variant above
    /// because it is the only one that names a path in the user's working tree.
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

impl OpsError {
    /// `Err(OpsError::Io { .. })` in the shape the restore paths need at their match arms.
    fn io<T>(path: std::path::PathBuf, source: std::io::Error) -> Result<T, OpsError> {
        Err(OpsError::Io { path, source })
    }
}

/// The basename glob a restore's temp file matches, re-exported here so `scan` and `ops`
/// share one constant (F8). The rule itself is [`crate::restore::is_restore_temp`].
pub use crate::restore::RESTORE_TEMP_GLOB;

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
/// The shipping lock budget, as `Engine::ops` sets it: 40 × 50 ms = 2 s.
pub const DEFAULT_LOCK: (u32, Duration) = (ledger::LOCK_RETRIES, ledger::LOCK_BACKOFF);

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
    /// Whether the root's filesystem folds case (`scan::probe_case_insensitive`, the same
    /// value the scan is given). Only the deletion restore reads it, and only to widen
    /// "the name is free" from byte-exact to case-folded (F6).
    pub case_insensitive: bool,
    /// Override changes made in memory since the last commit, keyed by path (`None` =
    /// removed). Start empty: `commit` replays them onto the on-disk ledger under the lock
    /// and clears them, so two engines over one root never lose each other's writes.
    pub staged: BTreeMap<String, Option<Override>>,
    /// How long `commit` waits for the root's ledger lock: [`ledger::LOCK_RETRIES`] ×
    /// [`ledger::LOCK_BACKOFF`] = 2 s in the shipping engine (`Engine::ops` sets it).
    /// A test that wants the `LockBusy` path shortens it rather than sleeping for two
    /// seconds; nothing else has a reason to touch it.
    pub lock: (u32, Duration),
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
                flags: Vec::new(),
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
            let _lock = LedgerLock::acquire_with(self.paths, self.lock.0, self.lock.1)?;
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
                collides_with: None,
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

    /// Accept one hunk (A3): the **baseline** CAS only; the baseline with that one hunk
    /// applied becomes the override blob.
    ///
    /// There is deliberately no live CAS here (Amendment A3, ruling of 2026-09-03). What
    /// the user accepted is a hunk against a baseline they were shown, and that baseline is
    /// still what it was — so the accept is meaningful whatever the working tree has done
    /// since. Refusing it because the file moved under them punished exactly the case the
    /// feature is for: an agent still writing to the file while the human reviews it. A
    /// stale accept cannot smuggle anything in, because the override is
    /// `apply_hunks(baseline, rendered_hunks, [k])` — baseline plus the one hunk they saw,
    /// never a byte of the live file.
    ///
    /// The consequence is that nothing in here may read the live file: the mode comes from
    /// `rendered.mode`, which is the mode the row was rendered with. A re-stat would be a
    /// live read by another name, and would reintroduce the race in the one place A3 says
    /// there must not be one.
    ///
    /// The baseline CAS stays (`BaselineMoved`): if the *baseline* moved, the hunks were
    /// computed against something that is no longer the comparison point, so applying one
    /// would write a blob the user never saw. `NoSuchHunk` and the deletion route stay.
    /// File, root and accept-all keep their live CAS (A5/A6) — those accept the live
    /// content, so the live content is what they must check.
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
        // A deletion renders as one hunk (`@@ -n +0,0 @@`) with its own accept control;
        // its live CAS is "still absent" (A7), not "unchanged content", so it takes the
        // deletion path — the sponsor hit the refusal live (2026-09-02).
        if rendered.oid.is_none() {
            return self.accept_file(rendered, fault);
        }
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
            // D1: the synthetic mode hunk moves only the mode - to the mode the row was
            // rendered with, which is the one the user saw on the hunk's `+` side.
            (base_oid, rendered.mode)
        } else {
            let base_bytes = match &base_oid {
                Some(o) => self.store.cat_blob(o)?,
                None => Vec::new(),
            };
            let spliced = hunks::apply_hunks(&base_bytes, hunks, &[hunk_index]);
            let oid = self.store.hash_bytes(&spliced)?;
            fault.at(FaultPoint::AfterObjectWrite);
            // A content hunk leaves the mode where the baseline had it; without a mode
            // hunk the modes agreed when the row was rendered, so the rendered mode is
            // that mode.
            let mode = if hunks.iter().any(Hunk::is_mode_change) {
                base_mode.or(rendered.mode)
            } else {
                rendered.mode
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

    // -----------------------------------------------------------------------------------
    // Restore — the only operation in lastcall that writes the user's working tree
    // -----------------------------------------------------------------------------------
    //
    // Read these three together with [`Ops::accept_hunk`] above, and do not harmonise
    // them. Accept-hunk deliberately has **no** live CAS (Amendment A3): it writes a blob
    // into the ledger out of the baseline and the hunk the user saw, so a working tree
    // that moved underneath cannot smuggle anything in. Restore is the exact opposite case
    // — it writes the working tree — so the live CAS is the entire guard, and it runs
    // twice: once at entry, and once more immediately before the `rename` (§6.3). The
    // remaining window between the second hash and the rename is the §11 residual; the
    // rescan every restore ends with is its mitigation.
    //
    // [`Ops::save_file`] below is the same shape for the same reason, with one addition
    // the restore does not have: it writes the ledger too, *after* the file.

    /// Everything both restore routes check before anything is computed, in the order the
    /// checks have to happen: a UTF-8 path, no unresolved conflict, no symlink in the
    /// parent chain (F9 — this must precede the first CAS hash, because `hash_path`'s own
    /// `lstat` follows a swapped parent directory and would happily agree), and no
    /// `filter` attribute the store cannot promise to reproduce (F2).
    fn restore_preflight(&self, path: &[u8]) -> Result<(), Refused> {
        Self::key(path)?;
        if self.is_conflicted(path) {
            return Err(Refused::Conflicted {
                path: path.to_vec(),
            });
        }
        crate::restore::check_parent_chain(self.store.root(), path).map_err(|e| match e {
            crate::restore::WriteError::Refuse(reason) => Refused::Unhashable {
                path: path.to_vec(),
                reason,
            },
            crate::restore::WriteError::Moved => Refused::Moved {
                path: path.to_vec(),
                live: None,
            },
            crate::restore::WriteError::Io { source, .. } => Refused::Unhashable {
                path: path.to_vec(),
                reason: source.to_string(),
            },
        })?;
        if let Some(name) = crate::restore::filter_attr(self.store, path) {
            return Err(Refused::Unhashable {
                path: path.to_vec(),
                reason: format!("filter={name}"),
            });
        }
        Ok(())
    }

    /// Whether the user's index holds `path` at a conflict stage (C4).
    ///
    /// Read live rather than taken from `Row.conflicted`: a merge can be resolved or
    /// started between the render and the keystroke, and for a write the live answer is
    /// the safe direction in both cases. A root with no user repo (a draft dir) has no
    /// conflicts, and an unreadable index is not a licence to write — but it is also not a
    /// reason to refuse every restore, so it fails open exactly as the scan's own notice
    /// path does.
    fn is_conflicted(&self, path: &[u8]) -> bool {
        let Some(rg) = self.repo else { return false };
        let Ok(out) = rg.run(&["ls-files", "-u", "-z"]) else {
            return false;
        };
        let Ok(entries) = crate::git::parse_ls_files_stage_z(&out) else {
            return false;
        };
        entries.iter().any(|e| e.path == path)
    }

    /// Whether a `Baseline::Empty` means "this file did not exist at the baseline" (so a
    /// file restore removes it) or "this root has no baseline yet" (so it must not).
    ///
    /// `BaselineResolver` gives `Empty` for both — a path outside the seen tree and a root
    /// with no seen tree at all (`ledger.rs:589`) — and the kickoff asks for opposite
    /// behaviour in each: "`Baseline::Empty` → a zero-byte file, never a removal (F17)"
    /// and "when the baseline is absent (an added file), the file is removed". The seen
    /// tree is what separates them, and it separates them exactly where F17 points: F17's
    /// named case is a draft root at `draft_initial = pending`, which *is*
    /// `seen_tree: None`. A git root's seen tree seeds from HEAD at open, so only a
    /// genuinely untracked file is `Empty` there, and removing it is what "restore this
    /// added file" means. Before anything has been seen, nothing has a baseline to go back
    /// to, and destroying the user's first-sight draft is the one outcome F17 forbids.
    fn empty_baseline_means_absent(&self) -> bool {
        self.ledger.seen_tree.is_some()
    }

    /// The baseline for `path`, CAS'd against the one the row was rendered with.
    fn restore_baseline(&mut self, rendered: &Rendered) -> Result<Baseline, Refused> {
        let baseline = {
            let mut resolver = BaselineResolver::new(
                self.ledger,
                self.tree,
                self.store,
                std::iter::once(rendered.path.as_slice()),
            );
            resolver.baseline(&rendered.path)
        };
        let (oid, mode) = match &baseline {
            Baseline::Present { oid, mode } => (Some(oid.clone()), Some(*mode)),
            Baseline::Absent | Baseline::Empty => (None, None),
        };
        if oid != rendered.baseline || mode != rendered.baseline_mode {
            return Err(Refused::BaselineMoved {
                path: rendered.path.clone(),
            });
        }
        Ok(baseline)
    }

    /// Refuse a restore whose worktree bytes are **not reproducible** from the blob they
    /// hashed to (verifier F2).
    ///
    /// Everything a restore writes goes out through `cat-file --filters`, which is what
    /// `git checkout` would write. Under `text=auto eol=crlf` that reproduces the user's
    /// CRLF file exactly. Under **bare** `* text=auto` on a native-LF platform it does not:
    /// git cleans CRLF to LF on the way in and writes LF on the way out, so a file the user
    /// keeps with CRLF endings comes back LF. Restoring one hunk of such a file rewrote
    /// every line ending in it, and — because the canonical blob then matched — the pile
    /// came back clean, so lastcall could not even show what it had done (invariant 2,
    /// "over-show, never hide").
    ///
    /// The guard is a round trip: materialise the **current** oid (the one the entry CAS
    /// just verified) through the same filters and compare with the live bytes. Equal means
    /// the worktree representation is reproducible and a restore can write it back
    /// faithfully; different means it is not, and lastcall refuses rather than normalising
    /// the user's line endings behind their back. One extra `cat-file` per restore, and it
    /// refuses only the genuinely lossy case.
    ///
    /// Exempt, all for want of content to compare: draft roots (raw-byte model, no
    /// filters), symlinks (the blob is the link text), deletion restores (`oid == None`,
    /// nothing on disk) and mode-only restores (which return before this).
    fn round_trip_guard(&self, rendered: &Rendered) -> Result<(), Refused> {
        if self.store.kind() != RootKind::Git || rendered.mode == Some(Mode::Symlink) {
            return Ok(());
        }
        let Some(oid) = rendered.oid.as_ref() else {
            return Ok(());
        };
        let full = self.store.root().join(OsStr::from_bytes(&rendered.path));
        // Unreadable is the entry CAS's business, not this guard's.
        let Ok(live) = std::fs::read(&full) else {
            return Ok(());
        };
        let refuse = |reason: &str| {
            Err(Refused::Unhashable {
                path: rendered.path.clone(),
                reason: reason.to_string(),
            })
        };
        match crate::restore::materialise(self.store, oid, &rendered.path) {
            Ok(back) if back == live => Ok(()),
            Ok(_) => Err(Refused::NotRoundTrippable {
                path: rendered.path.clone(),
            }),
            // A conversion we cannot run is never a licence to write.
            Err(_) => refuse("cannot reproduce the worktree bytes"),
        }
    }

    /// The bytes `content` should become on disk at `path`: through git's smudge/eol
    /// conversion on a git root, unchanged on a draft root (F2).
    fn restore_bytes(&self, path: &[u8], content: &[u8]) -> Result<Vec<u8>, OpsError> {
        if self.store.kind() != RootKind::Git {
            return Ok(content.to_vec());
        }
        // The blob has to exist before `cat-file` can convert it. Every blob a restore
        // writes back is one the user already had, so this normally finds the object
        // already there; a spliced hunk result is new and genuinely needs writing.
        let oid = self.store.hash_bytes(content)?;
        match crate::restore::materialise(self.store, &oid, path) {
            Ok(bytes) => Ok(bytes),
            // A conversion that fails is not a refusal we can name usefully — but it is
            // also never a reason to write the unconverted bytes over the user's file.
            Err(crate::restore::WriteError::Refuse(reason)) => Err(OpsError::Io {
                path: self.store.root().join(OsStr::from_bytes(path)),
                source: std::io::Error::other(reason),
            }),
            Err(crate::restore::WriteError::Moved) => Err(OpsError::Io {
                path: self.store.root().join(OsStr::from_bytes(path)),
                source: std::io::Error::other("moved"),
            }),
            Err(crate::restore::WriteError::Io { path, source }) => OpsError::io(path, source),
        }
    }

    /// The second live CAS plus the soft fault seam, as a closure the write path calls
    /// with the temp file on disk and the rename not yet done.
    fn second_cas<'f>(
        rendered: &Rendered,
        store: &'f Store,
        fault: &'f dyn FaultInjector,
    ) -> impl FnMut() -> Result<(), crate::restore::WriteError> + 'f {
        let rendered = rendered.clone();
        move || {
            fault.at(FaultPoint::AfterTempWrite);
            if fault.fails_at(FaultPoint::AfterTempWrite) {
                return Err(crate::restore::WriteError::Refuse("fault injected".into()));
            }
            // The parent chain was walked in the preflight, before the first CAS, and the
            // temp file has been sitting on disk since then. A directory swapped for a
            // symlink in that window is invisible to the hash compare below — `hash_path`
            // resolves through the new parent, and a decoy holding identical bytes makes
            // the CAS agree (verifier F7). So the walk runs again, here, with the rename
            // one statement away.
            if crate::restore::check_parent_chain(store.root(), &rendered.path).is_err() {
                return Err(crate::restore::WriteError::Moved);
            }
            match store.hash_path(&rendered.path) {
                Current::Present { oid, mode } => {
                    if rendered.oid.as_ref() == Some(&oid) && rendered.mode == Some(mode) {
                        Ok(())
                    } else {
                        Err(crate::restore::WriteError::Refuse(
                            "changed since rendered".into(),
                        ))
                    }
                }
                _ => Err(crate::restore::WriteError::Refuse(
                    "changed since rendered".into(),
                )),
            }
        }
    }

    /// Put `content` (or a symlink to it, when `mode` is `Symlink`) at `path`, or remove
    /// `path` when `content` is `None`. `before` is the second CAS.
    fn restore_write(
        &self,
        path: &[u8],
        content: Option<&[u8]>,
        mode: Option<Mode>,
        before: &mut dyn FnMut() -> Result<(), crate::restore::WriteError>,
    ) -> Result<Outcome, OpsError> {
        use crate::restore::{self as rst, WriteError};
        let result = match (content, mode) {
            (None, _) => rst::remove(self.store, path, before),
            (Some(target), Some(Mode::Symlink)) => {
                rst::write_symlink(self.store, path, target, before)
            }
            (Some(bytes), _) => rst::write_bytes(self.store, path, bytes, mode, before),
        };
        match result {
            Ok(()) => Ok(Outcome {
                refused: Vec::new(),
                compacted: false,
                written: false,
            }),
            // The second CAS is the only refusal this deep, and it is `Moved` — the same
            // answer the entry CAS gives, so the TUI has one case to render.
            Err(WriteError::Refuse(_) | WriteError::Moved) => Ok(Outcome {
                refused: vec![Refused::Moved {
                    path: path.to_vec(),
                    live: None,
                }],
                ..Default::default()
            }),
            Err(WriteError::Io { path, source }) => OpsError::io(path, source),
        }
    }

    /// Restore one hunk: the working file becomes `baseline ⊕ every content hunk but k`.
    ///
    /// No reverse-apply and no new diff code — a row's live content *is*
    /// `baseline ⊕ all hunks` while the live CAS holds, so dropping `k` from the selection
    /// is exactly "undo hunk k" (kickoff §3.3). The synthetic mode hunk is never in that
    /// selection: `apply_hunks` would splice its literal `mode 100755` bytes in at offset 0
    /// (F1). Restoring the mode hunk itself moves the mode alone, writing no bytes.
    pub fn restore_hunk(
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
        // A deletion row renders as one hunk with its own control (as accept does).
        if rendered.oid.is_none() {
            return self.restore_deletion(rendered, fault);
        }
        let Some(hunk) = hunks.get(hunk_index) else {
            return refuse(Refused::NoSuchHunk {
                path: rendered.path.clone(),
                index: hunk_index,
            });
        };
        let is_mode = hunk.is_mode_change();
        if let Err(r) = self.restore_preflight(&rendered.path) {
            return refuse(r);
        }
        if let Err(r) = self.cas_live(rendered) {
            return refuse(r);
        }
        let baseline = match self.restore_baseline(rendered) {
            Ok(b) => b,
            Err(r) => return refuse(r),
        };
        if is_mode {
            // D1: the mode hunk goes back to the baseline mode and nothing else is touched
            // — no temp file, no rename, and on a root that ignores the executable bit,
            // nothing at all.
            let mode = match &baseline {
                Baseline::Present { mode, .. } => Some(*mode),
                Baseline::Absent | Baseline::Empty => rendered.baseline_mode,
            };
            return match crate::restore::set_mode(self.store, &rendered.path, mode) {
                Ok(()) => Ok(Outcome::default()),
                Err(crate::restore::WriteError::Io { path, source }) => OpsError::io(path, source),
                // The leaf is a symlink now, so it is not the row that was rendered (F6).
                Err(crate::restore::WriteError::Moved) => refuse(Refused::Moved {
                    path: rendered.path.clone(),
                    live: None,
                }),
                Err(crate::restore::WriteError::Refuse(reason)) => refuse(Refused::Unhashable {
                    path: rendered.path.clone(),
                    reason,
                }),
            };
        }
        // Before any temp file exists: the user's bytes must be reproducible from the blob
        // we are about to splice into, or nothing is written at all (F2).
        if let Err(r) = self.round_trip_guard(rendered) {
            return refuse(r);
        }
        let base_bytes = match &baseline {
            Baseline::Present { oid, .. } => self.store.cat_blob(oid)?,
            Baseline::Absent | Baseline::Empty => Vec::new(),
        };
        // The hunk CAS has to cover the *hunks*, not only the file (verifier F3).
        //
        // "Restore hunk k" is `baseline ⊕ every content hunk but k`, and that identity holds
        // only while the list the caller passed is the complete one. `hunks::expand`
        // truncates at a line cap, so an expanded collapsed row can hand over a partial list
        // — and both CASes still pass, because the *file* has not moved. The write would
        // then be `baseline ⊕ a fragment`, silently discarding every edit the cap dropped.
        // The store's blobs are canonical, so reassembling the whole list and hashing it is
        // an exact test: it equals the current oid precisely when nothing is missing.
        let all: Vec<usize> = hunks
            .iter()
            .filter(|h| !h.is_mode_change())
            .map(|h| h.index)
            .collect();
        let whole = hunks::apply_hunks(&base_bytes, hunks, &all);
        // `cas_live` above proved the row has a current oid and that it is this one.
        let current = rendered.oid.as_ref().expect("cas_live passed");
        if self.store.hash_bytes(&whole)? != *current {
            return refuse(Refused::Incomplete {
                path: rendered.path.clone(),
            });
        }
        let keep: Vec<usize> = all.iter().copied().filter(|i| *i != hunk_index).collect();
        // An added file's only content hunk *is* the file (verifier F4; the F16 shape for
        // additions). "Everything but hunk 0" is nothing at all, and writing a zero-byte
        // file where the user's added file was left a truncated file and a still-pending
        // row — recoverable only by pressing restore a second time. Take the removal path
        // `restore_file` takes for exactly this row.
        let baseline_absent = matches!(&baseline, Baseline::Absent)
            || matches!(&baseline, Baseline::Empty if self.empty_baseline_means_absent());
        if baseline_absent && keep.is_empty() {
            let mut before = Self::second_cas(rendered, self.store, fault);
            return self.restore_write(&rendered.path, None, None, &mut before);
        }
        let content = hunks::apply_hunks(&base_bytes, hunks, &keep);
        let bytes = self.restore_bytes(&rendered.path, &content)?;
        // A content hunk leaves the mode where the live file has it: only the mode hunk
        // moves the mode, and it may still be pending.
        let mut before = Self::second_cas(rendered, self.store, fault);
        self.restore_write(&rendered.path, Some(&bytes), rendered.mode, &mut before)
    }

    /// Restore the whole file to its baseline.
    ///
    /// `Baseline::Empty` writes a zero-byte file and never a removal (F17): "empty" is the
    /// baseline for a path lastcall has seen with no content, and removing it would delete
    /// a file the user never asked to lose. `Baseline::Absent` — the file did not exist at
    /// the baseline — is the one case that removes.
    pub fn restore_file(
        &mut self,
        rendered: &Rendered,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let refuse = |r: Refused| {
            Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            })
        };
        if rendered.oid.is_none() {
            return self.restore_deletion(rendered, fault);
        }
        if let Err(r) = self.restore_preflight(&rendered.path) {
            return refuse(r);
        }
        if let Err(r) = self.cas_live(rendered) {
            return refuse(r);
        }
        let baseline = match self.restore_baseline(rendered) {
            Ok(b) => b,
            Err(r) => return refuse(r),
        };
        // A removal has no representation to preserve; every other arm writes bytes over
        // the user's file and must prove the round trip first (F2).
        let removes = matches!(&baseline, Baseline::Absent)
            || matches!(&baseline, Baseline::Empty if self.empty_baseline_means_absent());
        if !removes && let Err(r) = self.round_trip_guard(rendered) {
            return refuse(r);
        }
        let mut before = Self::second_cas(rendered, self.store, fault);
        match &baseline {
            Baseline::Absent => self.restore_write(&rendered.path, None, None, &mut before),
            Baseline::Empty if self.empty_baseline_means_absent() => {
                self.restore_write(&rendered.path, None, None, &mut before)
            }
            Baseline::Empty => self.restore_write(
                &rendered.path,
                Some(&[]),
                rendered.baseline_mode,
                &mut before,
            ),
            Baseline::Present { oid, mode } => {
                let content = self.store.cat_blob(oid)?;
                let bytes = if *mode == Mode::Symlink {
                    // A symlink's blob *is* the link text; there is nothing to smudge, and
                    // running it through `cat-file --filters` would be asking git to
                    // convert a path name.
                    content
                } else {
                    self.restore_bytes(&rendered.path, &content)?
                };
                self.restore_write(&rendered.path, Some(&bytes), Some(*mode), &mut before)
            }
        }
    }

    /// Restore a deleted file: put the baseline back where it was.
    ///
    /// The CAS here is "still absent", as `accept_deletion`'s is — but absent has to be
    /// decided from the parent's `read_dir`, byte-for-byte and (on a case-folding root)
    /// under case folding too. `exists`/`stat` would ask the filesystem, which folds; a
    /// byte-only rule would call `f1` free while `F1` sits there, and the `rename` would
    /// then fold onto `F1` and destroy the user's case-only rename (F6, D4).
    pub fn restore_deletion(
        &mut self,
        rendered: &Rendered,
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let refuse = |r: Refused| {
            Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            })
        };
        if let Err(r) = self.restore_preflight(&rendered.path) {
            return refuse(r);
        }
        let root = self.store.root().to_path_buf();
        let case_insensitive = self.case_insensitive;
        if let Some(other) = crate::restore::collision(&root, &rendered.path, case_insensitive) {
            return refuse(Refused::StillPresent {
                path: rendered.path.clone(),
                collides_with: Some(other),
            });
        }
        let baseline = match self.restore_baseline(rendered) {
            Ok(b) => b,
            Err(r) => return refuse(r),
        };
        // `rm -r dir` is the ordinary shape of a deletion, so the directory is often gone
        // too. Rebuilt component by component under the same lstat rule as the preflight.
        if let Err(e) = crate::restore::create_parents(&root, &rendered.path) {
            return match e {
                crate::restore::WriteError::Refuse(reason) => refuse(Refused::Unhashable {
                    path: rendered.path.clone(),
                    reason,
                }),
                crate::restore::WriteError::Moved => refuse(Refused::Moved {
                    path: rendered.path.clone(),
                    live: None,
                }),
                crate::restore::WriteError::Io { path, source } => OpsError::io(path, source),
            };
        }
        let path = rendered.path.clone();
        let mut before = move || {
            fault.at(FaultPoint::AfterTempWrite);
            if fault.fails_at(FaultPoint::AfterTempWrite) {
                return Err(crate::restore::WriteError::Refuse("fault injected".into()));
            }
            match crate::restore::collision(&root, &path, case_insensitive) {
                Some(_) => Err(crate::restore::WriteError::Refuse("appeared".into())),
                None => Ok(()),
            }
        };
        match &baseline {
            // Nothing was there at the baseline and nothing is there now: the row is
            // already at its baseline, and a restore that writes nothing is a success.
            Baseline::Absent => Ok(Outcome::default()),
            Baseline::Empty if self.empty_baseline_means_absent() => Ok(Outcome::default()),
            Baseline::Empty => self.restore_write(
                &rendered.path,
                Some(&[]),
                rendered.baseline_mode,
                &mut before,
            ),
            Baseline::Present { oid, mode } => {
                let content = self.store.cat_blob(oid)?;
                let bytes = if *mode == Mode::Symlink {
                    content
                } else {
                    self.restore_bytes(&rendered.path, &content)?
                };
                self.restore_write(&rendered.path, Some(&bytes), Some(*mode), &mut before)
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Save — the second operation that writes the user's working tree (Phase 8)
    // -----------------------------------------------------------------------------------

    /// Write `bytes` at `rendered.path` under compare-and-swap and set the path's override
    /// to what was written (§6.3 "editor save"; invariant 8).
    ///
    /// **Do not harmonise this with `accept_hunk`.** Accept-hunk has no live CAS on
    /// purpose (Amendment A3): it computes a blob from the baseline and the hunk the user
    /// saw and never touches the working tree, so a file that moved underneath cannot
    /// smuggle anything into the ledger. A save is the opposite case — it *writes* the
    /// working tree — so the live CAS is the entire guard, and, exactly as in a restore, it
    /// runs twice: once here at entry and once more inside `before_rename`.
    ///
    /// The order below is the one rule that is easy to get wrong. The oid is computed from
    /// the buffer's bytes **before anything is written** (design review F1) with
    /// [`Store::hash_bytes_as`], not read back from the file afterwards: `write_bytes`'s
    /// `before_rename` hook takes no arguments and cannot reach the temp file, and a fresh
    /// read after the rename would hash whatever an agent wrote in the
    /// rename-to-ledger window and bless it. Recording the oid of the bytes we wrote means
    /// such a write shows up as **pending** at the rescan, which is invariant 2's
    /// direction.
    ///
    /// The bytes go down verbatim — the caller read the worktree file, CRLF and all, so
    /// what comes back is what the user saw — with the **live** mode, so the executable
    /// bit survives and the override's mode matches what the next scan will `lstat`.
    /// The baseline is never touched.
    ///
    /// §11 residuals inherited from the restore verbatim: the hash-then-rename window
    /// (mitigated, as there, by the rescan every save ends with) and the orphaned open
    /// descriptor.
    pub fn save_file(
        &mut self,
        rendered: &Rendered,
        bytes: &[u8],
        fault: &dyn FaultInjector,
    ) -> Result<Outcome, OpsError> {
        let refuse = |r: Refused| {
            Ok(Outcome {
                refused: vec![r],
                ..Default::default()
            })
        };
        let not_editable = |why: &str| Refused::NotEditable {
            path: rendered.path.clone(),
            why: why.to_owned(),
        };
        // A deletion row has no file to save into, and a symlink's "content" is its target:
        // writing bytes at it would either follow the link or replace it, and neither is an
        // edit of the row the user was looking at.
        if rendered.oid.is_none() {
            return refuse(not_editable("the file is gone"));
        }
        if rendered.mode == Some(Mode::Symlink) {
            return refuse(not_editable("not a regular file"));
        }
        if let Err(r) = self.restore_preflight(&rendered.path) {
            return refuse(r);
        }
        let live = match self.cas_live(rendered) {
            Ok(live) => live,
            Err(r) => return refuse(r),
        };
        // Before any write. A hashing failure is an error, not a refusal, and leaves the
        // working tree untouched.
        let oid = self.store.hash_bytes_as(&rendered.path, bytes)?;
        let mut before = Self::second_cas(rendered, self.store, fault);
        let written = self.restore_write(&rendered.path, Some(bytes), Some(live.mode), &mut before);
        match written {
            Ok(out) if !out.ok() => return Ok(out),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        // The file is on disk; only now does the ledger learn about it. A crash in this
        // window leaves the new bytes and the old ledger — the edit is pending, which is
        // the fail-open direction, and nothing the user typed is lost.
        let key = match Self::key(&rendered.path) {
            Ok(k) => k,
            Err(r) => return refuse(r),
        };
        self.set_override(&key, Some(oid), Some(live.mode));
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

    /// Append a flag to `path` (A8; Amendment v1.7). Never touches `blob`.
    ///
    /// Appends rather than replaces: a review raises several questions about one file, and
    /// the second one must not silently eat the first. `hunk` carries the rendered hunk for
    /// a per-hunk flag and is `None` for a file flag; `summary` is the mirror image — the
    /// row's shape for a whole-file flag, `None` for a hunk flag (Amendment v1.8).
    pub fn flag(
        &mut self,
        path: &[u8],
        note: &str,
        hunk: Option<FlagHunk>,
        summary: Option<FlagSummary>,
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
                flags: Vec::new(),
                updated_at: now.clone(),
            });
        entry.flags.push(Flag {
            note: note.to_owned(),
            created_at: now.clone(),
            // A hunk flag quotes its lines; a whole-file flag carries the row's shape
            // instead. Never both.
            summary: if hunk.is_some() { None } else { summary },
            hunk,
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

    /// Clear **every** flag on `path`; an override left with nothing is removed.
    ///
    /// All of them and not one: Phase 7's TUI has no per-flag removal, so "unflag" is the
    /// undo for the whole path.
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
        entry.flags.clear();
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
    use crate::scan::{Change, Rename, pile_lines};
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
        assert!(
            b.ops()
                .flag(b"f3", "look", None, None, &NoFault)
                .unwrap()
                .ok()
        );
        repo.write("f1", "one more\n");
        let r1 = rendered(&a, b"f1");
        assert!(a.ops().accept_file(&r1, &NoFault).unwrap().ok());
        assert!(!a.ledger.overrides["f3"].flags.is_empty());
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

    /// A deleted row is one hunk with an accept control: `a` on it must accept the
    /// deletion (A7 semantics), not refuse it as "changed since rendered" because the
    /// live file is absent. Found by the sponsor in the Gate 4 run.
    #[test]
    fn ops_accept_hunk_on_a_deletion_row_accepts_the_deletion() {
        let repo = FixtureRepo::new("ops-del-hunk").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.remove("f2");
        let pile = h.scan().pile;
        let row = pile.row(b"f2").unwrap();
        assert_eq!(row.change, Change::Deleted);
        assert_eq!(row.hunks.len(), 1, "a deletion renders as exactly one hunk");
        let rendered = Rendered::of(row);
        assert!(rendered.oid.is_none());
        let out = h
            .ops()
            .accept_hunk(&rendered, &row.hunks, 1, &NoFault)
            .unwrap();
        assert!(matches!(
            out.refused[0],
            Refused::NoSuchHunk { index: 1, .. }
        ));
        let out = h
            .ops()
            .accept_hunk(&rendered, &row.hunks, 0, &NoFault)
            .unwrap();
        assert!(out.refused.is_empty(), "{:?}", out.refused);
        assert!(out.written);
        assert_eq!(h.ledger.overrides.get("f2").unwrap().blob, Some(None));
        assert!(
            h.scan().pile.row(b"f2").is_none(),
            "the deletion is no longer pending"
        );
    }

    /// D5 pairs the pile's rows against their *baselines* (tree ⊕ overrides), never the
    /// seen tree alone: an accepted deletion is not a rename source, a deleted row whose
    /// baseline is an override blob scores against that blob, and a fold (which moves
    /// exactly those overrides into the tree) leaves the pairing untouched.
    #[test]
    fn ops_rename_pairing_reads_baselines_so_compact_keeps_the_pile() {
        let repo = FixtureRepo::new("ops-ren-base").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let other: String = (0..40).map(|i| format!("other {i}\n")).collect();
        repo.write("old.rs", &body);
        repo.write("gone.rs", &body);
        h.mark_seen();
        assert!(h.scan().pile.is_empty());
        // `old.rs` is accepted at a full rewrite: its baseline is now an override blob
        // while the seen tree still holds `body`.
        repo.write("old.rs", &other);
        let r = rendered(&h, b"old.rs");
        assert!(h.ops().accept_file(&r, &NoFault).unwrap().ok());
        // `gone.rs` is accepted as deleted: override `null`, no row.
        repo.remove("gone.rs");
        let r = rendered(&h, b"gone.rs");
        assert!(h.ops().accept_deletion(&r, &NoFault).unwrap().ok());
        assert!(h.scan().pile.is_empty());
        // The accepted content moves, and the accepted-deleted content reappears elsewhere.
        repo.remove("old.rs");
        repo.write("new.rs", &other);
        repo.write("back.rs", &body);
        let pile = h.scan().pile;
        assert_eq!(pile_lines(&pile), vec!["back.rs", "new.rs", "old.rs"]);
        assert!(
            matches!(
                pile.row(b"old.rs").unwrap().rename,
                Some(Rename::To { ref to, similarity }) if to == b"new.rs" && similarity == 100
            ),
            "old.rs pairs at its accepted content, not the tree's: {:?}",
            pile.row(b"old.rs").unwrap().rename
        );
        assert!(matches!(
            pile.row(b"new.rs").unwrap().rename,
            Some(Rename::From { ref from, .. }) if from == b"old.rs"
        ));
        assert_eq!(
            pile.row(b"back.rs").unwrap().rename,
            None,
            "an accepted deletion is not a rename source"
        );
        // Folding the two overrides into the tree changes no baseline, so no pairing.
        h.ops().compact(&NoFault).unwrap();
        assert!(h.ledger.overrides.is_empty(), "folded");
        assert_eq!(h.scan().pile, pile, "compact changed the pile");
        assert!(!h.paths.index_tmp.exists(), "temp index unlinked");
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
        h.ops()
            .flag(b"f3", "keep me", None, None, &NoFault)
            .unwrap();
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
        assert_eq!(o.flags[0].note, "keep me");
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
        h.ops().flag(b"f1", "note", None, None, &NoFault).unwrap();
        let o = h.ledger.overrides.get("f1").unwrap();
        assert!(o.blob.is_none());
        assert!(h.scan().pile.is_empty(), "a flag alone is not pending");
        repo.write("f1", "x\n");
        assert_eq!(h.scan().pile.row(b"f1").unwrap().flags[0].note, "note");
        h.ops().unflag(b"f1", &NoFault).unwrap();
        assert!(!h.ledger.overrides.contains_key("f1"));
        let out = h.ops().unflag(b"nope", &NoFault).unwrap();
        assert!(!out.written);
        let out = h.ops().flag(b"bad\xff", "n", None, None, &NoFault).unwrap();
        assert!(matches!(out.refused[0], Refused::NonUtf8Path { .. }));
    }

    /// Amendment v1.8: a whole-file flag stores the row's shape, a hunk flag never does,
    /// and both survive a ledger round trip.
    #[test]
    fn ops_flag_stores_a_summary_for_a_whole_file_flag_only() {
        let repo = FixtureRepo::new("ops-flag-summary").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let summary = FlagSummary {
            hunks: 3,
            added: 12,
            deleted: 4,
        };
        h.ops()
            .flag(b"f1", "whole", None, Some(summary), &NoFault)
            .unwrap();
        let hunk = FlagHunk {
            index: 1,
            header: "@@ -1,1 +1,1 @@".into(),
            text: "-a\n+b\n".into(),
        };
        // The caller offering both is the TUI's mistake to make; the op takes the hunk and
        // drops the summary rather than writing a flag that claims to be both.
        h.ops()
            .flag(b"f1", "hunk", Some(hunk), Some(summary), &NoFault)
            .unwrap();

        let ledger::LoadResult::Loaded { ledger, .. } = ledger::load(&h.paths, &h.clock).unwrap()
        else {
            panic!("the ledger the flags were written to");
        };
        let flags = &ledger.overrides.get("f1").expect("the override").flags;
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].summary, Some(summary), "the whole-file flag");
        assert!(flags[0].hunk.is_none());
        assert_eq!(flags[1].summary, None, "the hunk flag carries none");
        assert!(flags[1].hunk.is_some());
    }

    // -----------------------------------------------------------------------------------
    // Restore (deliverable 1). The refusals come first, deliberately: every one of them is
    // a case where lastcall must NOT have written the user's file, so each asserts the
    // bytes on disk as well as the `Refused` variant.
    // -----------------------------------------------------------------------------------

    /// Fails softly at one point instead of killing the process, so a test can assert what
    /// the unwind left behind (see [`FaultInjector::fails_at`]).
    struct FailAt(FaultPoint);

    impl FaultInjector for FailAt {
        fn at(&self, _point: FaultPoint) {}
        fn fails_at(&self, point: FaultPoint) -> bool {
            point == self.0
        }
    }

    /// Every restore temp file left anywhere under `dir`.
    fn temp_ghosts(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            if name == ".git" {
                continue;
            }
            if crate::restore::is_restore_temp(name.as_encoded_bytes()) {
                out.push(name.to_string_lossy().into_owned());
            }
            if e.path().is_dir() {
                out.extend(temp_ghosts(&e.path()));
            }
        }
        out
    }

    #[test]
    fn ops_restore_refuses_when_the_file_moved_and_leaves_it_untouched() {
        let repo = FixtureRepo::new("ops-restore-moved").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "rendered\n");
        let r = rendered(&h, b"f1");
        // The agent writes again between the render and the keystroke.
        repo.write("f1", "moved on\n");
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(
            matches!(out.refused.first(), Some(Refused::Moved { .. })),
            "{out:?}"
        );
        assert!(!out.written, "a restore never writes the ledger");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"moved on\n",
            "a refused restore leaves the working file exactly as it was"
        );
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    #[test]
    fn ops_restore_refuses_a_conflicted_path() {
        let mut repo = FixtureRepo::new("ops-restore-conflict").unwrap();
        repo.write("f1", "base\n");
        repo.commit("base").unwrap();
        repo.checkout_b("side").unwrap();
        repo.write("f1", "side\n");
        repo.commit("side").unwrap();
        repo.checkout("main").unwrap();
        repo.write("f1", "main\n");
        repo.commit("main").unwrap();
        // C4: git leaves the conflict in the index and the markers in the file.
        let merged = repo.git(&["merge", "side"]);
        assert!(merged.is_err(), "the merge must conflict: {merged:?}");
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let row = h.scan().pile.row(b"f1").expect("f1 is pending").clone();
        assert!(row.conflicted, "the row is conflicted");
        let before = std::fs::read(repo.path().join("f1")).unwrap();
        let r = Rendered::of(&row);
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(
            matches!(out.refused.first(), Some(Refused::Conflicted { .. })),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            before,
            "one side of an unresolved merge is never overwritten"
        );
    }

    #[test]
    fn ops_restore_refuses_a_symlinked_parent_directory() {
        let mut repo = FixtureRepo::new("ops-restore-parent").unwrap();
        repo.write("d/f1", "one\n");
        repo.commit("dir").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("d/f1", "two\n");
        let r = rendered(&h, b"d/f1");
        // The classic escape: swap the parent for a symlink after the render. `O_NOFOLLOW`
        // on the leaf would not see this, and `hash_path`'s own `lstat` follows `d` — so
        // the CAS would agree and the write would land through the link (F9).
        std::fs::rename(repo.path().join("d"), repo.path().join("d_real")).unwrap();
        std::os::unix::fs::symlink("d_real", repo.path().join("d")).unwrap();
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        match out.refused.first() {
            Some(Refused::Unhashable { reason, .. }) => {
                assert_eq!(reason, "parent is a symlink")
            }
            other => panic!("expected a parent-symlink refusal, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(repo.path().join("d_real/f1")).unwrap(),
            b"two\n",
            "nothing was written through the link"
        );
    }

    #[test]
    fn ops_restore_refuses_a_filtered_path() {
        let mut repo = FixtureRepo::new("ops-restore-filter").unwrap();
        repo.write(".gitattributes", "f1 filter=lfs\n");
        repo.write("f1", "base\n");
        repo.commit("attrs").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "edited\n");
        let r = rendered(&h, b"f1");
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        match out.refused.first() {
            // The store's blobs are canonical (clean-filtered) content; without the
            // driver, writing them back would put an LFS pointer where the user's binary
            // was (F2). Refusing is the only honest answer.
            Some(Refused::Unhashable { reason, .. }) => assert_eq!(reason, "filter=lfs"),
            other => panic!("expected a filter refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(repo.path().join("f1")).unwrap(), b"edited\n");
    }

    /// A truncated hunk list is refused rather than written (verifier F3).
    ///
    /// The probe: a 3,000-line file whose first line and last 2,000 lines are edited. The
    /// row collapses on size, `hunks::expand` truncates at `EXPAND_LINE_CAP`, and the list
    /// that comes back describes only the first change. Both CASes pass — the file has not
    /// moved — so "restore hunk 0" used to write `baseline ⊕ nothing` and take all 2,000
    /// edits with it.
    #[test]
    fn ops_restore_hunk_refuses_a_truncated_hunk_list() {
        let mut repo = FixtureRepo::new("ops-restore-truncated").unwrap();
        let baseline: String = (0..3_000).map(|i| format!("line {i}\n")).collect();
        repo.write("big.txt", &baseline);
        repo.commit("big").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);

        let mut lines: Vec<String> = (0..3_000).map(|i| format!("line {i}\n")).collect();
        lines[0] = "EDIT 0\n".to_string();
        for line in lines.iter_mut().take(3_000).skip(1_000) {
            *line = line.replace("line", "EDIT");
        }
        let edited: String = lines.concat();
        repo.write("big.txt", &edited);

        let r = rendered(&h, b"big.txt");
        let expanded = crate::hunks::expand(baseline.as_bytes(), edited.as_bytes());
        assert!(
            expanded.omitted_lines > 0,
            "the fixture must actually overrun the cap"
        );

        let out = h
            .ops()
            .restore_hunk(&r, &expanded.hunks, 0, &NoFault)
            .unwrap();
        match out.refused.first() {
            Some(rf @ Refused::Incomplete { .. }) => assert_eq!(
                rf.message("restored"),
                "big.txt: only part of the diff is loaded; not restored"
            ),
            other => panic!("expected an incompleteness refusal, got {other:?}"),
        }

        let after = std::fs::read(repo.path().join("big.txt")).unwrap();
        assert_eq!(after, edited.as_bytes(), "the file is untouched");
        assert_eq!(
            after
                .split(|b| *b == b'\n')
                .filter(|l| l.starts_with(b"EDIT"))
                .count(),
            2_001,
            "every edited line survives"
        );
    }

    /// The round-trip guard (F2) refuses only the genuinely lossy case. A plain LF file
    /// under the same bare `* text=auto` that refuses a CRLF file round-trips exactly —
    /// clean to LF, smudge to LF — so it restores as it always did. Without this the guard
    /// would be a blanket refusal of every `text=auto` repo, which is most of them.
    #[test]
    fn ops_restore_under_bare_text_auto_still_restores_a_plain_lf_file() {
        let mut repo = FixtureRepo::new("ops-restore-lf").unwrap();
        repo.write(".gitattributes", "* text=auto\n");
        repo.write("lf.txt", "a\nb\nc\n");
        repo.commit("attrs").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("lf.txt", "a\nB\nc\n");
        let r = rendered(&h, b"lf.txt");
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(out.ok(), "an LF file is round-trippable: {out:?}");
        assert_eq!(
            std::fs::read(repo.path().join("lf.txt")).unwrap(),
            b"a\nb\nc\n",
            "restored to the baseline, endings untouched"
        );
    }

    #[test]
    fn ops_restore_fault_after_the_temp_write_leaves_no_ghost_and_no_change() {
        let repo = FixtureRepo::new("ops-restore-fault").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        let baseline = std::fs::read(repo.path().join("f1")).unwrap();
        repo.write("f1", "edited\n");
        let r = rendered(&h, b"f1");
        let out = h
            .ops()
            .restore_file(&r, &FailAt(FaultPoint::AfterTempWrite))
            .unwrap();
        assert!(!out.ok(), "the fault aborts the restore: {out:?}");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"edited\n",
            "the rename never happened"
        );
        assert!(
            temp_ghosts(repo.path()).is_empty(),
            "the temp file is removed on every error after it exists"
        );
        // And the same restore without the fault does land, so the failure was the fault
        // and not the shape of the request.
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(std::fs::read(repo.path().join("f1")).unwrap(), baseline);
    }

    /// The parent directory is swapped for a symlink while the temp file is on disk
    /// (verifier F7, probe P2).
    ///
    /// Two things went wrong at once and each is fatal on its own. The second CAS only
    /// hashed the *path*, and `hash_path` resolves through the new parent — so a decoy
    /// directory holding the same bytes made the compare agree and the restore wrote the
    /// baseline into a directory the user never pointed at. And the rename and the cleanup
    /// unlink were both by path too, so they went to the decoy while the temp file stayed
    /// behind in the real directory forever.
    #[test]
    fn ops_restore_parent_swapped_after_temp_write_is_moved_and_leaves_no_ghost() {
        /// Swaps `d` for a symlink to a decoy with identical bytes, once, at the moment the
        /// temp file exists and the rename has not happened.
        struct SwapParent {
            root: std::path::PathBuf,
            live: Vec<u8>,
            done: std::cell::Cell<bool>,
        }

        impl FaultInjector for SwapParent {
            fn at(&self, point: FaultPoint) {
                if point != FaultPoint::AfterTempWrite || self.done.replace(true) {
                    return;
                }
                let d = self.root.join("d");
                std::fs::rename(&d, self.root.join("d_real")).unwrap();
                let decoy = self.root.join("d_decoy");
                std::fs::create_dir(&decoy).unwrap();
                // The same bytes and the same mode: the point is that the CAS *agrees*.
                std::fs::write(decoy.join("f1"), &self.live).unwrap();
                std::os::unix::fs::symlink("d_decoy", &d).unwrap();
            }
        }

        let repo = FixtureRepo::new("ops-restore-swap").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("d/f1", "base\n");
        h.mark_seen();
        repo.write("d/f1", "edited\n");
        let r = rendered(&h, b"d/f1");
        let swap = SwapParent {
            root: repo.path().to_path_buf(),
            live: b"edited\n".to_vec(),
            done: std::cell::Cell::new(false),
        };

        let out = h.ops().restore_file(&r, &swap).unwrap();
        assert!(swap.done.get(), "the swap must actually have fired");
        assert!(!out.ok(), "a swapped parent is a refusal: {out:?}");
        assert_eq!(
            out.refused,
            vec![Refused::Moved {
                path: b"d/f1".to_vec(),
                live: None
            }],
            "and the refusal is Moved, not an Io error"
        );

        // The directory that was verified is the one every step of the write acted in, so
        // the temp file was unlinked from it — no ghost, and nothing else left behind.
        let mut real: Vec<String> = std::fs::read_dir(repo.path().join("d_real"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        real.sort();
        assert_eq!(real, vec!["f1".to_string()]);
        assert_eq!(
            std::fs::read(repo.path().join("d_real/f1")).unwrap(),
            b"edited\n",
            "the rename never happened"
        );
        assert_eq!(
            std::fs::read(repo.path().join("d_decoy/f1")).unwrap(),
            b"edited\n",
            "and nothing was written through the symlink into the decoy"
        );
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    #[test]
    fn ops_restore_hunk_leaves_the_other_hunks_pending() {
        let repo = FixtureRepo::new("ops-restore-hunk").unwrap();
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
        let out = h.ops().restore_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(!out.written, "no ledger write");
        // Exactly hunk 0 was undone: the file is the baseline with hunk 1 still applied.
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            base.replace("line 27\n", "LINE 27\n").as_bytes(),
        );
        let after = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(after.hunks.len(), 1, "the other hunk is still pending");
        assert!(after.hunks[0].lines.iter().any(|(_, l)| l == b"LINE 27\n"));
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    #[test]
    fn ops_restore_hunk_on_a_row_with_a_mode_hunk_writes_no_mode_bytes() {
        let repo = FixtureRepo::new("ops-restore-hunk-mode").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        if !h.store.filemode() {
            eprintln!("skipped: the root does not honour core.filemode");
            return;
        }
        let base: String = (0..30).map(|i| format!("line {i}\n")).collect();
        repo.write("f1", &base);
        h.mark_seen();
        let edited = base
            .replace("line 2\n", "LINE 2\n")
            .replace("line 27\n", "LINE 27\n");
        repo.write("f1", &edited);
        repo.chmod_x("f1", true);
        let row = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(row.hunks.len(), 3, "two content hunks and the mode hunk");
        assert!(row.hunks[2].is_mode_change());
        let r = Rendered::of(&row);
        let out = h.ops().restore_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        // F1: the mode hunk's literal `mode 100755` line must never reach `apply_hunks`,
        // which would splice it in at offset 0.
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            base.replace("line 27\n", "LINE 27\n").as_bytes(),
        );
        assert_eq!(
            h.scan()
                .pile
                .row(b"f1")
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .mode,
            Mode::Executable,
            "a content-hunk restore leaves the mode where the live file has it"
        );
    }

    #[test]
    fn ops_restore_mode_only_hunk_chmods_and_writes_no_bytes() {
        let repo = FixtureRepo::new("ops-restore-mode").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        if !h.store.filemode() {
            eprintln!("skipped: the root does not honour core.filemode");
            return;
        }
        repo.chmod_x("f1", true);
        let row = h.scan().pile.row(b"f1").unwrap().clone();
        assert_eq!(row.hunks.len(), 1);
        assert!(row.hunks[0].is_mode_change(), "D1: mode only");
        let before = std::fs::read(repo.path().join("f1")).unwrap();
        let r = Rendered::of(&row);
        let out = h.ops().restore_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            before,
            "a mode restore writes no bytes"
        );
        assert!(
            h.scan().pile.is_empty(),
            "the mode is back at the baseline, so nothing is pending"
        );
        assert!(
            temp_ghosts(repo.path()).is_empty(),
            "the mode path never makes a temp file at all"
        );
    }

    #[test]
    fn ops_restore_removes_an_added_file() {
        let repo = FixtureRepo::new("ops-restore-added").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("added", "new\n");
        let row = h.scan().pile.row(b"added").unwrap().clone();
        assert_eq!(row.change, Change::Added);
        let r = Rendered::of(&row);
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(
            !repo.path().join("added").exists(),
            "a file that did not exist at the baseline is removed, not truncated"
        );
        assert!(h.scan().pile.is_empty());
    }

    /// The hunk route reaches the same place the file route does for an added file
    /// (verifier F4). It used to write a zero-byte file and leave the row pending.
    #[test]
    fn ops_restore_hunk_on_an_added_file_removes_it() {
        let repo = FixtureRepo::new("ops-restore-added-hunk").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("added", "new\n");
        let row = h.scan().pile.row(b"added").unwrap().clone();
        assert_eq!(row.change, Change::Added);
        assert_eq!(
            row.hunks.len(),
            1,
            "the addition renders as one content hunk"
        );
        let r = Rendered::of(&row);
        let out = h.ops().restore_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(
            !repo.path().join("added").exists(),
            "the row's only content hunk is the file: restoring it removes the file"
        );
        assert!(h.scan().pile.is_empty(), "and the row is gone");
    }

    /// The neighbouring case that must NOT remove: a first-sight root, where every row is
    /// "added" but nothing has a baseline to go back to (F17).
    #[test]
    fn ops_restore_hunk_at_first_sight_does_not_remove() {
        let repo = FixtureRepo::new("ops-restore-added-hunk-fs").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        h.ledger.seen_tree = None;
        repo.write("added", "new\n");
        let row = h.scan().pile.row(b"added").unwrap().clone();
        let r = Rendered::of(&row);
        let out = h.ops().restore_hunk(&r, &row.hunks, 0, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(
            repo.path().join("added").exists(),
            "with no seen tree there is no baseline to go back to; the draft survives"
        );
    }

    #[test]
    fn ops_restore_at_first_sight_writes_a_zero_byte_file_and_never_removes() {
        let repo = FixtureRepo::new("ops-restore-firstsight").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        // F17's case: a root with no seen tree at all (a draft root at
        // `draft_initial = pending`). Every row is "added" there, but nothing has a
        // baseline to go back to, so a restore must not destroy the user's file.
        h.ledger.seen_tree = None;
        h.tree_entries = TreeEntries::new();
        repo.write("f1", "draft\n");
        let r = rendered(&h, b"f1");
        let out = h.ops().restore_file(&r, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(
            repo.path().join("f1").exists(),
            "F17: `Baseline::Empty` at first sight is never a removal"
        );
        assert_eq!(std::fs::read(repo.path().join("f1")).unwrap(), b"");
    }

    #[test]
    fn ops_restore_deletion_recreates_a_missing_parent() {
        let mut repo = FixtureRepo::new("ops-restore-mkparent").unwrap();
        repo.write("d/sub/f1", "content\n");
        repo.commit("nested").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        // `rm -r d` is the ordinary shape of this deletion, so the directories go too.
        repo.remove("d/sub/f1");
        std::fs::remove_dir(repo.path().join("d/sub")).unwrap();
        std::fs::remove_dir(repo.path().join("d")).unwrap();
        let row = h.scan().pile.row(b"d/sub/f1").unwrap().clone();
        assert_eq!(row.change, Change::Deleted);
        let r = Rendered::of(&row);
        let out = h.ops().restore_deletion(&r, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(
            std::fs::read(repo.path().join("d/sub/f1")).unwrap(),
            b"content\n"
        );
        assert!(h.scan().pile.is_empty());
    }

    // -----------------------------------------------------------------------------------
    // Save (Phase 8 deliverable 1)
    // -----------------------------------------------------------------------------------

    /// Gate item 1 at the engine: an editor save produces zero new pending for the saved
    /// content, and the override the ledger records is the file's real post-write hash.
    #[test]
    fn ops_save_file_is_cas_and_the_rescan_shows_zero_pending() {
        let repo = FixtureRepo::new("ops-save").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "one\ntwo\n");
        let r = rendered(&h, b"f1");
        let out = h
            .ops()
            .save_file(&r, b"one\nTWO\nthree\n", &NoFault)
            .unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(out.written, "a save writes the ledger, unlike a restore");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"one\nTWO\nthree\n",
            "the buffer's bytes land verbatim"
        );
        assert!(
            h.scan().pile.is_empty(),
            "invariant 8: the user never reviews their own just-typed change"
        );
        let live = match h.store.hash_path(b"f1") {
            Current::Present { oid, .. } => oid,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            h.ledger.overrides["f1"].blob,
            Some(Some(live)),
            "the override is the hash of what is on disk"
        );
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    /// The CAS refuses and — the half that matters — writes nothing at all.
    #[test]
    fn ops_save_file_refuses_when_the_file_moved_and_leaves_it_untouched() {
        let repo = FixtureRepo::new("ops-save-moved").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "rendered\n");
        let r = rendered(&h, b"f1");
        // The agent writes between the buffer being read and Ctrl-S.
        repo.write("f1", "the agent got there first\n");
        let out = h.ops().save_file(&r, b"my edit\n", &NoFault).unwrap();
        assert!(
            matches!(out.refused.first(), Some(Refused::Moved { .. })),
            "{out:?}"
        );
        assert!(!out.written, "a refused save never touches the ledger");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"the agent got there first\n",
            "a refused save leaves the working file byte for byte as it was"
        );
        assert!(!h.ledger.overrides.contains_key("f1"));
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    /// The mode goes down with the bytes, and the proof is the *rescan*: the scan compares
    /// `(oid, mode)`, so an override whose mode disagreed with the file would leave the row
    /// pending even though the content matched.
    #[test]
    fn ops_save_file_keeps_the_executable_bit() {
        let mut repo = FixtureRepo::new("ops-save-exec").unwrap();
        repo.write("s.sh", "#!/bin/sh\necho one\n");
        repo.chmod_x("s.sh", true);
        repo.commit("script").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        if !h.store.filemode() {
            return; // a root that ignores the executable bit has nothing to prove
        }
        repo.write("s.sh", "#!/bin/sh\necho two\n");
        repo.chmod_x("s.sh", true);
        let r = rendered(&h, b"s.sh");
        assert_eq!(r.mode, Some(Mode::Executable));
        let out = h
            .ops()
            .save_file(&r, b"#!/bin/sh\necho three\n", &NoFault)
            .unwrap();
        assert!(out.ok(), "{out:?}");
        use std::os::unix::fs::PermissionsExt;
        let bits = std::fs::metadata(repo.path().join("s.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert!(bits & 0o111 != 0, "still executable: {bits:o}");
        assert!(
            h.scan().pile.is_empty(),
            "the rescan agrees on content and mode"
        );
    }

    /// D3 through a save: a CRLF file under `* text=auto` goes back with its CRLFs intact,
    /// the override is the *filtered* oid git will compute for it, and nothing pends.
    #[test]
    fn scenario_d3_save_of_a_crlf_text_auto_file_round_trips() {
        let mut repo = FixtureRepo::new("ops-save-crlf").unwrap();
        repo.write(".gitattributes", "* text=auto\n");
        repo.write("crlf.txt", "a\r\nb\r\nc\r\n");
        repo.commit("crlf").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("crlf.txt", "a\r\nB\r\nc\r\n");
        let r = rendered(&h, b"crlf.txt");
        let edited: &[u8] = b"a\r\nB\r\nC\r\n";
        let out = h.ops().save_file(&r, edited, &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(
            std::fs::read(repo.path().join("crlf.txt")).unwrap(),
            edited,
            "the CRLFs the user saw are the CRLFs that go back"
        );
        assert_eq!(
            h.ledger.overrides["crlf.txt"].blob,
            Some(Some(h.store.hash_bytes(b"a\nB\nC\n").unwrap())),
            "the override is the LF-normalised blob git stores, not the raw bytes"
        );
        assert!(h.scan().pile.is_empty());
    }

    /// Neither a deletion row nor a symlink is an editable file, and the refusal says so
    /// without pretending the CAS failed.
    #[test]
    fn ops_save_file_refuses_a_symlink_and_a_deletion() {
        let mut repo = FixtureRepo::new("ops-save-noteditable").unwrap();
        repo.symlink("f1", "link");
        repo.commit("link").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.remove("f2");
        repo.remove("link");
        repo.symlink("f3", "link");
        let gone = rendered(&h, b"f2");
        assert!(gone.oid.is_none());
        let out = h.ops().save_file(&gone, b"resurrect\n", &NoFault).unwrap();
        match out.refused.first() {
            Some(Refused::NotEditable { why, .. }) => assert_eq!(why, "the file is gone"),
            other => panic!("{other:?}"),
        }
        assert!(!repo.path().join("f2").exists(), "nothing was recreated");

        let link = rendered(&h, b"link");
        assert_eq!(link.mode, Some(Mode::Symlink));
        let out = h.ops().save_file(&link, b"bytes\n", &NoFault).unwrap();
        match out.refused.first() {
            Some(Refused::NotEditable { why, .. }) => assert_eq!(why, "not a regular file"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_link(repo.path().join("link")).unwrap(),
            std::path::Path::new("f3"),
            "the link is untouched and nothing was written through it"
        );
        assert_eq!(
            Refused::NotEditable {
                path: b"link".to_vec(),
                why: "not a regular file".into(),
            }
            .to_string(),
            "link: not a regular file; not saved"
        );
    }

    /// F3 — a draft (non-git) root behaves exactly as a git root does.
    #[test]
    fn scenario_f3_save_on_a_draft_root() {
        let mut d = proptests::Draft::new("lc-save-draft");
        let mut files = BTreeMap::new();
        files.insert("n.md".to_owned(), b"one\ntwo\n".to_vec());
        d.reset(&files);
        d.write("n.md", b"one\nTWO\n");
        let pile = d.scan();
        let r = Rendered::of(pile.row(b"n.md").expect("pending"));
        let out = d
            .ops()
            .save_file(&r, b"one\nTWO\nthree\n", &NoFault)
            .unwrap();
        assert!(out.ok(), "{out:?}");
        assert_eq!(
            std::fs::read(d.root.join("n.md")).unwrap(),
            b"one\nTWO\nthree\n"
        );
        assert!(d.scan().is_empty(), "zero pending on a draft root too");
    }

    /// Drops `ledger.json.tmp` at [`FaultPoint::AfterLedgerTmpWrite`], leaving exactly the
    /// on-disk state a crash before the rename would (the `engine.rs` `DropTmp` twin).
    struct DropLedgerTmp(std::path::PathBuf);

    impl FaultInjector for DropLedgerTmp {
        fn at(&self, point: FaultPoint) {
            if point == FaultPoint::AfterLedgerTmpWrite {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    /// F10: the file is written and the process dies before the ledger lands. Nothing the
    /// user typed is lost — it is on disk — and the edit is simply **pending**, which is
    /// the fail-open direction invariant 2 asks for.
    #[test]
    fn ops_save_file_that_dies_before_the_ledger_shows_the_edit_pending() {
        let repo = FixtureRepo::new("ops-save-e1").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        // One clean save first, so there is a real ledger on disk to compare against.
        repo.write("f2", "kept\n");
        let r2 = rendered(&h, b"f2");
        assert!(h.ops().save_file(&r2, b"kept\n", &NoFault).unwrap().ok());
        repo.write("f1", "one\n");
        let r = rendered(&h, b"f1");
        let tmp = h.paths.ledger.with_extension("json.tmp");
        let err = h
            .ops()
            .save_file(&r, b"my edit\n", &DropLedgerTmp(tmp))
            .expect_err("the ledger rename fails");
        assert!(matches!(err, OpsError::Ledger(_)), "{err}");
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"my edit\n",
            "the buffer is safely on disk"
        );
        // The in-memory ledger still holds the staged override; the engine seam drops it.
        // What the *next* engine sees is the on-disk ledger, which never learned anything.
        let disk = match ledger::load(&h.paths, &h.clock).unwrap() {
            LoadResult::Loaded { ledger, .. } => ledger,
            other => panic!("{other:?}"),
        };
        assert!(
            !disk.overrides.contains_key("f1"),
            "the ledger on disk never learned about the save"
        );
        assert!(
            disk.overrides.contains_key("f2"),
            "and the earlier save is still there"
        );
        h.ledger = disk;
        assert_eq!(
            pile_lines(&h.scan().pile),
            vec!["f1"],
            "the edit shows as pending rather than being lost"
        );
        assert!(temp_ghosts(repo.path()).is_empty());
    }

    /// F10, the other fault point: a failure while the temp file is on disk leaves neither
    /// a changed file nor a ghost.
    #[test]
    fn ops_save_file_that_fails_before_the_rename_leaves_no_trace() {
        let repo = FixtureRepo::new("ops-save-temp-fault").unwrap();
        let state = TempDir::new("lc-ops");
        let mut h = Harness::new(&repo, &state);
        repo.write("f1", "one\n");
        let r = rendered(&h, b"f1");
        let out = h
            .ops()
            .save_file(&r, b"my edit\n", &FailAt(FaultPoint::AfterTempWrite))
            .unwrap();
        assert!(!out.ok(), "the fault aborts the save: {out:?}");
        assert!(!out.written);
        assert_eq!(
            std::fs::read(repo.path().join("f1")).unwrap(),
            b"one\n",
            "the rename never happened"
        );
        assert!(temp_ghosts(repo.path()).is_empty());
        assert!(!h.ledger.overrides.contains_key("f1"));
        // The same save without the fault lands, so the failure was the fault.
        let out = h.ops().save_file(&r, b"my edit\n", &NoFault).unwrap();
        assert!(out.ok(), "{out:?}");
        assert!(h.scan().pile.is_empty());
    }

    /// Kickoff deliverable 8 (ii)/(iii): property tests over one draft root per test — a
    /// private store on a temp dir, no user repo, the `RootKind::Draft` plumbing exactly as
    /// the engine opens one. Each case rewrites the file set and resets the ledger to first
    /// sight before running its ops. Driven through `TestRunner` (not `proptest!`) so the
    /// fixture is built once per test and shared across cases; failure persistence is off
    /// because a manual runner has no source file to write beside, and a case is fully
    /// determined by its inputs anyway.
    mod proptests {
        use std::cell::RefCell;
        use std::path::PathBuf;

        use globset::GlobSet;
        use lastcall_testkit::tmp::TempDir;
        use proptest::prelude::*;
        use proptest::test_runner::{TestCaseError, TestRunner};

        use super::*;
        use crate::engine::DEFAULT_ROW_CAP;
        use crate::env::Env;
        use crate::hunks::Tag;
        use crate::ledger::FixedClock;
        use crate::scan::{Change, ScanInputs};

        pub(super) struct Draft {
            _dir: TempDir,
            pub(super) root: PathBuf,
            store: Store,
            index: PrivateIndex,
            ledger: Ledger,
            tree: TreeEntries,
            paths: RepoPaths,
            globs: GlobSet,
            clock: FixedClock,
        }

        impl Draft {
            pub(super) fn new(name: &str) -> Self {
                let dir = TempDir::new(name);
                let root = dir.mkdir("draft");
                let state = dir.mkdir("state");
                let env = Env::empty(dir.path())
                    .with_home(dir.mkdir("home"))
                    .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
                    .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
                    .with_var("GIT_CONFIG_NOSYSTEM", "1")
                    .with_var("LASTCALL_STATE_DIR", state.to_string_lossy());
                let paths = RepoPaths::under(state.join("repo"));
                let (store, notices) =
                    Store::open(&env, &root, RootKind::Draft, &paths, None).unwrap();
                assert!(notices.is_empty(), "{notices:?}");
                let index = PrivateIndex::new(store.git().clone(), &paths, RootKind::Draft, None);
                let ledger = Ledger::new(&root, RootKind::Draft, None, Self::seen_at());
                Self {
                    _dir: dir,
                    root,
                    store,
                    index,
                    ledger,
                    tree: TreeEntries::new(),
                    paths,
                    globs: GlobSet::empty(),
                    clock: FixedClock::at_unix(1_800_000_000),
                }
            }

            fn seen_at() -> SeenAt {
                SeenAt {
                    head_commit: None,
                    branch: None,
                    at: "2026-01-01T00:00:00Z".into(),
                }
            }

            /// Make `files` the whole worktree and the ledger a first sight of it.
            pub(super) fn reset(&mut self, files: &BTreeMap<String, Vec<u8>>) {
                for entry in std::fs::read_dir(&self.root).unwrap() {
                    std::fs::remove_file(entry.unwrap().path()).unwrap();
                }
                for (name, bytes) in files {
                    self.write(name, bytes);
                }
                let seen = self.store.tree_of_disk().unwrap();
                self.tree = self.store.ls_tree(&seen).unwrap();
                self.ledger = Ledger::new(&self.root, RootKind::Draft, Some(seen), Self::seen_at());
                ledger::save(&self.paths, &self.ledger).unwrap();
            }

            pub(super) fn write(&self, name: &str, bytes: &[u8]) {
                std::fs::write(self.root.join(name), bytes).unwrap();
            }

            fn set(&self, name: &str, content: Option<&[u8]>) {
                match content {
                    Some(b) => self.write(name, b),
                    None => {
                        let _ = std::fs::remove_file(self.root.join(name));
                    }
                }
            }

            /// Disk content per name (`None` = absent).
            fn disk(&self, names: &[&str]) -> BTreeMap<String, Option<Vec<u8>>> {
                names
                    .iter()
                    .map(|n| ((*n).to_owned(), std::fs::read(self.root.join(n)).ok()))
                    .collect()
            }

            pub(super) fn scan(&self) -> Pile {
                crate::scan::scan(&ScanInputs {
                    store: &self.store,
                    index: &self.index,
                    repo: None,
                    ledger: &self.ledger,
                    seen_tree: self.ledger.seen_tree.as_ref(),
                    tree: &self.tree,
                    case_insensitive: false,
                    collapsed_globs: &self.globs,
                    collapse_size_bytes: 1 << 20,
                    excluded_dirs: &[],
                    index_tmp: &self.paths.index_tmp,
                    row_cap: DEFAULT_ROW_CAP,
                })
                .unwrap()
                .pile
            }

            pub(super) fn ops(&mut self) -> Ops<'_> {
                Ops {
                    store: &self.store,
                    index: &self.index,
                    repo: None,
                    paths: &self.paths,
                    ledger: &mut self.ledger,
                    tree: &mut self.tree,
                    clock: &self.clock,
                    compaction_threshold: 500,
                    case_insensitive: false,
                    staged: BTreeMap::new(),
                    lock: DEFAULT_LOCK,
                }
            }
        }

        /// 8 cases in the unit tier, `PROPTEST_CASES` (64 from the pre-push hook and CI)
        /// when set — see `crate::env::proptest_cases`. The pure `hunks` proptest keeps
        /// its own 1000.
        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: crate::env::proptest_cases(),
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        const NAMES: [&str; 5] = ["f0", "f1", "f2", "f3", "f4"];

        /// Short line-based content from a six-letter alphabet, so diffs have context.
        fn content() -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(0..6u8, 0..8).prop_map(|v| {
                v.iter()
                    .map(|b| format!("l{b}\n"))
                    .collect::<String>()
                    .into_bytes()
            })
        }

        fn name() -> impl Strategy<Value = String> {
            (0..NAMES.len()).prop_map(|i| NAMES[i].to_owned())
        }

        fn file_set() -> impl Strategy<Value = BTreeMap<String, Vec<u8>>> {
            prop::collection::btree_map(name(), content(), 0..NAMES.len())
        }

        /// A batch of edits: set a name to content, or remove it.
        fn edits() -> impl Strategy<Value = Vec<(String, Option<Vec<u8>>)>> {
            prop::collection::vec((name(), prop::option::of(content())), 0..5)
        }

        fn sorted_changes(hunks: &[Hunk]) -> Vec<(Tag, Vec<u8>)> {
            let mut v: Vec<(Tag, Vec<u8>)> = hunks.iter().flat_map(Hunk::change_lines).collect();
            v.sort();
            v
        }

        #[test]
        fn ops_accept_all_then_edits_pends_exactly_the_post_snapshot_delta_and_compact_never_changes_the_pile()
         {
            let d = RefCell::new(Draft::new("lc-prop-all"));
            let mut runner = TestRunner::new(config());
            let result = runner.run(
                &(file_set(), edits(), edits(), 0..8usize),
                |(files, first, second, pick)| {
                    let mut d = d.borrow_mut();
                    d.reset(&files);
                    for (name, c) in &first {
                        d.set(name, c.as_deref());
                    }
                    let snapshot = d.scan();
                    let out = d.ops().accept_all(&snapshot, &NoFault).unwrap();
                    prop_assert!(out.ok());
                    // (A file `second` leaves alone must show no row below: accept-all
                    // cleared it.)
                    let post = d.disk(&NAMES);
                    for (name, c) in &second {
                        d.set(name, c.as_deref());
                    }
                    let now = d.disk(&NAMES);
                    let pile = d.scan();
                    prop_assert_eq!(pile.omitted, 0);
                    let changed = NAMES.iter().filter(|n| post[**n] != now[**n]).count();
                    prop_assert_eq!(pile.rows.len(), changed, "rows: {:?}", pile_lines(&pile));
                    for name in NAMES {
                        let (was, is) = (&post[name], &now[name]);
                        let row = pile.row(name.as_bytes());
                        if was == is {
                            prop_assert!(
                                row.is_none(),
                                "{name}: unchanged since the snapshot, yet pending"
                            );
                            continue;
                        }
                        prop_assert!(
                            row.is_some(),
                            "{name}: changed since the snapshot, yet not pending"
                        );
                        let row = row.unwrap();
                        let expected = match (was, is) {
                            (None, Some(_)) => Change::Added,
                            (Some(_), None) => Change::Deleted,
                            _ => Change::Modified,
                        };
                        prop_assert_eq!(row.change, expected, "{}", name);
                        if let (Some(w), Some(i)) = (was, is) {
                            prop_assert_eq!(
                                &row.hunks,
                                &hunks::diff(w, i),
                                "{}: not exactly the post-snapshot delta",
                                name
                            );
                        }
                    }
                    // A fold never changes the pile — with a fresh override in the mix too.
                    if !pile.rows.is_empty() {
                        let r = Rendered::of(&pile.rows[pick % pile.rows.len()]);
                        let out = d.ops().accept_file(&r, &NoFault).unwrap();
                        prop_assert!(out.ok(), "{:?}", out);
                    }
                    let before = d.scan();
                    d.ops().compact(&NoFault).unwrap();
                    prop_assert!(
                        d.ledger.overrides.values().all(|o| o.blob.is_none()),
                        "folded"
                    );
                    prop_assert_eq!(d.scan(), before, "compact changed the pile");
                    Ok(())
                },
            );
            if let Err(e) = result {
                panic!("{e}");
            }
        }

        /// Replay one `(files, first, second, pick)` case of the proptest above by hand:
        /// the pile before and after `compact`.
        fn replay_compact_case(
            name: &str,
            files: &[(&str, &[u8])],
            first: &[(&str, Option<&[u8]>)],
            second: &[(&str, Option<&[u8]>)],
            pick: usize,
        ) -> (Pile, Pile) {
            let mut d = Draft::new(name);
            let files: BTreeMap<String, Vec<u8>> = files
                .iter()
                .map(|(n, c)| ((*n).to_owned(), c.to_vec()))
                .collect();
            d.reset(&files);
            for (n, c) in first {
                d.set(n, *c);
            }
            let snapshot = d.scan();
            assert!(d.ops().accept_all(&snapshot, &NoFault).unwrap().ok());
            for (n, c) in second {
                d.set(n, *c);
            }
            let pile = d.scan();
            if !pile.rows.is_empty() {
                let r = Rendered::of(&pile.rows[pick % pile.rows.len()]);
                assert!(d.ops().accept_file(&r, &NoFault).unwrap().ok());
            }
            let before = d.scan();
            d.ops().compact(&NoFault).unwrap();
            (before, d.scan())
        }

        #[test]
        fn ops_compact_keeps_the_pile_when_an_accepted_deletion_resembles_an_addition_a() {
            let (before, after) = replay_compact_case(
                "lc-compact-a",
                &[("f3", b""), ("f4", b"l2\nl0\nl3\n")],
                &[],
                &[("f4", None), ("f3", None), ("f0", Some(b"l2\nl0\n"))],
                2,
            );
            assert_eq!(after, before, "compact changed the pile");
        }

        #[test]
        fn ops_compact_keeps_the_pile_when_an_accepted_deletion_resembles_an_addition_b() {
            let (before, after) = replay_compact_case(
                "lc-compact-b",
                &[],
                &[("f4", Some(b"l1\nl3\nl1\n")), ("f1", Some(b""))],
                &[("f0", Some(b"l1\nl1\n")), ("f4", None), ("f1", None)],
                2,
            );
            assert_eq!(after, before, "compact changed the pile");
        }

        /// A baseline of unique lines and a current side with a few separated edits
        /// (delete / insert / replace at distinct spots), so every change line names its
        /// hunk and the hunks proptest's oracle applies.
        fn hunky_file(tag: &'static str) -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
            (
                6..24usize,
                prop::collection::btree_set(0..24usize, 1..5),
                prop::collection::vec(0..3u8, 5),
            )
                .prop_map(move |(n, spots, kinds)| {
                    let base: Vec<String> = (0..n).map(|i| format!("{tag}{i}\n")).collect();
                    let mut cur = base.clone();
                    for (k, spot) in spots.into_iter().filter(|s| *s < n).rev().enumerate() {
                        match kinds[k % kinds.len()] {
                            0 => {
                                cur.remove(spot);
                            }
                            1 => cur.insert(spot + 1, format!("{tag}new{spot}\n")),
                            _ => cur[spot] = format!("{tag}rep{spot}\n"),
                        }
                    }
                    (base.concat().into_bytes(), cur.concat().into_bytes())
                })
        }

        /// A base and a current side with **exactly two** well-separated replacements, so
        /// the diff is always two hunks and a hunk accept is never the same thing as a file
        /// accept. Yields `(base, cur, marker_a, marker_b)` where the markers are the two
        /// changed lines on the current side, each unique in the file.
        fn two_hunk_file() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
            (26..44usize, 0..6usize, 0..6usize).prop_map(|(n, da, db)| {
                // `a` is at most 7 and `b` at least `n - 9`, so the two spots are never
                // closer than 26 - 9 - 7 = 10 lines: three lines of context on each side
                // plus one between can never let the changes fall into a single hunk.
                let a = 2 + da;
                let b = n - 3 - db;
                let base: Vec<String> = (0..n).map(|i| format!("t{i}\n")).collect();
                let mut cur = base.clone();
                let (ma, mb) = (format!("A{a}\n"), format!("B{b}\n"));
                cur[a] = ma.clone();
                cur[b] = mb.clone();
                (
                    base.concat().into_bytes(),
                    cur.concat().into_bytes(),
                    ma.into_bytes(),
                    mb.into_bytes(),
                )
            })
        }

        /// Amendment A3: a hunk accept has no live CAS. The user is shown a baseline and a
        /// hunk against it; that is still true however the working tree moved since, so the
        /// accept lands. Before A3 an agent writing to the file during the review turned
        /// every hunk accept into a `Refused::Moved`.
        ///
        /// What lands is `apply_hunks(baseline, rendered_hunks, [k])` — the baseline plus
        /// exactly the hunk the user saw, never a byte the edit brought in. The check is
        /// therefore the *re-diff* against that new override, not a whole-file equality:
        /// the next render must be exactly what the new override and the new disk differ by.
        #[test]
        fn ops_hunk_accept_lands_despite_an_unseen_edit_and_writes_only_the_rendered_hunk() {
            let d = RefCell::new(Draft::new("lc-prop-a3-unseen"));
            let mut runner = TestRunner::new(config());
            let result = runner.run(
                &(two_hunk_file(), any::<bool>(), 0..6usize),
                |((base, cur, ma, mb), pick_b, tail)| {
                    let mut d = d.borrow_mut();
                    d.reset(&BTreeMap::from([("f".to_owned(), base.clone())]));
                    d.write("f", &cur);
                    let row = d.scan().row(b"f").cloned().expect("f is pending");
                    let rendered = Rendered::of(&row);
                    prop_assert_eq!(row.hunks.len(), 2, "the fixture must have two hunks");
                    let k = usize::from(pick_b);

                    // The unseen edit: an agent appends to the file after the render. It
                    // touches neither hunk's lines, so nothing about the accept is stale
                    // except the live content the old CAS used to insist on.
                    let mut live = cur.clone();
                    live.extend_from_slice(format!("late{tail}\n").as_bytes());
                    d.write("f", &live);

                    let out = d
                        .ops()
                        .accept_hunk(&rendered, &row.hunks, k, &NoFault)
                        .unwrap();
                    prop_assert!(
                        out.ok(),
                        "A3: the live file moved, the accept still lands: {:?}",
                        out
                    );

                    // The override is the baseline plus that one hunk, and the next render
                    // is exactly the re-diff of it against what is on disk now.
                    let expected = hunks::apply_hunks(&base, &row.hunks, &[k]);
                    let after = d.scan().row(b"f").cloned().expect("the rest is pending");
                    prop_assert_eq!(
                        &after.hunks,
                        &hunks::diff(&expected, &live),
                        "not the re-diff against the new override"
                    );
                    // Concretely: the accepted marker is gone, the other one and the unseen
                    // edit are still pending.
                    let lines = sorted_changes(&after.hunks);
                    let has = |m: &[u8]| lines.iter().any(|(t, l)| *t == Tag::Insert && l == m);
                    let (accepted, other) = if k == 0 { (&ma, &mb) } else { (&mb, &ma) };
                    prop_assert!(!has(accepted), "the accepted hunk is still shown");
                    prop_assert!(has(other), "the other hunk stopped being pending");
                    prop_assert!(
                        has(format!("late{tail}\n").as_bytes()),
                        "the unseen edit is not pending"
                    );
                    Ok(())
                },
            );
            result.unwrap();
        }

        /// The other half of A3: an accepted hunk's lines being rewritten *in place* must
        /// show as `X → Y`, not as the original `base → Y`. The accept moved the baseline
        /// to include X, so the next diff starts from X — which is the whole point of
        /// storing `apply_hunks(baseline, .., [k])` rather than the live blob.
        #[test]
        fn ops_rewriting_an_accepted_hunk_in_place_renders_the_accepted_lines_as_the_old_side() {
            let d = RefCell::new(Draft::new("lc-prop-a3-rewrite"));
            let mut runner = TestRunner::new(config());
            let result = runner.run(
                &(two_hunk_file(), any::<bool>(), 0..6usize),
                |((base, cur, ma, mb), pick_b, y)| {
                    let mut d = d.borrow_mut();
                    d.reset(&BTreeMap::from([("f".to_owned(), base.clone())]));
                    d.write("f", &cur);
                    let row = d.scan().row(b"f").cloned().expect("f is pending");
                    prop_assert_eq!(row.hunks.len(), 2, "the fixture must have two hunks");
                    let k = usize::from(pick_b);
                    let (x, other) = if k == 0 { (&ma, &mb) } else { (&mb, &ma) };
                    let out = d
                        .ops()
                        .accept_hunk(&Rendered::of(&row), &row.hunks, k, &NoFault)
                        .unwrap();
                    prop_assert!(out.ok(), "{:?}", out);

                    // Rewrite X's line to Y, in place, leaving everything else alone.
                    let yline = format!("Y{y}\n").into_bytes();
                    let live = String::from_utf8(cur.clone())
                        .unwrap()
                        .replace(
                            std::str::from_utf8(x).unwrap(),
                            std::str::from_utf8(&yline).unwrap(),
                        )
                        .into_bytes();
                    d.write("f", &live);

                    let after = d.scan().row(b"f").cloned().expect("still pending");
                    // The X region shows exactly X → Y: one hunk, and its old side is the
                    // line the user accepted, not the line the base had there.
                    let xy: Vec<&Hunk> = after
                        .hunks
                        .iter()
                        .filter(|h| h.change_lines().iter().any(|(_, l)| *l == yline))
                        .collect();
                    prop_assert_eq!(
                        xy.len(),
                        1,
                        "one hunk covers the rewrite: {:?}",
                        after.hunks
                    );
                    let changes = xy[0].change_lines();
                    prop_assert!(
                        changes.contains(&(Tag::Delete, x.clone())),
                        "the old side must be the accepted line {:?}, not the base's: {:?}",
                        String::from_utf8_lossy(x),
                        changes
                    );
                    prop_assert!(
                        !changes
                            .iter()
                            .any(|(t, l)| *t == Tag::Delete && l.starts_with(b"t") && *l != *x),
                        "the base's line at that spot is not the old side: {:?}",
                        changes
                    );
                    prop_assert!(changes.contains(&(Tag::Insert, yline.clone())));
                    // And the hunk that was never accepted is still pending, untouched.
                    let lines = sorted_changes(&after.hunks);
                    prop_assert!(
                        lines.contains(&(Tag::Insert, other.clone())),
                        "the unaccepted hunk stopped being pending"
                    );
                    Ok(())
                },
            );
            result.unwrap();
        }

        /// One op: `(file b?, accept the whole file?, hunk pick)`.
        fn ops_seq() -> impl Strategy<Value = Vec<(bool, bool, usize)>> {
            prop::collection::vec((any::<bool>(), any::<bool>(), 0..8usize), 1..5)
        }

        fn remove_all(
            remaining: &mut Vec<(Tag, Vec<u8>)>,
            accepted: Vec<(Tag, Vec<u8>)>,
        ) -> Result<(), TestCaseError> {
            for line in accepted {
                let pos = remaining.iter().position(|x| *x == line);
                prop_assert!(pos.is_some(), "accepted line {:?} was not pending", line);
                remaining.remove(pos.unwrap());
            }
            Ok(())
        }

        #[test]
        fn ops_interleaved_hunk_and_file_accepts_never_show_an_accepted_hunk_and_every_unaccepted_hunk_survives()
         {
            let d = RefCell::new(Draft::new("lc-prop-hunks"));
            let mut runner = TestRunner::new(config());
            let names = ["a", "b"];
            let result = runner.run(
                &(hunky_file("a"), hunky_file("b"), ops_seq()),
                |(a, b, ops)| {
                    let mut d = d.borrow_mut();
                    let pair = [a, b];
                    let seed: BTreeMap<String, Vec<u8>> = names
                        .iter()
                        .zip(&pair)
                        .map(|(n, (base, _))| ((*n).to_owned(), base.clone()))
                        .collect();
                    d.reset(&seed);
                    for (n, (_, cur)) in names.iter().zip(&pair) {
                        d.write(n, cur);
                    }
                    // The oracle: per file, the change lines of the initial diff, minus the
                    // accepted ones.
                    let mut remaining: Vec<Vec<(Tag, Vec<u8>)>> = pair
                        .iter()
                        .map(|(base, cur)| sorted_changes(&hunks::diff(base, cur)))
                        .collect();
                    // One scan per op: the pile checked after an accept is the one the
                    // next op renders from.
                    let mut pile = d.scan();
                    for (file_b, whole_file, pick) in ops {
                        let f = usize::from(file_b);
                        let Some(row) = pile.row(names[f].as_bytes()).cloned() else {
                            prop_assert!(
                                remaining[f].is_empty(),
                                "{}: no row, {} change lines left",
                                names[f],
                                remaining[f].len()
                            );
                            continue;
                        };
                        let rendered = Rendered::of(&row);
                        let out = if whole_file || row.hunks.is_empty() {
                            remaining[f].clear();
                            d.ops().accept_file(&rendered, &NoFault).unwrap()
                        } else {
                            let i = pick % row.hunks.len();
                            remove_all(&mut remaining[f], row.hunks[i].change_lines())?;
                            d.ops()
                                .accept_hunk(&rendered, &row.hunks, i, &NoFault)
                                .unwrap()
                        };
                        prop_assert!(out.ok(), "{:?}", out);
                        pile = d.scan();
                        for (g, name) in names.iter().enumerate() {
                            let shown = pile
                                .row(name.as_bytes())
                                .map(|r| sorted_changes(&r.hunks))
                                .unwrap_or_default();
                            prop_assert_eq!(
                                &shown,
                                &remaining[g],
                                "{}: shown change lines are not the unaccepted ones",
                                name
                            );
                        }
                    }
                    Ok(())
                },
            );
            if let Err(e) = result {
                panic!("{e}");
            }
        }

        /// Arbitrary editor-buffer bytes: CRLF, a lone CR, no trailing newline, tabs, NUL
        /// (a NUL-bearing buffer is *binary* to git and must still pend nothing).
        fn save_bytes() -> impl Strategy<Value = Vec<u8>> {
            let piece = prop_oneof![
                Just(&b"a\n"[..]),
                Just(&b"b\r\n"[..]),
                Just(&b"\r"[..]),
                Just(&b"\n"[..]),
                Just(&b"\tt\n"[..]),
                Just(&b"tail"[..]),
                Just(&b"\0\n"[..]),
                Just(&b"\xc3\xa9\r\n"[..]),
            ];
            prop::collection::vec(piece, 0..8).prop_map(|v| v.concat())
        }

        /// Gate item 1 as a property (64 cases in prepush): whatever the buffer holds, a
        /// save puts exactly those bytes on disk and the rescan that follows pends nothing.
        #[test]
        fn ops_save_then_scan_pends_nothing_for_any_bytes() {
            let d = RefCell::new(Draft::new("lc-prop-save"));
            let mut runner = TestRunner::new(config());
            let result = runner.run(&(content(), save_bytes()), |(seed, bytes)| {
                let mut d = d.borrow_mut();
                let mut files = BTreeMap::new();
                files.insert("n".to_owned(), seed.clone());
                d.reset(&files);
                // Something has to be pending for there to be a row to save into.
                let mut edited = seed.clone();
                edited.extend_from_slice(b"pending\n");
                d.write("n", &edited);
                let pile = d.scan();
                let row = pile.row(b"n").expect("the edit is pending").clone();
                let rendered = Rendered::of(&row);
                let out = d.ops().save_file(&rendered, &bytes, &NoFault).unwrap();
                prop_assert!(out.ok(), "{:?}", out);
                prop_assert_eq!(
                    std::fs::read(d.root.join("n")).unwrap(),
                    bytes.clone(),
                    "the buffer's bytes land verbatim"
                );
                let after = d.scan();
                prop_assert!(
                    after.row(b"n").is_none(),
                    "the saved row is still pending: {:?}",
                    crate::scan::pile_lines(&after)
                );
                Ok(())
            });
            if let Err(e) = result {
                panic!("{e}");
            }
        }
    }
}
