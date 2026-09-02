//! HEAD, branch, in-progress state and transition notices (docs/spec/00-spec.md §6.4
//! "HEAD tracking", "In-progress operations"; kickoff deliverable 8).
//!
//! `rev-parse HEAD` and `symbolic-ref -q --short HEAD` are the truth. The last line of
//! `<git-dir>/logs/HEAD` is a **hint for the notice text only**: a wrong notice is a
//! cosmetic bug; a notice never changes a baseline or a row.

use std::path::{Path, PathBuf};

use crate::git::{GitError, Oid, RepoGit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgress {
    pub fn as_str(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
        }
    }
}

/// One inspection of a root's HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    /// `None` before the first commit.
    pub head: Option<Oid>,
    /// Short branch name; `None` when detached or unborn-and-unnamed.
    pub branch: Option<String>,
    pub detached: bool,
    pub in_progress: Option<InProgress>,
    pub merge_head: Option<Oid>,
    pub shallow: bool,
    /// The per-worktree git dir (`rev-parse --git-dir`, absolute).
    pub git_dir: PathBuf,
    /// The common dir (`rev-parse --git-common-dir`, absolute); equals `git_dir` unless
    /// this is a linked worktree.
    pub common_dir: PathBuf,
}

impl HeadState {
    /// An empty state for draft roots and for "before the first inspection".
    pub fn none() -> Self {
        Self {
            head: None,
            branch: None,
            detached: false,
            in_progress: None,
            merge_head: None,
            shallow: false,
            git_dir: PathBuf::new(),
            common_dir: PathBuf::new(),
        }
    }

    pub fn is_linked_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }
}

/// Inspect the root now.
pub fn inspect(rg: &RepoGit) -> Result<HeadState, GitError> {
    let head = rg.rev_parse_verify("HEAD")?;
    let branch = {
        let out = rg.run_raw(&["symbolic-ref", "-q", "--short", "HEAD"], None)?;
        if out.success() {
            Some(out.stdout_trimmed()).filter(|s| !s.is_empty())
        } else {
            None
        }
    };
    let detached = head.is_some() && branch.is_none();
    let git_dir = absolute_dir(rg, "--git-dir")?;
    let common_dir = absolute_dir(rg, "--git-common-dir")?;
    let merge_head = rg.rev_parse_verify("MERGE_HEAD")?;
    let in_progress = in_progress_of(&git_dir);
    let shallow = {
        let out = rg.run_raw(&["rev-parse", "--is-shallow-repository"], None)?;
        out.success() && out.stdout_trimmed() == "true"
    };
    Ok(HeadState {
        head,
        branch,
        detached,
        in_progress,
        merge_head,
        shallow,
        git_dir,
        common_dir,
    })
}

fn absolute_dir(rg: &RepoGit, flag: &str) -> Result<PathBuf, GitError> {
    let out = rg.run(&["rev-parse", "--path-format=absolute", flag])?;
    let s = String::from_utf8_lossy(&out).trim().to_owned();
    Ok(std::fs::canonicalize(&s).unwrap_or_else(|_| PathBuf::from(s)))
}

/// Which operation is in progress, from the per-worktree git dir's marker files.
pub fn in_progress_of(git_dir: &Path) -> Option<InProgress> {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        Some(InProgress::Rebase)
    } else if git_dir.join("MERGE_HEAD").is_file() {
        Some(InProgress::Merge)
    } else if git_dir.join("CHERRY_PICK_HEAD").is_file() {
        Some(InProgress::CherryPick)
    } else if git_dir.join("REVERT_HEAD").is_file() {
        Some(InProgress::Revert)
    } else {
        None
    }
}

/// The reflog hint: the last `logs/HEAD` message, plus (for a rebase) what it was onto.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReflogHint {
    pub message: String,
    pub onto: Option<String>,
}

