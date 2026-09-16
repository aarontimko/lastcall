//! The seen oracle (Phase 11, deliverable 7): one sentence, checked against every screen
//! the engine produces over random branch histories.
//!
//! > A path's current entry is treated as seen only if it was committed before lastcall
//! > first saw the repository, or it is the first-sight entry, or the user accepted exactly
//! > that entry.
//!
//! The test tracks that set (`Seen`) itself, from the operations it generated, never from
//! the engine's own state. An entry is content plus mode, so a mode flip is an entry of its
//! own. Every operation runs against a real fixture repository through the real engine, and
//! every scan goes through `Engine::scan` (`Fresh::scan`), never through `inspect_head`
//! alone, so the branch sync a pile depends on is the one a scan performs.
//!
//! **Property 1, no hide:** after every scan, every path absent from the pile has a current
//! entry in `Seen(path)`: the entry in the commit made before first sight, the first-sight
//! entry, or an entry accepted by an `AcceptAll` (the entry at accept time). `Seen` is
//! keyed by path alone, as the sentence is: an entry the user accepted is seen wherever it
//! turns up, and a screen that leaves it out is not hiding anything from them.
//!
//! **Property 2, a scan you did not make cannot manufacture work:** each sequence runs
//! twice, once with a scan after every git command and once with the scans only where the
//! sequence says (a run of git commands is one unobserved step; an accept has to see a
//! pile, so it scans in both runs). Every row of the unobserved run's final pile is a row
//! of the observed run's, and both runs end on the same branch.
//!
//! Property 2 is the *subset* of the equality deliverable 7 asks for, and the narrowing is
//! a finding, not a convenience: the equality is false in the other direction for reasons
//! the fold has nothing to do with, and it is false that way before and after every change
//! this phase made. Two shapes are pinned below as tests of their own
//! ([`seen_oracle_a_detour_through_an_older_branch_over_shows_only_when_watched`],
//! [`seen_oracle_a_branch_visited_while_watching_keeps_its_own_record`]): when the watching
//! run passes through a branch, that branch gets a record of its own, and the next first
//! sight copies *that* record rather than the one the unwatched run still carries. The
//! watching run then over-shows, which §2 allows. The direction that must never happen is
//! the one D25 caught: a pile that grows because nobody was looking.
//!
//! **Shapes the generator cannot express** are pinned as hand-driven tests at the end of
//! this file, each run through the same two runs and the same two properties: a symlink at
//! the commit a branch is cut at, a branch built without a checkout merged into the branch
//! being watched and then checked out, the same by a rebase, a criss-cross whose two
//! merge-bases make git pick the one carrying an entry nobody accepted, and the diverged
//! form of the intermediate-version shape (`docs/spec` §11's residual). Each sequence is
//! the one the third verifier's probes drive by hand.
//!
//! Case count: the unit tier's 8, `PROPTEST_CASES` when set (64 from the pre-push tier),
//! the same machinery as the store-backed proptests in `ops::tests::proptests`, read here
//! rather than through `crate::env` because this is an integration test and that reader is
//! crate-private. The sequence is four to seven operations rather than deliverable 7's six
//! to twelve, to hold the 64-case run inside the pre-push budget; the report says so.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Fresh, plumb_commit};
use lastcall_engine::scan::Pile;
use lastcall_testkit::fixture_repo::SEED_FILES;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// Four paths: two the seed commit already carries (so their first-sight entry is a blob)
/// and two it does not (so their first-sight entry is absence).
const PATHS: [&str; 4] = ["f1", "f2", "g1", "g2"];
/// Four blobs, short enough that a diff is one hunk.
const BLOBS: [&str; 4] = ["v0\n", "v1\n", "v2\n", "v3\n"];

