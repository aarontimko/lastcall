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
/// `(HEAD commit, short branch)` right now; `(None, None)` before the first commit or
/// when detached (branch only). Accept-all stamps `seen_at` with it — an annotation
/// input, never a baseline.
pub fn current_head(rg: &RepoGit) -> (Option<Oid>, Option<String>) {
    let head = rg.rev_parse_verify("HEAD").ok().flatten();
    let branch = rg
        .run(&["symbolic-ref", "-q", "--short", "HEAD"])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .filter(|s| !s.is_empty());
    (head, branch)
}

/// The branch `<git_dir>/HEAD` names, or `None` when it is detached, unborn-and-unnamed,
/// missing, locked or mid-write (Amendment v1.12, R1).
///
/// One file read and **no git process**: this runs on every scan of every root, and the
/// answer decides which seen record is in force. `checkout` and `switch` write `HEAD`
/// through a lockfile and a rename, so a torn read never happens; a missing file or a
/// `HEAD.lock` can, and both answer `None`, which means "no switch" everywhere upstream.
pub fn head_branch(git_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(git_dir.join("HEAD")).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let name = text.trim().strip_prefix("ref: refs/heads/")?;
    (!name.is_empty()).then(|| name.to_owned())
}

pub fn inspect(rg: &RepoGit) -> Result<HeadState, GitError> {
    Ok(inspect_with_paths(rg, &[])?.0)
}

/// [`inspect`] plus `rev-parse --git-path <name>` for each of `git_paths`, folded into the
/// same batched call — the shape the engine's open uses so that one root costs **four** git
/// spawns however many git paths it wants (Phase 5 deliverable 1c; before: six for the
/// inspection alone, plus one per git path).
///
/// The batch holds only flags that cannot fail (`rev-parse` exits non-zero as a whole if
/// any one flag does). `symbolic-ref -q --short HEAD` and the two `rev-parse -q --verify`
/// calls therefore stay separate: their non-zero exit *is* the answer (an unborn HEAD, no
/// merge in progress).
pub fn inspect_with_paths(
    rg: &RepoGit,
    git_paths: &[&str],
) -> Result<(HeadState, Vec<PathBuf>), GitError> {
    // The branch first, and the commit *through* the branch it names. `git checkout`
    // updates the working tree and the index before it moves HEAD, so an inspection that
    // a worktree event started can easily still be running when HEAD flips: reading the
    // commit first and the branch second pairs the commit one branch had with the name of
    // the other, and the notice then reads `switched main → main`. Resolving the name's
    // own ref cannot mix two branches: the pair is either wholly before the checkout or
    // wholly after it, and the event for the other one follows. An unborn branch has a
    // name and no commit (verify fails), and a detached HEAD has no name, both as before.
    let branch = {
        let out = rg.run_raw(&["symbolic-ref", "-q", "--short", "HEAD"], None)?;
        if out.success() {
            Some(out.stdout_trimmed()).filter(|s| !s.is_empty())
        } else {
            None
        }
    };
    let head = match &branch {
        Some(name) => rg.rev_parse_verify(&format!("refs/heads/{name}"))?,
        None => rg.rev_parse_verify("HEAD")?,
    };
    let detached = head.is_some() && branch.is_none();

    let mut flags: Vec<&str> = vec!["--git-dir", "--git-common-dir", "--is-shallow-repository"];
    for name in git_paths {
        flags.push("--git-path");
        flags.push(name);
    }
    let lines = rg.rev_parse_batch(&flags, 3 + git_paths.len())?;
    // Outputs pair by position: three fixed lines, then one per `--git-path <name>` pair.
    let git_dir = canonical(&lines[0]);
    let common_dir = canonical(&lines[1]);
    let shallow = lines[2] == "true";
    let paths: Vec<PathBuf> = lines[3..].iter().map(PathBuf::from).collect();

    let merge_head = rg.rev_parse_verify("MERGE_HEAD")?;
    let in_progress = in_progress_of(&git_dir);
    Ok((
        HeadState {
            head,
            branch,
            detached,
            in_progress,
            merge_head,
            shallow,
            git_dir,
            common_dir,
        },
        paths,
    ))
}

