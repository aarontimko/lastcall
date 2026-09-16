//! The seen oracle (Phase 11, deliverable 7): one sentence, checked against every screen
//! the engine produces over random branch histories.
//!
//! > A path's current entry is treated as seen only if it was committed before lastcall
//! > first saw the repository, or it is the first-sight entry, or the user accepted exactly
//! > that entry.
//!
//! The test tracks that set (`Seen`) itself, from the operations it generated, never from
//! the engine's own state. Every operation runs against a real fixture repository through
//! the real engine, and every scan goes through `Engine::scan` (`Fresh::scan`), never
//! through `inspect_head` alone, so the branch sync a pile depends on is the one a scan
//! performs.
//!
//! **Property 1, no hide:** after every scan, every path absent from the pile has a current
//! entry in `Seen(path)`: the entry in the commit made before first sight, the first-sight
//! entry, or an entry accepted by an `AcceptAll` (the entry at accept time). `Seen` is
//! keyed by path alone, as the sentence is: an entry the user accepted is seen wherever it
//! turns up, and a screen that leaves it out is not hiding anything from them.
//!
//! **Property 2, a scan you did not make cannot manufacture work:** each generated sequence
//! runs twice, once with a scan after every git command and once with the scans only where
//! the sequence says (a run of git commands is one unobserved step; an accept has to see a
//! pile, so it scans in both runs). Every row of the unobserved run's final pile is a row of
//! the observed run's, and both runs end on the same branch.
//!
//! Property 2 is the *subset* of the equality deliverable 7 asks for, and the narrowing is
//! a finding, not a convenience: the equality is false in the other direction for reasons
//! the fold has nothing to do with, and it is false that way both before and after the
//! merge-base change. Two shapes are pinned below as tests of their own
//! ([`seen_oracle_a_detour_through_an_older_branch_over_shows_only_when_watched`],
//! [`seen_oracle_a_branch_visited_while_watching_keeps_its_own_record`]): when the watching
//! run passes through a branch, that branch gets a record of its own, and the next first
//! sight copies *that* record rather than the one the unwatched run still carries. The
//! watching run then over-shows, which §2 allows. The direction that must never happen is
//! the one D25 caught: a pile that grows because nobody was looking.
//!
//! **Discipline of the generator, on purpose:** a `Commit` is generated on a branch only
//! when that branch has no commit made while lastcall watched that is still unaccepted.
//! That keeps the §11 residual (an intermediate version behind a baseline accepted as one
//! hunk) outside the generator; lifting it needs the additive `before` blob §11 names.
//!
//! Case count: the unit tier's 8, `PROPTEST_CASES` when set (64 from the pre-push tier),
//! the same machinery as the store-backed proptests in `ops::tests::proptests`, read here
//! rather than through `crate::env` because this is an integration test and that reader is
//! crate-private. The sequence is four to nine operations rather than deliverable 7's six
//! to twelve, to hold the 64-case run inside the pre-push budget; the report says so.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::Fresh;
use lastcall_engine::scan::Pile;
use lastcall_testkit::fixture_repo::SEED_FILES;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// Four paths: two the seed commit already carries (so their first-sight entry is a blob)
/// and two it does not (so their first-sight entry is absence).
const PATHS: [&str; 4] = ["f1", "f2", "g1", "g2"];
/// Four blobs, short enough that a diff is one hunk.
const BLOBS: [&str; 4] = ["v0\n", "v1\n", "v2\n", "v3\n"];

/// One path's content: a blob from the alphabet, the seed's own, or absence.
type Cell = Option<&'static str>;
/// The four paths' content at one point in the history.
type Content = [Cell; 4];

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// The content of the four paths at the seed commit, which is the root's first sight.
fn seed_content() -> Content {
    let at = |p: &str| -> Cell { SEED_FILES.iter().find(|(n, _)| *n == p).map(|(_, c)| *c) };
    [at(PATHS[0]), at(PATHS[1]), at(PATHS[2]), at(PATHS[3])]
}

// ---------------------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------------------

/// Where a new branch is cut: at the current tip, at another branch's tip, or at the commit
/// before the current branch's last one.
#[derive(Debug, Clone, Copy)]
enum CutAt {
    Tip,
    Other(usize),
    Older,
}