/// One path's entry in the model: content and the executable bit, or absence.
type Cell = Option<(&'static str, bool)>;
/// The four paths' entries at one point in the history.
type Content = [Cell; 4];
/// The same entry read off the disk. A symlink's content is its link text, which is what
/// the engine stores as its baseline blob.
type Live = Option<(String, bool)>;

fn live(cell: Cell) -> Live {
    cell.map(|(c, x)| (c.to_owned(), x))
}

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
    let at = |p: &str| -> Cell {
        SEED_FILES
            .iter()
            .find(|(n, _)| *n == p)
            .map(|(_, c)| (*c, false))
    };
    [at(PATHS[0]), at(PATHS[1]), at(PATHS[2]), at(PATHS[3])]
}

/// The seed commit's content for `f1`, the path most of the hand-driven shapes move.
fn seed_f1() -> &'static str {
    SEED_FILES
        .iter()
        .find(|(n, _)| *n == PATHS[0])
        .map(|(_, c)| *c)
        .expect("the seed carries f1")
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
    /// The executable bit flipped on a path that is present, and committed. An entry of its
    /// own in `Seen`, so accepting a blob never makes its other mode seen.
    Mode(usize),
    AcceptAll,
    /// A new branch plus the checkout of it.
    Cut(CutAt),
    Checkout(usize),
    DeleteBranch(usize),

    // ---- hand-driven only, never generated: shapes the generator cannot express ----
    /// Paths written and committed. `None` removes the path; a content starting with `@`
    /// makes a symlink whose link text is the rest.
    Edit(Vec<(usize, Option<&'static str>)>),
    /// A raw git command line against the fixture.
    Git(Vec<&'static str>),
    /// A commit built by plumbing alone: `refs/heads/<name>` is moved to it, nothing is
    /// checked out, so the branch never gets a record of its own.
    Plumb {
        name: &'static str,
        base: &'static str,
        parents: Vec<&'static str>,
        sets: Vec<(usize, Option<&'static str>)>,
        after: u64,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => prop::collection::vec(
                (0..PATHS.len(), prop::option::of(0..BLOBS.len())),
                1..=3,
             ).prop_map(Op::Commit),
        2 => (0..PATHS.len()).prop_map(Op::Mode),
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
    prop::collection::vec(op(), 4..=7)
}

// ---------------------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------------------

/// One branch, as the model keeps it: the content at its creation point followed by the
/// content after each commit made on it while lastcall watched.
#[derive(Debug, Clone)]
struct Branch {
    hist: Vec<Content>,
}

impl Branch {
    fn tip(&self) -> Content {
        *self.hist.last().expect("a branch always has its base")
    }
    /// The branch as it was one commit ago (`HEAD~1`), for a `Cut(Older)`.
    fn older(&self) -> Self {
        let n = self.hist.len() - 1;
        Self {
            hist: self.hist[..n].to_vec(),
        }
    }
}

/// What the two runs are compared on: the pile and the branch the record in force belongs
/// to. The record itself (its tree, its overrides) is not compared: two runs may reach the
/// same screens through different records, and the screen is what the properties are about.
#[derive(Debug, PartialEq, Eq)]
struct Final {
    pile: Vec<String>,
    seen_branch: Option<String>,
}

struct World {
    s: Fresh,
    names: Vec<String>,
    branches: BTreeMap<String, Branch>,
    cur: String,
    next_id: usize,
    seen: BTreeMap<&'static str, BTreeSet<Live>>,
    /// The branch whose record the unwatched run holds: the one it was on at its last
    /// scan. Both runs track it, because it depends on the generated sequence alone, and
    /// both refuse to delete it (see [`Op::DeleteBranch`]).
    in_force: String,
    /// A hand-driven operation the model cannot follow has run. Property 1 is still checked
    /// against the disk; the model's own idea of the worktree is not.
    free: bool,
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
            set.insert(live(seed[i]));
            seen.insert(*p, set);
        }
        let mut branches = BTreeMap::new();
        branches.insert("main".to_owned(), Branch { hist: vec![seed] });
        Self {
            s,
            names: vec!["main".to_owned()],
            branches,
            cur: "main".to_owned(),
            next_id: 0,
            seen,
            in_force: "main".to_owned(),
            free: false,
            log: vec![format!(
                "# run: {}",
                if observed { "observed" } else { "unobserved" }
            )],
        }
    }

    fn br(&self) -> &Branch {
        &self.branches[&self.cur]
    }

    /// The entry on disk: the link text of a symlink, else the content and whether the
    /// executable bit is set, else absence.
    fn disk(&self, path: &str) -> Live {
        use std::os::unix::fs::PermissionsExt;
        let full = self.s.repo.path().join(path);
        let md = std::fs::symlink_metadata(&full).ok()?;
        if md.file_type().is_symlink() {
            let target = std::fs::read_link(&full).ok()?;
            return Some((target.to_string_lossy().into_owned(), false));
        }
        let text = std::fs::read_to_string(&full).ok()?;
        Some((text, md.permissions().mode() & 0o111 != 0))
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
            if !self.free {
                let model = live(self.br().tip()[i]);
                prop_assert_eq!(
                    &disk,
                    &model,
                    "the model and the worktree disagree about {}\n{}",
                    path,
                    self.log.join("\n")
                );
            }
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

    /// The branch HEAD is on, after a hand-driven command moved it.
    fn refresh_cur(&mut self) {
        self.cur = self
            .s
            .repo
            .git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .expect("HEAD")
            .trim()
            .to_owned();
    }

    fn step(&mut self, op: &Op, observed: bool) -> Result<(), TestCaseError> {
        match op {
            Op::Commit(edits) => {
                let was = self.br().tip();
                let mut now = was;
                for (pi, bi) in edits {
                    // A path that is already there keeps its mode when its content changes;
                    // a path written where there was none arrives as a plain file.
                    let keeps = was[*pi].is_some_and(|(_, x)| x);
                    now[*pi] = bi.map(|i| (BLOBS[i], keeps));
                }
                if now == was {
                    return Ok(());
                }
                for (i, path) in PATHS.iter().enumerate() {
                    if now[i] == was[i] {
                        continue;
                    }
                    match now[i] {
                        Some((c, _)) => {
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
            }
            Op::Mode(pi) => {
                let was = self.br().tip();
                let Some((c, x)) = was[*pi] else {
                    return Ok(());
                };
                let mut now = was;
                now[*pi] = Some((c, !x));
                self.s.repo.chmod_x(PATHS[*pi], !x);
                self.log
                    .push(format!("chmod {} {} on {}", PATHS[*pi], !x, self.cur));
                self.s.repo.commit("mode").expect("commit");
                let cur = self.cur.clone();
                let b = self.branches.get_mut(&cur).expect("current branch");
                b.hist.push(now);
            }
            Op::AcceptAll => {
                // An accept has to see a pile, so this is a scan point in both runs, and
                // the unwatched run's record in force becomes this branch's.
                self.in_force = self.cur.clone();
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
            }
            Op::Cut(at) => {
                let (target, model) = match at {
                    CutAt::Tip => ("HEAD".to_owned(), self.br().clone()),
                    CutAt::Other(i) => {
                        let n = self.names[i % self.names.len()].clone();
                        (n.clone(), self.branches[&n].clone())
                    }
                    CutAt::Older => {
                        if self.br().hist.len() < 2 {
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
                // Never the current branch, and never the branch whose record the
                // unwatched run still holds: deleting that one takes the fold's other tip
                // away with it, and the fail-open ladder's answer is the copy as it is
                // (`docs/dev/engine.md`, "the branch left has no ref"). The watching run
                // folded before the ref went, so the two runs part company for a reason
                // the fold never had a say in. Pinned as a test of its own,
                // `seen_oracle_deleting_the_branch_left_before_the_scan_keeps_the_copy`.
                let others: Vec<String> = self
                    .names
                    .iter()
                    .filter(|n| **n != self.cur && **n != self.in_force)
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
            Op::Edit(edits) => {
                self.free = true;
                for (pi, what) in edits {
                    let path = PATHS[*pi];
                    let full = self.s.repo.path().join(path);
                    if std::fs::symlink_metadata(&full).is_ok() {
                        self.s.repo.remove(path);
                    }
                    if let Some(c) = what {
                        match c.strip_prefix('@') {
                            Some(target) => self.s.repo.symlink(target, path),
                            None => {
                                self.s.repo.write(path, *c);
                            }
                        }
                    }
                }
                self.log.push(format!("edit {edits:?} on {}", self.cur));
                self.s.repo.commit("hand").expect("commit");
            }
            Op::Git(args) => {
                self.free = true;
                self.git(args);
                self.refresh_cur();
            }
            Op::Plumb {
                name,
                base,
                parents,
                sets,
                after,
            } => {
                self.free = true;
                let sets: Vec<(&str, Option<&str>)> =
                    sets.iter().map(|(pi, c)| (PATHS[*pi], *c)).collect();
                let oid = plumb_commit(
                    &self.s.repo,
                    name,
                    base,
                    parents,
                    &sets,
                    "built without a checkout",
                    *after,
                );
                self.log.push(format!(
                    "plumb {name} from {base} parents {parents:?} -> {oid}"
                ));
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

/// A hand-driven shape, run the way the random test runs a generated one: both runs, with
/// Property 1 checked at every scan of each and Property 2 over the two finals.
fn both_runs(ops: &[Op]) -> (Final, Final) {
    let (observed, log_o) = run(ops, true).expect("the watching run");
    let (unobserved, log_u) = run(ops, false).expect("the unwatched run");
    no_row_manufactured(&observed, &unobserved, &log_o, &log_u).expect("no row is manufactured");
    (observed, unobserved)
}

/// The rows of a pile that name `path`, for the hand-driven shapes' own expectations.
fn rows_for<'a>(f: &'a Final, path: &str) -> Vec<&'a String> {
    f.pile
        .iter()
        .filter(|r| r.split(':').next() == Some(path))
        .collect()
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
    both_runs(&ops);
}

/// The third verifier's F2, proptest's own shrunk case at 64 cases: two branches cut with
/// no scan between them, `f1` deleted and the deletion accepted on the second, then the
/// first checked out. The watching run gives that branch a record of its own and shows
/// nothing; the unwatched run reaches it through a first sight whose merge-base is the
/// commit lastcall first saw, so the entry it carries is seen state and must fold back.
/// Red until it does: the unwatched run manufactures `f1:Added`.
#[test]
fn seen_oracle_the_seed_folds_back_at_the_first_sight_head() {
    let ops = vec![
        Op::Cut(CutAt::Tip),
        Op::Cut(CutAt::Tip),
        Op::Commit(vec![(0, None)]),
        Op::AcceptAll,
        Op::Checkout(4),
    ];
    let (observed, unobserved) = both_runs(&ops);
    assert!(observed.pile.is_empty(), "{observed:?}");
    assert!(unobserved.pile.is_empty(), "{unobserved:?}");
}

/// The third shape the generator found, pinned here and kept out of it: `f1` is accepted on
/// `main` at `c2`, a branch is cut at `c1`, and `main` is deleted before lastcall looks. The
/// watching run folded `f1` back to `c1` while `refs/heads/main` was still there; the
/// unwatched run reaches its first scan with the record in force belonging to a branch that
/// no longer has a tip to compare against, so the copy stands as it is and `f1` over-shows.
/// That is the fail-open ladder's row for "the branch left has no ref", not the fold's
/// doing: it reads the same before and after the seen-state target.
#[test]
fn seen_oracle_deleting_the_branch_left_before_the_scan_keeps_the_copy() {
    let ops = vec![
        Op::Commit(vec![(0, Some(0))]),
        Op::AcceptAll,
        Op::Cut(CutAt::Older),
    ];
    let mut watched = World::new(true);
    let mut unwatched = World::new(false);
    for op in &ops {
        watched.step(op, true).expect("the watching run");
        unwatched.step(op, false).expect("the unwatched run");
    }
    // `git branch -D main` by hand: the generator's own DeleteBranch refuses the branch
    // whose record the unwatched run holds, which is exactly this one.
    watched.git(&["branch", "-q", "-D", "main"]);
    unwatched.git(&["branch", "-q", "-D", "main"]);
    let o = watched.finish().expect("watched");
    let u = unwatched.finish().expect("unwatched");
    assert!(o.pile.is_empty(), "the watching run folded f1 away: {o:?}");
    assert_eq!(
        u.pile,
        vec!["f1:Modified".to_owned()],
        "and the unwatched run over-shows it, the copy standing as it is: {}",
        unwatched.log.join("\n")
    );
}

/// A pinned finding, green before and after the seen-state target: the equality
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
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(observed.pile, vec!["g1:Added".to_owned()]);
    assert!(unobserved.pile.is_empty(), "{unobserved:?}");
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
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(observed.pile, vec!["f1:Deleted".to_owned()]);
    assert!(unobserved.pile.is_empty(), "{unobserved:?}");
}

// ---------------------------------------------------------------------------------------
// Shapes the generator cannot express, driven by hand. Each sequence is the one the third
// verifier's `h1.sh` / `h1b.sh` drive.
// ---------------------------------------------------------------------------------------

/// A symlink at the commit the branch is cut at (the verifier's T5b). `f1` becomes a
/// symlink at `c2`, is a file again at `c3`, and `B` is cut at `c2`. Nothing was ever
/// accepted, so the link must show on `B`.
#[test]
fn seen_oracle_a_symlink_at_the_cut_commit_is_not_seen_state() {
    let ops = vec![
        Op::Edit(vec![(0, Some("@f2"))]),
        Op::Git(vec!["branch", "-q", "B"]),
        Op::Edit(vec![(0, Some(seed_f1()))]),
        Op::Git(vec!["checkout", "-q", "B"]),
    ];
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(rows_for(&observed, "f1").len(), 1, "{observed:?}");
    assert_eq!(rows_for(&unobserved, "f1").len(), 1, "{unobserved:?}");
}

/// A branch built without a checkout, merged into the branch being watched with `-X ours`
/// and then checked out (the verifier's T3b). Its own tip is the merge-base, and no record
/// has ever composed the entry it carries for `f1`, so that entry must show.
#[test]
fn seen_oracle_a_never_visited_branch_merged_in_is_not_seen_state() {
    let ops = vec![
        Op::Git(vec!["checkout", "-q", "-b", "A"]),
        Op::Edit(vec![(0, Some("v2\n"))]),
        Op::AcceptAll,
        Op::Plumb {
            name: "other",
            base: "main",
            parents: vec!["main"],
            sets: vec![(0, Some("vm\n")), (3, Some("k2\n"))],
            after: 1,
        },
        Op::Git(vec![
            "merge",
            "-q",
            "--no-ff",
            "-X",
            "ours",
            "-m",
            "merge other",
            "other",
        ]),
        Op::AcceptAll,
        Op::Git(vec!["checkout", "-q", "other"]),
    ];
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(rows_for(&observed, "f1").len(), 1, "{observed:?}");
    assert_eq!(rows_for(&unobserved, "f1").len(), 1, "{unobserved:?}");
}

/// The same by a rebase (the verifier's T3a): `A` is rebased onto the branch nobody visited,
/// keeping `A`'s own blob, and that branch is then checked out.
#[test]
fn seen_oracle_a_rebase_onto_a_never_visited_branch_is_not_seen_state() {
    let ops = vec![
        Op::Git(vec!["checkout", "-q", "-b", "A"]),
        Op::Edit(vec![(0, Some("v2\n"))]),
        Op::AcceptAll,
        Op::Plumb {
            name: "other",
            base: "main",
            parents: vec!["main"],
            sets: vec![(0, Some("vm\n"))],
            after: 1,
        },
        Op::Git(vec!["rebase", "-q", "-X", "theirs", "other"]),
        Op::Git(vec!["checkout", "-q", "other"]),
    ];
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(rows_for(&observed, "f1").len(), 1, "{observed:?}");
    assert_eq!(rows_for(&unobserved, "f1").len(), 1, "{unobserved:?}");
}

/// A criss-cross with two merge-bases, in the order that makes git pick the unsafe one (the
/// verifier's T1b). `X`'s side is built by plumbing so the branch is never visited by either
/// run; the merge-base git returns carries `f1 = v2`, which nobody ever accepted, and the
/// other one does not. The entry at that base must not become seen state.
#[test]
fn seen_oracle_a_criss_cross_merge_base_is_not_seen_state() {
    let ops = vec![
        // x1: `X` at the seed commit plus g1, older than c2 so git's merge-base picks c2.
        Op::Plumb {
            name: "X",
            base: "main",
            parents: vec!["main"],
            sets: vec![(2, Some("q1\n"))],
            after: 10,
        },
        Op::Edit(vec![(0, Some("v2\n"))]),
        Op::Git(vec!["merge", "-q", "--no-ff", "-m", "m1", "X"]),
        // x2: the same merge from X's side. The merged tree is main's own, and the second
        // parent is m1's first parent, the commit that carries f1 = v2.
        Op::Plumb {
            name: "X",
            base: "main",
            parents: vec!["X", "main^1"],
            sets: vec![],
            after: 1,
        },
        Op::Plumb {
            name: "X",
            base: "X",
            parents: vec!["X"],
            sets: vec![(2, Some("q2\n"))],
            after: 2,
        },
        Op::Edit(vec![(0, Some("v3\n"))]),
        Op::AcceptAll,
        Op::Git(vec!["checkout", "-q", "X"]),
    ];
    let mut watched = World::new(true);
    let mut unwatched = World::new(false);
    for op in &ops {
        watched.step(op, true).expect("the watching run");
        unwatched.step(op, false).expect("the unwatched run");
    }
    // The shape's own precondition: two merge-bases, and the one git hands the fold is the
    // one carrying the blob nobody accepted.
    let all = watched
        .s
        .repo
        .git(&["merge-base", "--all", "main", "X"])
        .expect("merge-base --all");
    assert_eq!(
        all.lines().count(),
        2,
        "the criss-cross has two merge-bases: {all}"
    );
    let picked = watched
        .s
        .repo
        .git(&["merge-base", "main", "X"])
        .expect("merge-base")
        .trim()
        .to_owned();
    assert_eq!(
        watched
            .s
            .repo
            .git(&["show", &format!("{picked}:f1")])
            .expect("f1 at the merge-base"),
        "v2\n",
        "git picks the base carrying the entry nobody accepted"
    );
    let o = watched.finish().expect("watched");
    let u = unwatched.finish().expect("unwatched");
    no_row_manufactured(&o, &u, &watched.log, &unwatched.log).expect("no row is manufactured");
    assert_eq!(rows_for(&o, "f1").len(), 1, "{o:?}");
    assert_eq!(rows_for(&u, "f1").len(), 1, "{u:?}");
}

/// The diverged form of the intermediate-version shape (the verifier's T9, `docs/spec` §11's
/// residual): `f1` goes v1.5 then v2 inside one window and is accepted as one hunk, and a
/// branch is cut at the v1.5 commit and committed to. v1.5 was never on a screen, so it must
/// show there.
#[test]
fn seen_oracle_an_intermediate_version_on_a_diverged_branch_is_not_seen_state() {
    let ops = vec![
        Op::Edit(vec![(0, Some("v1.5\n"))]),
        Op::Edit(vec![(0, Some("v2\n"))]),
        Op::AcceptAll,
        Op::Git(vec!["checkout", "-q", "-b", "B", "HEAD~1"]),
        Op::Edit(vec![(2, Some("q\n"))]),
    ];
    let (observed, unobserved) = both_runs(&ops);
    assert_eq!(rows_for(&observed, "f1").len(), 1, "{observed:?}");
    assert_eq!(rows_for(&unobserved, "f1").len(), 1, "{unobserved:?}");
}