/// Read the hint from `<git_dir>/logs/HEAD`. `None` when there is no reflog.
pub fn last_reflog(git_dir: &Path) -> Option<ReflogHint> {
    let text = std::fs::read_to_string(git_dir.join("logs").join("HEAD")).ok()?;
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let last = lines.pop()?;
    let message = reflog_message(last)?.to_owned();
    let onto = if message.starts_with("rebase") {
        lines.iter().rev().find_map(|l| {
            reflog_message(l)
                .and_then(|m| m.strip_prefix("rebase (start): checkout "))
                .or_else(|| {
                    reflog_message(l).and_then(|m| m.strip_prefix("rebase -i (start): checkout "))
                })
                .map(str::to_owned)
        })
    } else {
        None
    };
    Some(ReflogHint { message, onto })
}

/// The message part of one reflog line (`<old> <new> <ident> <ts> <tz>\t<message>`).
pub fn reflog_message(line: &str) -> Option<&str> {
    line.split_once('\t').map(|(_, m)| m.trim())
}

/// What a HEAD transition was, classified from the reflog hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    Commit,
    Checkout,
    Rebase,
    Reset { target: String },
    Merge { message: String },
    Other { message: String },
}

/// Classify a reflog message by prefix.
pub fn classify(message: &str) -> Transition {
    let m = message.trim();
    if m == "commit"
        || m.starts_with("commit ")
        || m.starts_with("commit:")
        || m.starts_with("commit (")
    {
        Transition::Commit
    } else if m.starts_with("checkout:") {
        Transition::Checkout
    } else if m.starts_with("rebase") && (m.contains("(finish)") || m.contains("finished")) {
        // "rebase (finish)", "rebase -i (finish)", "rebase (continue) (finish)",
        // "rebase finished" (older git).
        Transition::Rebase
    } else if let Some(t) = m.strip_prefix("reset: moving to ") {
        Transition::Reset {
            target: t.trim().to_owned(),
        }
    } else if m.starts_with("pull") || m.starts_with("merge") {
        Transition::Merge {
            message: m.to_owned(),
        }
    } else {
        Transition::Other {
            message: m.to_owned(),
        }
    }
}

/// Inputs the notice text needs beyond the two states.
#[derive(Debug, Clone, Default)]
pub struct TransitionFacts {
    /// `rev-list --count prev..next`, when computable.
    pub commits: Option<u64>,
    /// Pending rows after the scan that followed the switch.
    pub files_differ: usize,
}

fn label(state: &HeadState) -> String {
    match (&state.branch, &state.head) {
        (Some(b), _) => b.clone(),
        (None, Some(h)) => short(h),
        (None, None) => "unborn".to_owned(),
    }
}

fn short(oid: &Oid) -> String {
    oid.as_str().chars().take(7).collect()
}

