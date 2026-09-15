//! The `lastcall status` report (kickoff deliverable 12): a stable, timestamp-free JSON
//! document (`status_version: 1`) plus the human rendering. Roots are sorted by path,
//! rows by path bytes, so two runs over the same state are byte-identical.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::engine::{Engine, RootState};
use crate::git::Oid;
use crate::headstate::InProgress;
use crate::roots::Badge;
use crate::scan::{Annotation, Change, Collapsed, Entry, Pile, Rename, Row};
use crate::store::RootKind;

pub const STATUS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusReport {
    pub status_version: u32,
    /// The state dir this run resolved (`LASTCALL_STATE_DIR` → `$XDG_STATE_HOME/lastcall`
    /// → `~/.local/state/lastcall`), so two runs can be told apart by the store they read.
    /// Additive in v1.9 (`status_version` stays 1).
    pub state_dir: String,
    pub notices: Vec<String>,
    pub roots: Vec<RootStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BadgeStatus {
    WorktreeOf(String),
    NestedIn(String),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootStatus {
    pub root: String,
    pub kind: RootKind,
    pub parent: String,
    /// The `<repo-hash>` directory name under
    /// `<state_dir>/roots/<parent-hash>/repos/` — this root's store, by name.
    /// Additive in v1.9 (`status_version` stays 1).
    pub store: String,
    /// `ledger.json`'s mtime as ISO-8601 UTC; `null` when no ledger has been written yet.
    /// Additive in v1.9 (`status_version` stays 1).
    pub ledger_written_at: Option<String>,
    pub badge: Option<BadgeStatus>,
    pub head: Option<Oid>,
    pub branch: Option<String>,
    /// `org/repo` of `remote.origin.url`; `null` for no remote or a local-path origin.
    /// Additive in v1.3 (`status_version` stays 1).
    pub remote: Option<String>,
    pub in_progress: Option<InProgress>,
    pub seen_tree: Option<Oid>,
    pub seen_head: Option<Oid>,
    /// The branch the seen record above belongs to; `null` for a draft root, a detached
    /// `HEAD`, or a ledger a 1.1 binary wrote that has not been attributed yet. Additive in
    /// v1.12 (`status_version` stays 1).
    pub seen_branch: Option<String>,
    /// The branches with a parked seen record, sorted; empty when this root has only ever
    /// been on one. Additive in v1.12 (`status_version` stays 1).
    pub parked_branches: Vec<String>,
    pub pending: Vec<RowStatus>,
    /// Changed paths the row cap left unscanned (`Pile::omitted`); `0` when nothing was
    /// cut. Additive in Phase 4 (`status_version` stays 1).
    pub omitted: usize,
    pub groups: Vec<GroupStatus>,
    /// This root's undo stack depth (`Pile::undo`); `0` when there is nothing to undo.
    /// Additive in v1.11 (`status_version` stays 1).
    pub undo: usize,
    /// When this root stops being snoozed; `null` when it is not snoozed, and `null` for a
    /// deadline that has already passed. Additive in v1.11 (`status_version` stays 1).
    pub snoozed_until: Option<String>,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlagStatus {
    pub note: String,
}

/// One flag in the additive `flags` array (Amendment v1.7). The hunk *text* stays out of
/// `status` — the export (`flags::export`) is its outlet.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlagEntryStatus {
    pub note: String,
    pub created_at: String,
    pub hunk: Option<FlagHunkStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlagHunkStatus {
    pub index: usize,
    pub header: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RenameStatus {
    From { from: String, similarity: u8 },
    To { to: String, similarity: u8 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RowStatus {
    pub path: String,
    pub change: Change,
    pub baseline: Option<Entry>,
    pub current: Option<Entry>,
    pub added: usize,
    pub deleted: usize,
    pub hunks: usize,
    pub annotation: Option<Annotation>,
    pub conflicted: bool,
    pub collapsed: Option<Collapsed>,
    /// The first flag, as in schema 1.0. Kept for compatibility; `flags` is the full list.
    pub flag: Option<FlagStatus>,
    /// Every flag on the row, oldest first. Additive in v1.7 (`status_version` stays 1).
    pub flags: Vec<FlagEntryStatus>,
    pub rename: Option<RenameStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GroupStatus {
    pub kind: Annotation,
    pub paths: Vec<String>,
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `ledger.json`'s mtime as ISO-8601 UTC — `None` when the file is absent (a root whose
/// first sight has not been written yet) or its metadata is unreadable.
fn ledger_written_at(ledger: &Path) -> Option<String> {
    let m = std::fs::metadata(ledger).ok()?;
    Some(crate::ledger::iso8601(m.modified().ok()?))
}

impl RowStatus {
    pub fn of(row: &Row) -> Self {
        Self {
            path: lossy(&row.path),
            change: row.change,
            baseline: row.baseline.clone(),
            current: row.current.clone(),
            added: row.added,
            deleted: row.deleted,
            hunks: row.hunks.len(),
            annotation: row.annotation,
            conflicted: row.conflicted,
            collapsed: row.collapsed,
            flag: row.flags.first().map(|f| FlagStatus {
                note: f.note.clone(),
            }),
            flags: row
                .flags
                .iter()
                .map(|f| FlagEntryStatus {
                    note: f.note.clone(),
                    created_at: f.created_at.clone(),
                    hunk: f.hunk.as_ref().map(|h| FlagHunkStatus {
                        index: h.index,
                        header: h.header.clone(),
                    }),
                })
                .collect(),
            rename: row.rename.as_ref().map(|r| match r {
                Rename::From { from, similarity } => RenameStatus::From {
                    from: lossy(from),
                    similarity: *similarity,
                },
                Rename::To { to, similarity } => RenameStatus::To {
                    to: lossy(to),
                    similarity: *similarity,
                },
            }),
        }
    }
}

impl RootStatus {
    /// Build from a root and its (already computed) pile; `scan_error` becomes a notice.
    pub fn of(root: &RootState, pile: Option<&Pile>, scan_error: Option<String>) -> Self {
        let mut notices = root.notices.clone();
        if let Some(p) = pile {
            notices.extend(p.notices.iter().cloned());
        }
        if let Some(e) = scan_error {
            notices.push(format!("scan failed: {e}"));
        }
        let pending = pile
            .map(|p| p.rows.iter().map(RowStatus::of).collect())
            .unwrap_or_default();
        let groups = pile
            .map(|p| {
                p.groups()
                    .into_iter()
                    .map(|g| GroupStatus {
                        kind: g.kind,
                        paths: g.paths.iter().map(|p| lossy(p)).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            root: path_string(&root.path),
            kind: root.kind,
            parent: path_string(&root.parent),
            store: root
                .paths
                .repo_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ledger_written_at: ledger_written_at(&root.paths.ledger),
            badge: root.badge.as_ref().map(|b| match b {
                Badge::WorktreeOf(p) => BadgeStatus::WorktreeOf(path_string(p)),
                Badge::NestedIn(p) => BadgeStatus::NestedIn(path_string(p)),
            }),
            head: root.head.head.clone(),
            branch: root.head.branch.clone(),
            remote: root.remote.clone(),
            in_progress: root.head.in_progress,
            seen_tree: root.ledger.seen_tree.clone(),
            seen_head: root.ledger.seen_at.head_commit.clone(),
            seen_branch: root.ledger.seen_branch.clone(),
            // `BTreeMap`, so the names come out sorted without a sort here.
            parked_branches: root.ledger.branches.keys().cloned().collect(),
            pending,
            omitted: pile.map(|p| p.omitted).unwrap_or(0),
            groups,
            // Both from the pile, which the engine stamped from the ledger with the
            // expiry already applied (Amendment v1.11).
            undo: pile.map(|p| p.undo).unwrap_or(0),
            snoozed_until: pile.and_then(|p| p.snoozed_until.clone()),
            notices,
        }
    }

    pub fn name(&self) -> String {
        Path::new(&self.root)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.clone())
    }
}

/// Why a report could not be built.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("--root {}: not a watched root", .0.display())]
    UnknownRoot(PathBuf),
}

impl StatusReport {
    /// Scan every root — or only the roots `only` resolves to (a path inside a root
    /// selects it), scanning nothing else — and build the report. A path that resolves to
    /// no root is an error, never an empty report.
    pub fn build(
        engine: &mut Engine,
        only: Option<&[PathBuf]>,
    ) -> Result<StatusReport, StatusError> {
        let results = match only {
            Some(paths) => {
                let mut selected: Vec<PathBuf> = Vec::new();
                for p in paths {
                    let root = engine
                        .resolve_root(p)
                        .ok_or_else(|| StatusError::UnknownRoot(p.clone()))?;
                    if !selected.contains(&root) {
                        selected.push(root);
                    }
                }
                selected
                    .into_iter()
                    .map(|r| {
                        let result = engine.scan(&r);
                        (r, engine.scan_seq(), result)
                    })
                    .collect()
            }
            None => engine.scan_all(),
        };
        let mut roots = Vec::new();
        for (path, _seq, result) in results {
            let Some(root) = engine.root(&path) else {
                continue;
            };
            let status = match &result {
                Ok(pile) => RootStatus::of(root, Some(pile), None),
                Err(e) => RootStatus::of(root, root.last_pile.as_ref(), Some(e.to_string())),
            };
            roots.push(status);
        }
        roots.sort_by(|a, b| a.root.as_bytes().cmp(b.root.as_bytes()));
        Ok(StatusReport {
            status_version: STATUS_VERSION,
            state_dir: path_string(engine.layout().state_dir()),
            notices: engine.notices().to_vec(),
            roots,
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// The human rendering: the state dir first (so two runs can be told apart by the
    /// store they read — Amendment v1.9), then one header per root, one line per row,
    /// group lines last.
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("state dir: {}\n", self.state_dir));
        for n in &self.notices {
            out.push_str(&format!("notice: {n}\n"));
        }
        for root in &self.roots {
            out.push('\n');
            let branch = root
                .branch
                .clone()
                .or_else(|| root.head.as_ref().map(short))
                .unwrap_or_else(|| match root.kind {
                    RootKind::Git => "no commits".to_owned(),
                    RootKind::Draft => "draft".to_owned(),
                });
            let badge = match &root.badge {
                Some(BadgeStatus::WorktreeOf(p)) => format!("  [worktree of {}]", basename(p)),
                Some(BadgeStatus::NestedIn(p)) => format!("  [nested in {}]", basename(p)),
                None => String::new(),
            };
            let in_progress = root
                .in_progress
                .map(|ip| format!("  [{} in progress]", ip.as_str()))
                .unwrap_or_default();
            // Amendment v1.11: the two facts a headless reader would otherwise have to
            // open the ledger for, appended in the order the JSON lists them.
            let undo = if root.undo > 0 {
                format!(" · {} undo", root.undo)
            } else {
                String::new()
            };
            let snoozed = root
                .snoozed_until
                .as_deref()
                .map(|u| format!(" · snoozed until {}", crate::ledger::iso8601_date(u)))
                .unwrap_or_default();
            out.push_str(&format!(
                "{} ({branch}){badge}{in_progress}  {} pending{undo}{snoozed}\n",
                root.name(),
                root.pending.len()
            ));
            for n in &root.notices {
                out.push_str(&format!("  notice: {n}\n"));
            }
            for row in &root.pending {
                out.push_str(&format!("  {}\n", row_line(row)));
            }
            for g in &root.groups {
                let kind = match g.kind {
                    Annotation::Upstream => "upstream",
                    Annotation::Mixed => "mixed",
                };
                out.push_str(&format!("  {kind} · {} files\n", g.paths.len()));
            }
        }
        out
    }
}

fn short(oid: &Oid) -> String {
    oid.as_str().chars().take(7).collect()
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_owned())
}

/// `M f1  +3 −1  [upstream]`
pub fn row_line(row: &RowStatus) -> String {
    let letter = match row.change {
        Change::Modified => 'M',
        Change::Added => 'A',
        Change::Deleted => 'D',
        Change::Mode => 'X',
        Change::Typechange => 'T',
        Change::Unreadable => '?',
    };
    let mut s = format!("{letter} {}", row.path);
    if row.conflicted {
        s.push_str("  [conflict]");
    }
    match row.collapsed {
        Some(Collapsed::Glob) => s.push_str("  (collapsed)"),
        Some(Collapsed::Binary) => s.push_str("  (binary)"),
        Some(Collapsed::Size) => s.push_str("  (large)"),
        None => s.push_str(&format!("  +{} −{}", row.added, row.deleted)),
    }
    match row.annotation {
        Some(Annotation::Upstream) => s.push_str("  [upstream]"),
        Some(Annotation::Mixed) => s.push_str("  [mixed]"),
        None => {}
    }
    match &row.rename {
        Some(RenameStatus::From { from, similarity }) => {
            s.push_str(&format!("  (renamed from {from}, {similarity}%)"));
        }
        Some(RenameStatus::To { to, similarity }) => {
            s.push_str(&format!("  (renamed to {to}, {similarity}%)"));
        }
        None => {}
    }
    if let Some(f) = &row.flag {
        s.push_str(&format!("  ⚑ {}", f.note));
    }
    s
}