#[derive(Debug, Clone)]
enum Op {
    /// One to three paths set to a blob or removed, committed on the current branch.
    Commit(Vec<(usize, Option<usize>)>),
    AcceptAll,
    /// A new branch plus the checkout of it.
    Cut(CutAt),
    Checkout(usize),
    DeleteBranch(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(
                (0..PATHS.len(), prop::option::of(0..BLOBS.len())),
                1..=3,
             ).prop_map(Op::Commit),
        3 => Just(Op::AcceptAll),
        3 => prop_oneof![
                Just(CutAt::Tip),
                (0..6usize).prop_map(CutAt::Other),
                Just(CutAt::Older),
             ].prop_map(Op::Cut),
        3 => (0..6usize).prop_map(Op::Checkout),
        1 => (0..6usize).prop_map(Op::DeleteBranch),
    ]
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 4..=9)
}

// ---------------------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------------------

/// One branch, as the model keeps it: the content at its creation point followed by the
/// content after each commit made on it while lastcall watched, and whether each of those
/// commits is still unaccepted.
#[derive(Debug, Clone)]
struct Branch {
    hist: Vec<Content>,
    dirty: Vec<bool>,
}

impl Branch {
    fn tip(&self) -> Content {
        *self.hist.last().expect("a branch always has its base")
    }
    /// The generator's discipline: no commit while one of this branch's own commits is
    /// still unaccepted.
    fn clean(&self) -> bool {
        self.dirty.iter().all(|d| !d)
    }
    /// The branch as it was one commit ago (`HEAD~1`), for a `Cut(Older)`.
    fn older(&self) -> Self {
        let n = self.hist.len() - 1;
        Self {
            hist: self.hist[..n].to_vec(),
            dirty: self.dirty[..n - 1].to_vec(),
        }
    }
}

/// The record in force plus the pile, as Property 2 compares two runs.
#[derive(Debug, PartialEq, Eq)]
struct Final {
    pile: Vec<String>,
    seen_branch: Option<String>,
    seen_tree: Option<String>,
    overrides: BTreeMap<String, (Option<String>, Option<String>)>,
}

struct World {
    s: Fresh,
    names: Vec<String>,
    branches: BTreeMap<String, Branch>,
    cur: String,
    next_id: usize,
    seen: BTreeMap<&'static str, BTreeSet<Option<String>>>,
    /// Every git command and scan, in order: the failure message is a script.
    log: Vec<String>,
}

impl World {
    fn new(observed: bool) -> Self {
        let s = Fresh::new("oracle");
        let seed = seed_content();
        let mut seen = BTreeMap::new();
        for (i, p) in PATHS.iter().enumerate() {
            // The first-sight entry, which is also the entry of the one commit made before
            // lastcall saw the root.
            let mut set = BTreeSet::new();
            set.insert(seed[i].map(str::to_owned));
            seen.insert(*p, set);
        }
        let mut branches = BTreeMap::new();
        branches.insert(
            "main".to_owned(),
            Branch {
                hist: vec![seed],
                dirty: Vec::new(),
            },
        );
        Self {
            s,
            names: vec!["main".to_owned()],
            branches,
            cur: "main".to_owned(),
            next_id: 0,
            seen,
            log: vec![format!(
                "# run: {}",
                if observed { "observed" } else { "unobserved" }
            )],
        }
    }

    fn br(&self) -> &Branch {
        &self.branches[&self.cur]
    }