fn canonical(s: &str) -> PathBuf {
    std::fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s))
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
    /// The branch left, when the arrival was the new branch's **first sight** (Amendment
    /// v1.12, R8): its record had to be made, as a copy of that branch's. `None` for every
    /// other transition, including a return to a branch whose record was parked.
    pub first_sight_from: Option<String>,
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
            // A move that changed no commit says so and stops there, first sight or not
            // (B2, B3, D15): there is nothing for the user to have missed, and the copy
            // had nothing to fold.
            if prev.head == next.head {
                format!("switched {} → {} (same commit)", label(prev), label(next))
            } else if let Some(from) = &facts.first_sight_from {
                let n = facts.files_differ;
                let plural = if n == 1 { "file" } else { "files" };
                format!(
                    "switched {} → {}: first time here, seen state carried from {from}; {n} {plural} pending",
                    label(prev),
                    label(next),
                )
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

    /// R8's first-sight form, the count in the singular. The branch left is named because
    /// the state the user is looking at is that branch's, carried over.
    #[test]
    fn headstate_transition_first_sight_with_files_pending() {
        let a = state("a", Some("main"), None);
        let b = state("b", Some("feat/other"), None);
        let facts = TransitionFacts {
            commits: None,
            files_differ: 1,
            first_sight_from: Some("main".into()),
        };
        assert_eq!(
            transition(
                &a,
                &b,
                Some(&hint("checkout: moving from main to feat/other")),
                &facts
            )
            .unwrap(),
            "switched main → feat/other: first time here, seen state carried from main; 1 file pending"
        );
        let two = TransitionFacts {
            files_differ: 2,
            ..facts
        };
        assert_eq!(
            transition(
                &a,
                &b,
                Some(&hint("checkout: moving from main to feat/other")),
                &two
            )
            .unwrap(),
            "switched main → feat/other: first time here, seen state carried from main; 2 files pending"
        );
    }

    /// D14's return leg: a first sight that folded everything away still says so, with a
    /// zero — the wording is about where the state came from, not about the count. And the
    /// same-commit case (B2, D15, D23) keeps today's text whether it is a first sight or
    /// not, which is the one place the form is decided by the heads and not by the fact.
    #[test]
    fn headstate_transition_first_sight_with_nothing_pending() {
        let a = state("a", Some("future"), None);
        let b = state("b", Some("main"), None);
        let facts = TransitionFacts {
            commits: None,
            files_differ: 0,
            first_sight_from: Some("future".into()),
        };
        assert_eq!(
            transition(
                &a,
                &b,
                Some(&hint("checkout: moving from future to main")),
                &facts
            )
            .unwrap(),
            "switched future → main: first time here, seen state carried from future; 0 files pending"
        );
        let same = state("a", Some("feat/w"), None);
        assert_eq!(
            transition(
                &a,
                &same,
                Some(&hint("checkout: moving from future to feat/w")),
                &facts
            )
            .unwrap(),
            "switched future → feat/w (same commit)"
        );
    }

    #[test]
    fn headstate_transition_texts() {
        let a = state("a", Some("main"), None);
        let b = state("b", Some("main"), None);
        let facts = TransitionFacts {
            commits: Some(1),
            files_differ: 0,
            first_sight_from: None,
        };
        assert_eq!(
            transition(&a, &b, Some(&hint("commit: agent commit")), &facts),
            Some("committed on main (1 commit)".into())
        );
        let three = TransitionFacts {
            commits: Some(3),
            files_differ: 0,
            first_sight_from: None,
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
            first_sight_from: None,
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

    /// The commit is read through the branch's own ref, so the pair an inspection reports
    /// is always one real state and never one branch's name beside another's commit. The
    /// two states that have no such ref keep their answers: an unborn branch has a name
    /// and no commit, a detached HEAD a commit and no name.
    #[test]
    fn headstate_inspect_pairs_the_branch_with_its_own_ref() {
        use crate::store::tests::fixture_env;
        use lastcall_testkit::fixture_repo::FixtureRepo;
        use lastcall_testkit::tmp::TempDir;
        let mut repo = FixtureRepo::new("pair").unwrap();
        let state_dir = TempDir::new("lc-pair");
        let env = fixture_env(&repo, &state_dir);
        let rg = RepoGit::new(&env, repo.path());

        let on_main = inspect(&rg).unwrap();
        assert_eq!(on_main.branch.as_deref(), Some("main"));
        assert_eq!(
            on_main.head,
            rg.rev_parse_verify("refs/heads/main").unwrap(),
            "the commit is the branch's own tip"
        );

        // A second branch at a different commit: the name and the commit move together.
        repo.checkout_b("other").unwrap();
        repo.commit_files(&[("only-here.txt", "one\n")], "on other")
            .unwrap();
        let on_other = inspect(&rg).unwrap();
        assert_eq!(on_other.branch.as_deref(), Some("other"));
        assert_eq!(
            on_other.head,
            rg.rev_parse_verify("refs/heads/other").unwrap()
        );
        assert_ne!(on_other.head, on_main.head);

        // An unborn branch: a name, no commit, not detached.
        repo.git(&["checkout", "-q", "--orphan", "fresh"]).unwrap();
        repo.git(&["rm", "-rqf", "--cached", "."]).unwrap();
        let unborn = inspect(&rg).unwrap();
        assert_eq!(unborn.branch.as_deref(), Some("fresh"));
        assert!(unborn.head.is_none() && !unborn.detached);
    }

    /// The batched `rev-parse` must answer exactly what the six separate calls answered,
    /// including in a linked worktree (where `--git-dir` and `--git-common-dir` differ) and
    /// on a detached HEAD, and it must cost four spawns, not six.
    #[test]
    fn headstate_batched_rev_parse_matches_the_separate_calls() {
        use crate::store::tests::fixture_env;
        use lastcall_testkit::fixture_repo::FixtureRepo;
        use lastcall_testkit::tmp::TempDir;
        let repo = FixtureRepo::new("batch").unwrap();
        let state_dir = TempDir::new("lc-batch");
        let env = fixture_env(&repo, &state_dir);
        let rg = RepoGit::new(&env, repo.path());

        const PATHS: [&str; 3] = ["objects", "info/attributes", "info/exclude"];
        let before = crate::git::thread_spawn_count();
        let (s, paths) = inspect_with_paths(&rg, &PATHS).unwrap();
        let spawns = crate::git::thread_spawn_count() - before;
        assert_eq!(
            spawns, 4,
            "HEAD verify, symbolic-ref, one batched rev-parse, MERGE_HEAD verify"
        );
        assert_eq!(paths.len(), PATHS.len(), "one line per --git-path");
        // Positional pairing: each line is the path of the flag at the same position.
        for (name, got) in PATHS.iter().zip(&paths) {
            assert_eq!(got, &rg.git_path(name).unwrap(), "--git-path {name}");
        }
        // The batched answers equal the ones `inspect` alone produces.
        let plain = inspect(&rg).unwrap();
        assert_eq!(
            (s.git_dir, s.common_dir, s.shallow),
            (
                plain.git_dir.clone(),
                plain.common_dir.clone(),
                plain.shallow
            )
        );
        assert!(!plain.is_linked_worktree());

        // A linked worktree: git_dir is under the common dir, and they must not collapse.
        let wt = repo.parent_dir().join("wt");
        repo.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"])
            .unwrap();
        let wrg = RepoGit::new(&env, &wt);
        let (w, wpaths) = inspect_with_paths(&wrg, &PATHS).unwrap();
        assert!(
            w.is_linked_worktree(),
            "{:?} vs {:?}",
            w.git_dir,
            w.common_dir
        );
        assert_ne!(w.git_dir, w.common_dir);
        assert_eq!(w.branch.as_deref(), Some("wtb"));
        // `objects` lives in the common dir; `info/exclude` is per-worktree only when the
        // worktree has one, so assert against git's own answer rather than a guess.
        for (name, got) in PATHS.iter().zip(&wpaths) {
            assert_eq!(
                got,
                &wrg.git_path(name).unwrap(),
                "linked --git-path {name}"
            );
        }

        // Detached: the batch still returns its three lines (only the ref lookups change).
        repo.checkout("--detach").unwrap();
        let (d, dpaths) = inspect_with_paths(&rg, &PATHS).unwrap();
        assert!(d.detached && d.branch.is_none());
        assert_eq!(dpaths, paths, "paths do not depend on HEAD");

        // A flag count that disagrees with the expected line count is a parse error, never
        // a silently shifted answer.
        let err = rg.rev_parse_batch(&["--git-dir", "--is-shallow-repository"], 3);
        assert!(
            matches!(err, Err(crate::git::GitError::Parse { .. })),
            "{err:?}"
        );
    }
}
