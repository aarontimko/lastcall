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
    pub badge: Option<BadgeStatus>,
    pub head: Option<Oid>,
    pub branch: Option<String>,
    pub in_progress: Option<InProgress>,
    pub seen_tree: Option<Oid>,
    pub seen_head: Option<Oid>,
    pub pending: Vec<RowStatus>,
    pub groups: Vec<GroupStatus>,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlagStatus {
    pub note: String,
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
    pub flag: Option<FlagStatus>,
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
            flag: row.flag.as_ref().map(|f| FlagStatus {
                note: f.note.clone(),
            }),
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
            badge: root.badge.as_ref().map(|b| match b {
                Badge::WorktreeOf(p) => BadgeStatus::WorktreeOf(path_string(p)),
                Badge::NestedIn(p) => BadgeStatus::NestedIn(path_string(p)),
            }),
            head: root.head.head.clone(),
            branch: root.head.branch.clone(),
            in_progress: root.head.in_progress,
            seen_tree: root.ledger.seen_tree.clone(),
            seen_head: root.ledger.seen_at.head_commit.clone(),
            pending,
            groups,
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

impl StatusReport {
    /// Scan every root (or only `only`, when given) and build the report.
    pub fn build(engine: &mut Engine, only: Option<&[PathBuf]>) -> StatusReport {
        let results = engine.scan_all();
        let wanted: Option<Vec<PathBuf>> = only.map(|paths| {
            paths
                .iter()
                .filter_map(|p| engine.resolve_root(p))
                .collect()
        });
        let mut roots = Vec::new();
        for (path, result) in results {
            if let Some(w) = &wanted
                && !w.contains(&path)
            {
                continue;
            }
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
        StatusReport {
            status_version: STATUS_VERSION,
            notices: engine.notices().to_vec(),
            roots,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// The human rendering: one header per root, one line per row, group lines last.
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        for n in &self.notices {
            out.push_str(&format!("notice: {n}\n"));
        }
        for (i, root) in self.roots.iter().enumerate() {
            if i > 0 || !self.notices.is_empty() {
                out.push('\n');
            }
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
            out.push_str(&format!(
                "{} ({branch}){badge}{in_progress}  {} pending\n",
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