    fn disk(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.s.repo.path().join(path)).ok()
    }

    /// A scan through the engine, with Property 1 checked on what it produced.
    fn scan(&mut self) -> Result<Pile, TestCaseError> {
        let pile = self.s.scan();
        self.log.push(format!(
            "scan on {} -> [{}]",
            self.cur,
            lastcall_testkit::engine::pile_string(&pile)
        ));
        for (i, path) in PATHS.iter().enumerate() {
            let disk = self.disk(path);
            let model = self.br().tip()[i].map(str::to_owned);
            prop_assert_eq!(
                &disk,
                &model,
                "the model and the worktree disagree about {}\n{}",
                path,
                self.log.join("\n")
            );
            if pile.row(path.as_bytes()).is_some() {
                continue;
            }
            prop_assert!(
                self.seen[path].contains(&disk),
                "HIDE: {} is {:?} on disk, no row shows it, and it is not in Seen({}) = {:?}\n{}",
                path,
                disk,
                path,
                self.seen[path],
                self.log.join("\n")
            );
        }
        Ok(pile)
    }

    fn git(&mut self, args: &[&str]) {
        self.log.push(format!("git {}", args.join(" ")));
        self.s.repo.git(args).expect("git");
    }

    fn step(&mut self, op: &Op, observed: bool) -> Result<(), TestCaseError> {
        match op {
            Op::Commit(edits) => {
                // The discipline: nothing this branch committed while lastcall watched is
                // still unaccepted.
                if !self.br().clean() {
                    return Ok(());
                }
                let was = self.br().tip();
                let mut now = was;
                for (pi, bi) in edits {
                    now[*pi] = bi.map(|i| BLOBS[i]);
                }
                if now == was {
                    return Ok(());
                }
                for (i, path) in PATHS.iter().enumerate() {
                    if now[i] == was[i] {
                        continue;
                    }
                    match now[i] {
                        Some(c) => {
                            self.s.repo.write(path, c);
                        }
                        None => self.s.repo.remove(path),
                    }
                }
                self.log.push(format!("commit on {} -> {now:?}", self.cur));
                self.s.repo.commit("agent").expect("commit");
                let cur = self.cur.clone();
                let b = self.branches.get_mut(&cur).expect("current branch");
                b.hist.push(now);
                b.dirty.push(true);
            }
            Op::AcceptAll => {
                let pile = self.scan()?;
                self.s.accept_all_snapshot(&pile);
                self.log.push("accept-all".to_owned());
                for row in &pile.rows {
                    let Ok(p) = std::str::from_utf8(&row.path) else {
                        continue;
                    };
                    let Some(known) = PATHS.iter().find(|n| **n == p) else {
                        continue;
                    };
                    let entry = self.disk(p);
                    self.seen.get_mut(*known).expect("path").insert(entry);
                }
                let cur = self.cur.clone();
                let b = self.branches.get_mut(&cur).expect("current branch");
                b.dirty.fill(false);
            }
            Op::Cut(at) => {
                let (target, model) = match at {
                    CutAt::Tip => ("HEAD".to_owned(), self.br().clone()),
                    CutAt::Other(i) => {
                        let n = self.names[i % self.names.len()].clone();
                        (n.clone(), self.branches[&n].clone())
                    }
                    CutAt::Older => {
                        if self.br().dirty.is_empty() {
                            ("HEAD".to_owned(), self.br().clone())
                        } else {
                            ("HEAD~1".to_owned(), self.br().older())
                        }
                    }
                };
                let name = format!("b{}", self.next_id);
                self.next_id += 1;
                self.git(&["checkout", "-q", "-b", &name, &target]);
                self.names.push(name.clone());
                self.branches.insert(name.clone(), model);
                self.cur = name;
            }
            Op::Checkout(i) => {
                let name = self.names[i % self.names.len()].clone();
                if name == self.cur {
                    return Ok(());
                }
                self.git(&["checkout", "-q", &name]);
                self.cur = name;
            }
            Op::DeleteBranch(i) => {
                let others: Vec<String> = self
                    .names
                    .iter()
                    .filter(|n| **n != self.cur)
                    .cloned()
                    .collect();
                if others.is_empty() {
                    return Ok(());
                }
                let name = others[i % others.len()].clone();
                self.git(&["branch", "-q", "-D", &name]);
                self.names.retain(|n| *n != name);
                self.branches.remove(&name);
            }
        }
        if observed {
            let _ = self.scan()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Final, TestCaseError> {
        let pile = self.scan()?;
        let ledger = self.s.ledger();
        Ok(Final {
            pile: pile
                .rows
                .iter()
                .map(|r| {
                    format!(
                        "{}:{:?}",
                        String::from_utf8_lossy(&r.path).into_owned(),
                        r.change
                    )
                })
                .collect(),
            seen_branch: ledger.seen_branch.clone(),
            seen_tree: ledger.seen_tree.as_ref().map(ToString::to_string),
            // `updated_at` is a wall-clock stamp and differs between the two runs by
            // design; the baseline an override carries is what the record is.
            overrides: ledger
                .overrides
                .iter()
                .map(|(k, o)| {
                    (
                        k.clone(),
                        (
                            o.blob.as_ref().map(|b| {
                                b.as_ref().map_or("absent".to_owned(), ToString::to_string)
                            }),
                            o.mode.map(|m| m.as_str().to_owned()),
                        ),
                    )
                })
                .collect(),
        })
    }
}

fn run(ops: &[Op], observed: bool) -> Result<(Final, Vec<String>), TestCaseError> {
    let mut w = World::new(observed);
    for op in ops {
        w.step(op, observed)?;
    }
    let f = w.finish()?;
    Ok((f, w.log))
}

/// Property 2, as the runs are compared: the unobserved run's rows are rows of the
/// observed run's, and both end on the same branch.
fn no_row_manufactured(
    observed: &Final,
    unobserved: &Final,
    log_o: &[String],
    log_u: &[String],
) -> Result<(), TestCaseError> {
    prop_assert_eq!(
        &observed.seen_branch,
        &unobserved.seen_branch,
        "the two runs ended on different branches\n--- observed ---\n{}\n--- unobserved ---\n{}",
        log_o.join("\n"),
        log_u.join("\n")
    );
    for row in &unobserved.pile {
        prop_assert!(
            observed.pile.contains(row),
            "MANUFACTURED: {} is on the pile of the run that skipped the scans and on no \
             screen the watching run ever showed ({:?})\n--- observed ---\n{}\n\
             --- unobserved ---\n{}",
            row,
            observed.pile,
            log_o.join("\n"),
            log_u.join("\n")
        );
    }
    Ok(())
}

/// The two properties, over one generated sequence each.
#[test]
fn seen_oracle_no_hide_and_no_manufactured_work_over_random_branch_histories() {
    let mut runner = TestRunner::new(config());
    let result = runner.run(&ops(), |ops| {
        let (observed, log_o) = run(&ops, true)?;
        let (unobserved, log_u) = run(&ops, false)?;
        no_row_manufactured(&observed, &unobserved, &log_o, &log_u)
    });
    if let Err(e) = result {
        panic!("{e}");
    }
}

/// The sponsor's gate run as the oracle sees it (D25's main case): a branch cut from the
/// shared ancestor and committed to inside one unobserved window. Red until the fold
/// targets the merge-base: the unwatched run shows `g1` as a deletion, which no watching
/// run ever shows.
#[test]
fn seen_oracle_the_gate_run_shape_manufactures_no_deletion() {
    let ops = vec![
        Op::Cut(CutAt::Tip),
        Op::Commit(vec![(2, Some(0))]),
        Op::AcceptAll,
        Op::Checkout(0),
        Op::Cut(CutAt::Tip),
        Op::Commit(vec![(3, Some(1))]),
    ];
    let (observed, log_o) = run(&ops, true).expect("the watching run");
    let (unobserved, log_u) = run(&ops, false).expect("the unwatched run");
    no_row_manufactured(&observed, &unobserved, &log_o, &log_u).expect("no row is manufactured");
}

/// A pinned finding, green before and after the merge-base change: the equality
/// deliverable 7 asks for is false in the over-show direction. Watching the detour through
/// a branch cut at an older commit gives that branch a record which never saw `g1`, and the
/// branch cut next copies it; the unwatched run still carries the record that accepted
/// `g1`. Both are hide-free, and the unwatched pile is a subset of the watched one.
#[test]
fn seen_oracle_a_detour_through_an_older_branch_over_shows_only_when_watched() {
    let ops = vec![
        Op::Commit(vec![(2, Some(0))]),
        Op::AcceptAll,
        Op::Cut(CutAt::Older),
        Op::Cut(CutAt::Other(0)),
    ];
    let (observed, log_o) = run(&ops, true).expect("the watching run");
    let (unobserved, log_u) = run(&ops, false).expect("the unwatched run");
    assert_eq!(
        observed.pile,
        vec!["g1:Added".to_owned()],
        "{}",
        log_o.join("\n")
    );
    assert!(unobserved.pile.is_empty(), "{}", log_u.join("\n"));
    no_row_manufactured(&observed, &unobserved, &log_o, &log_u).expect("no row is manufactured");
}

/// The same finding from the other side, proptest's own shrunk case at 64 cases: a branch
/// the watching run passed through has a parked record of its own, so returning to it shows
/// the deletion again; the unwatched run never gave it one and copies the record that
/// accepted the deletion. Over-show again, and again only in the watched run.
#[test]
fn seen_oracle_a_branch_visited_while_watching_keeps_its_own_record() {
    let ops = vec![
        Op::Commit(vec![(0, None)]),
        Op::Cut(CutAt::Tip),
        Op::Cut(CutAt::Tip),
        Op::AcceptAll,
        Op::DeleteBranch(2),
        Op::Checkout(2),
    ];
    let (observed, log_o) = run(&ops, true).expect("the watching run");
    let (unobserved, log_u) = run(&ops, false).expect("the unwatched run");
    assert_eq!(
        observed.pile,
        vec!["f1:Deleted".to_owned()],
        "{}",
        log_o.join("\n")
    );
    assert!(unobserved.pile.is_empty(), "{}", log_u.join("\n"));
    no_row_manufactured(&observed, &unobserved, &log_o, &log_u).expect("no row is manufactured");
}