/// The notice for `prev → next`, or `None` when nothing user-visible happened or an
/// operation is still in progress (notices are emitted once when it clears).
pub fn transition(
    prev: &HeadState,
    next: &HeadState,
    hint: Option<&ReflogHint>,
    facts: &TransitionFacts,
) -> Option<String> {
    if next.in_progress.is_some() {
        return None;
    }
    let cleared = prev.in_progress.is_some();
    let moved = prev.head != next.head || prev.branch != next.branch;
    if !moved && !cleared {
        return None;
    }
    let message = hint.map(|h| h.message.as_str()).unwrap_or("");
    let text = match classify(message) {
        Transition::Commit if prev.head != next.head => {
            let n = facts.commits.unwrap_or(1).max(1);
            let plural = if n == 1 { "commit" } else { "commits" };
            format!("committed on {} ({n} {plural})", label(next))
        }
        Transition::Commit => format!("switched {} → {} (same commit)", label(prev), label(next)),
        Transition::Checkout => {
            if prev.head == next.head {
                format!("switched {} → {} (same commit)", label(prev), label(next))
            } else {
                format!(
                    "switched {} → {}: {} files differ from seen state",
                    label(prev),
                    label(next),
                    facts.files_differ
                )
            }
        }
        Transition::Rebase => match hint.and_then(|h| h.onto.clone()) {
            Some(onto) => format!("rebased {} onto {onto}", label(next)),
            None => format!("rebased {}", label(next)),
        },
        Transition::Reset { target } => format!("reset: moving to {target}"),
        Transition::Merge { message } => {
            let rest = message
                .strip_prefix("pull:")
                .or_else(|| message.strip_prefix("pull "))
                .or_else(|| message.strip_prefix("merge "))
                .or_else(|| message.strip_prefix("merge"))
                .unwrap_or(&message)
                .trim();
            if rest.is_empty() {
                "merged".to_owned()
            } else {
                format!("merged {rest}")
            }
        }
        Transition::Other { message } if !message.is_empty() => {
            format!("HEAD moved: {message}")
        }
        Transition::Other { .. } => match (&prev.head, &next.head) {
            (Some(a), Some(b)) if a != b => format!("HEAD moved: {} → {}", short(a), short(b)),
            _ => format!("switched {} → {}", label(prev), label(next)),
        },
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(head: &str, branch: Option<&str>, in_progress: Option<InProgress>) -> HeadState {
        HeadState {
            head: Oid::parse(&head.repeat(40)),
            branch: branch.map(str::to_owned),
            detached: branch.is_none(),
            in_progress,
            merge_head: None,
            shallow: false,
            git_dir: PathBuf::from("/x/.git"),
            common_dir: PathBuf::from("/x/.git"),
        }
    }

    fn hint(m: &str) -> ReflogHint {
        ReflogHint {
            message: m.to_owned(),
            onto: None,
        }
    }

    #[test]
    fn headstate_classifies_literal_reflog_messages() {
        assert_eq!(classify("commit: agent commit"), Transition::Commit);
        assert_eq!(classify("commit (amend): x"), Transition::Commit);
        assert_eq!(classify("commit (merge): x"), Transition::Commit);
        assert_eq!(classify("commit (initial): x"), Transition::Commit);
        assert_eq!(
            classify("checkout: moving from main to feat-x"),
            Transition::Checkout
        );
        assert_eq!(
            classify("rebase (finish): returning to refs/heads/feat-x"),
            Transition::Rebase
        );
        assert_eq!(
            classify("rebase -i (finish): returning to refs/heads/x"),
            Transition::Rebase
        );
        assert_eq!(
            classify("reset: moving to HEAD~1"),
            Transition::Reset {
                target: "HEAD~1".into()
            }
        );
        assert!(matches!(
            classify("pull: Fast-forward"),
            Transition::Merge { .. }
        ));
        assert!(matches!(
            classify("merge origin/main: Merge made by the 'ort' strategy."),
            Transition::Merge { .. }
        ));
        assert!(matches!(
            classify("cherry-pick: x"),
            Transition::Other { .. }
        ));
    }

    #[test]
    fn headstate_transition_texts() {
        let a = state("a", Some("main"), None);
        let b = state("b", Some("main"), None);
        let facts = TransitionFacts {
            commits: Some(1),
            files_differ: 0,
        };
        assert_eq!(
            transition(&a, &b, Some(&hint("commit: agent commit")), &facts),
            Some("committed on main (1 commit)".into())
        );
        let three = TransitionFacts {
            commits: Some(3),
            files_differ: 0,
        };
        assert_eq!(
            transition(&a, &b, Some(&hint("commit: x")), &three).unwrap(),
            "committed on main (3 commits)"
        );
        let feat = state("a", Some("feat-x"), None);
        assert_eq!(
            transition(
                &a,
                &feat,
                Some(&hint("checkout: moving from main to feat-x")),
                &facts
            )
            .unwrap(),
            "switched main → feat-x (same commit)"
        );
        let feat_y = state("c", Some("feat-y"), None);
        let differ = TransitionFacts {
            commits: None,
            files_differ: 2,
        };
        assert_eq!(
            transition(
                &a,
                &feat_y,
                Some(&hint("checkout: moving from main to feat-y")),
                &differ
            )
            .unwrap(),
            "switched main → feat-y: 2 files differ from seen state"
        );
        let rebased = ReflogHint {
            message: "rebase (finish): returning to refs/heads/feat-x".into(),
            onto: Some("origin/main".into()),
        };
        let fx2 = state("d", Some("feat-x"), None);
        assert_eq!(
            transition(&feat, &fx2, Some(&rebased), &facts).unwrap(),
            "rebased feat-x onto origin/main"
        );
        assert_eq!(
            transition(&b, &a, Some(&hint("reset: moving to T~1")), &facts).unwrap(),
            "reset: moving to T~1"
        );
        assert_eq!(
            transition(&a, &b, Some(&hint("pull: Fast-forward")), &facts).unwrap(),
            "merged Fast-forward"
        );
        assert_eq!(
            transition(&a, &b, Some(&hint("weird: thing")), &facts).unwrap(),
            "HEAD moved: weird: thing"
        );
        assert_eq!(
            transition(&a, &b, None, &facts).unwrap(),
            "HEAD moved: aaaaaaa → bbbbbbb"
        );
        assert_eq!(
            transition(&a, &a, Some(&hint("commit: x")), &facts),
            None,
            "nothing moved"
        );
    }

    #[test]
    fn headstate_notices_suppressed_while_in_progress_and_emitted_when_cleared() {
        let a = state("a", Some("main"), None);
        let merging = state("a", Some("main"), Some(InProgress::Merge));
        let done = state("b", Some("main"), None);
        let facts = TransitionFacts::default();
        assert_eq!(transition(&a, &merging, Some(&hint("x")), &facts), None);
        assert_eq!(
            transition(&merging, &merging, Some(&hint("x")), &facts),
            None
        );
        assert!(
            transition(&merging, &done, Some(&hint("commit (merge): m")), &facts)
                .unwrap()
                .starts_with("committed on main")
        );
        // Cleared without HEAD moving (an aborted merge): still one notice.
        assert!(transition(&merging, &a, Some(&hint("reset: moving to HEAD")), &facts).is_some());
    }

    #[test]
    fn headstate_reflog_parsing_and_in_progress_markers() {
        let dir = lastcall_testkit::tmp::TempDir::new("lc-head");
        let git_dir = dir.mkdir("g");
        assert_eq!(last_reflog(&git_dir), None);
        dir.write(
            "g/logs/HEAD",
            "0000 1111 A <a@b> 1 +0000\tcommit (initial): c1\n\
             1111 2222 A <a@b> 2 +0000\trebase (start): checkout origin/main\n\
             2222 3333 A <a@b> 3 +0000\trebase (pick): x\n\
             3333 3333 A <a@b> 4 +0000\trebase (finish): returning to refs/heads/feat-x\n",
        );
        let h = last_reflog(&git_dir).unwrap();
        assert_eq!(h.message, "rebase (finish): returning to refs/heads/feat-x");
        assert_eq!(h.onto.as_deref(), Some("origin/main"));
        assert_eq!(in_progress_of(&git_dir), None);
        dir.write("g/MERGE_HEAD", "x\n");
        assert_eq!(in_progress_of(&git_dir), Some(InProgress::Merge));
        dir.mkdir("g/rebase-merge");
        assert_eq!(
            in_progress_of(&git_dir),
            Some(InProgress::Rebase),
            "rebase wins"
        );
    }

    #[test]
    fn headstate_inspect_reads_a_real_repo() {
        use crate::store::tests::fixture_env;
        use lastcall_testkit::fixture_repo::FixtureRepo;
        use lastcall_testkit::tmp::TempDir;
        let repo = FixtureRepo::new("head").unwrap();
        let state_dir = TempDir::new("lc-head");
        let env = fixture_env(&repo, &state_dir);
        let rg = RepoGit::new(&env, repo.path());
        let s = inspect(&rg).unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.head, Oid::parse(repo.head().unwrap().trim()));
        assert!(!s.detached && !s.shallow && s.in_progress.is_none());
        assert!(!s.is_linked_worktree());
        assert!(s.git_dir.join("HEAD").is_file());
        repo.checkout("--detach").unwrap();
        let d = inspect(&rg).unwrap();
        assert!(d.detached && d.branch.is_none());
        assert_eq!(
            last_reflog(&d.git_dir).unwrap().message.split(':').next(),
            Some("checkout")
        );
    }
}
