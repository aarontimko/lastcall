//! Phase 14 B (Amendment v1.15, D16 amended): content already accepted on another branch
//! of the same repository is marked `seen_on` and folds into one `seen` group. Real git,
//! temp fixtures; D16 itself lives in `test_integration_scenarios_d.rs`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::Fresh;
use lastcall_engine::config::Config;
use lastcall_engine::engine::EngineOptions;
use lastcall_engine::ledger::{Clock, iso8601};
use lastcall_engine::ops::{NoFault, Rendered};
use lastcall_engine::scan::{Annotation, GroupKind, Pile};
use lastcall_testkit::assert_pile;

/// `(path, seen_on)` per row, in pile order.
fn marks(pile: &Pile) -> Vec<(String, Vec<String>)> {
    pile.rows
        .iter()
        .map(|r| (r.path_lossy(), r.seen_on.clone()))
        .collect()
}

fn m(path: &str, on: &[&str]) -> (String, Vec<String>) {
    (
        path.to_owned(),
        on.iter().map(|b| (*b).to_owned()).collect(),
    )
}

fn seen_paths(pile: &Pile) -> Vec<String> {
    pile.seen_group()
        .map(|g| {
            g.paths
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// First sight on `main`; `run-1` commits `a.rs`, `b.rs`, `c.rs` and they are accepted
/// there; back on `main`, then a fresh `feat-x` from `main` is in force with `run-1` and
/// `main` parked.
fn reviewed_run(name: &str) -> Fresh {
    reviewed_run_on(Fresh::new(name))
}

/// [`reviewed_run`] over a caller-built `Fresh` (the retention tests' movable clock).
fn reviewed_run_on(mut s: Fresh) -> Fresh {
    assert_pile!(s.engine, s.root, "", "first sight on main");
    s.repo.checkout_b("run-1").unwrap();
    s.repo.write("a.rs", "a\n");
    s.repo.write("b.rs", "b\n");
    s.repo.write("c.rs", "c\n");
    s.repo.commit("run-1 work").unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "the run's work");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "run-1 reviewed");
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "back on main");
    s.repo.checkout_b("feat-x").unwrap();
    assert_pile!(s.engine, s.root, "", "feat-x starts as main's copy");
    s
}

fn accept_seen_group(s: &mut Fresh, pile: &Pile) {
    let group = pile.seen_group().expect("a seen group");
    let rendered: Vec<Rendered> = group
        .paths
        .iter()
        .map(|p| Rendered::of(pile.row(p).unwrap()))
        .collect();
    let out = s
        .engine
        .ops(&s.root)
        .unwrap()
        .accept_group(&rendered, pile.seen_branch.as_deref(), &NoFault)
        .unwrap();
    assert!(out.ok(), "{out:?}");
}

#[test]
fn seen_group_cherry_pick_folds_and_a_group_accept_empties_it() {
    let mut s = reviewed_run("seen-cp");
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "cherry-picked");
    assert_eq!(
        marks(&pile),
        vec![
            m("a.rs", &["run-1"]),
            m("b.rs", &["run-1"]),
            m("c.rs", &["run-1"])
        ]
    );
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
    let main_before = s.ledger().branches["main"].clone();
    accept_seen_group(&mut s, &pile);
    assert_pile!(s.engine, s.root, "", "the group accept empties the pile");
    assert_eq!(
        s.ledger().branches["main"],
        main_before,
        "main's record untouched"
    );
    s.restart();
    assert_pile!(s.engine, s.root, "", "restart identical");
}

#[test]
fn seen_group_cherry_pick_with_one_file_edited_afterwards() {
    let mut s = reviewed_run("seen-cp-edit");
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    s.repo.write("b.rs", "b\nlater\n");
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs");
    assert_eq!(
        marks(&pile),
        vec![m("a.rs", &["run-1"]), m("b.rs", &[]), m("c.rs", &["run-1"])],
        "the edited file is a plain row"
    );
    assert_eq!(seen_paths(&pile), vec!["a.rs", "c.rs"]);
}

#[test]
fn seen_group_rebase_of_a_reviewed_branch_onto_main() {
    let mut s = Fresh::new("seen-rebase");
    assert_pile!(s.engine, s.root, "", "first sight on main");
    s.repo.checkout_b("run-1").unwrap();
    s.repo.write("a.rs", "a\n");
    s.repo.commit("run-1 work").unwrap();
    assert!(s.accept_all().ok());
    // main moves on, and its new file is reviewed on main.
    s.repo.checkout("main").unwrap();
    s.repo.write("m.rs", "m\n");
    s.repo.commit("main work").unwrap();
    assert_pile!(s.engine, s.root, "m.rs", "main's own work");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "main reviewed");
    // The reviewed branch is rebased onto main: main's reviewed file arrives on run-1.
    s.repo.checkout("run-1").unwrap();
    assert_pile!(s.engine, s.root, "", "run-1's record in force");
    s.repo.git(&["rebase", "-q", "main"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "m.rs", "rebased onto main");
    assert_eq!(marks(&pile), vec![m("m.rs", &["main"])]);
    assert_eq!(seen_paths(&pile), vec!["m.rs"]);
}

#[test]
fn seen_group_squash_merge() {
    let mut s = reviewed_run("seen-squash");
    s.repo.git(&["merge", "-q", "--squash", "run-1"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "squash staged");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
    s.repo.commit("squashed").unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "squash committed");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
}

#[test]
fn seen_group_a_file_flagged_after_the_cherry_pick_keeps_its_row() {
    let mut s = reviewed_run("seen-flag");
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs");
    s.engine
        .flag(&s.root, b"c.rs", "look again", None, None)
        .unwrap();
    let pile = s.scan();
    let c = pile.row(b"c.rs").unwrap();
    assert_eq!(c.flags.len(), 1, "the flag is on the row");
    assert_eq!(c.seen_on, vec!["run-1".to_owned()], "and so is the mark");
    assert_eq!(
        seen_paths(&pile),
        vec!["a.rs", "b.rs"],
        "c.rs is its own row"
    );
}

/// The names in the ledger's `retired` map, read from its own JSON (the shape the next
/// process loads; empty when the field is omitted).
fn retired(s: &Fresh) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ledger().to_json()).expect("ledger json");
    match v.get("retired") {
        Some(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

#[test]
fn seen_group_a_deleted_branch_keeps_matching_from_its_retired_record() {
    let mut s = reviewed_run("seen-deleted");
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs");
    s.repo.git(&["branch", "-D", "run-1"]).unwrap();
    let pile = s.scan();
    assert_eq!(
        seen_paths(&pile),
        vec!["a.rs", "b.rs", "c.rs"],
        "the parked record stays until a switch prunes it"
    );
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "on main");
    s.repo.checkout("feat-x").unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "back on feat-x");
    assert!(!s.ledger().branches.contains_key("run-1"), "pruned");
    assert_eq!(
        marks(&pile),
        vec![
            m("a.rs", &["run-1"]),
            m("b.rs", &["run-1"]),
            m("c.rs", &["run-1"])
        ],
        "the retired record keeps matching after the prune"
    );
    assert_eq!(retired(&s), vec!["run-1"], "and retired");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
    s.restart();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "restart");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
}

#[test]
fn seen_group_an_upstream_row_that_matches_stays_upstream() {
    let mut s = Fresh::new("seen-upstream");
    assert_pile!(s.engine, s.root, "", "first sight on main");
    s.repo.coworker_push(1).unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    // A branch takes the coworker's commit and it is reviewed there.
    s.repo.checkout_b("run-1").unwrap();
    s.repo
        .git(&["merge", "-q", "--ff-only", "origin/main"])
        .unwrap();
    assert_pile!(s.engine, s.root, "u1 upstream", "upstream on run-1");
    assert!(s.accept_all().ok());
    // main pulls the same commit: the row matches run-1's record and is upstream.
    s.repo.checkout("main").unwrap();
    assert_pile!(s.engine, s.root, "", "main");
    s.repo.git(&["pull", "-q", "--ff-only"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "u1 upstream", "pulled on main");
    let u1 = pile.row(b"u1").unwrap();
    assert_eq!(u1.annotation, Some(Annotation::Upstream));
    assert!(u1.seen_on.is_empty(), "upstream is never marked seen");
    let kinds: Vec<GroupKind> = pile.groups().iter().map(|g| g.kind).collect();
    assert_eq!(kinds, vec![GroupKind::Upstream], "counted once");
}

#[test]
fn seen_group_is_absent_for_a_root_on_one_branch() {
    let mut s = Fresh::new("seen-one-branch");
    s.repo.write("n.rs", "n\n");
    let pile = assert_pile!(s.engine, s.root, "n.rs");
    assert!(pile.rows.iter().all(|r| r.seen_on.is_empty()));
    assert_eq!(s.engine.root(&s.root).unwrap().seen_cache.len(), 0);
}

/// `just tryout cherry-pick`, step by step at the engine: the git mechanics its twelve
/// steps print give the outcome each step promises (`scripts/tryout.py`,
/// `scenario_cherry_pick`; `docs/dev/tryout.md`). The sandbox is built before first sight,
/// as the tryout builds it: `a.rs`, `b.rs`, `c.rs` on `main`; `run-2` commits `d.rs` and
/// `e.rs`; `feat-x`, `feat-y`, `feat-z` at `main`; the repository on `run-1` with the three
/// files edited and uncommitted.
#[test]
fn seen_group_the_tryout_walk_gives_what_each_step_promises() {
    use lastcall_testkit::fixture_repo::FixtureRepo;

    let mut repo = FixtureRepo::new("seen-walk").unwrap();
    repo.commit_files(&[("a.rs", "a\n"), ("b.rs", "b\n"), ("c.rs", "c\n")], "base")
        .unwrap();
    for b in ["feat-x", "feat-y", "feat-z"] {
        repo.git(&["branch", b]).unwrap();
    }
    repo.checkout_b("run-2").unwrap();
    repo.commit_files(&[("d.rs", "d\n"), ("e.rs", "e\n")], "run-2 work")
        .unwrap();
    repo.checkout("main").unwrap();
    repo.checkout_b("run-1").unwrap();
    for n in ["a", "b", "c"] {
        repo.write(&format!("{n}.rs"), format!("{n}\nrun-1 edit\n"));
    }
    let mut s = Fresh::over(
        repo,
        lastcall_engine::config::Config::default(),
        lastcall_engine::engine::EngineOptions::default(),
        false,
    );
    let seed = |s: &mut Fresh| -> Vec<String> {
        s.scan()
            .rows
            .iter()
            .map(|r| r.path_lossy())
            .filter(|p| !["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"].contains(&p.as_str()))
            .filter(|p| !p.starts_with("note-"))
            .collect()
    };
    assert_eq!(
        seed(&mut s),
        Vec::<String>::new(),
        "the seed files are not pending"
    );

    // (1) The reviewed work, then committed.
    assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "step 1: three rows");
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "step 1: ctrl-a empties it");
    s.repo.git(&["commit", "-q", "-am", "run-1 work"]).unwrap();
    assert_pile!(s.engine, s.root, "", "step 1: the commit changes nothing");
    // (2) The switch.
    s.repo.git(&["switch", "-q", "feat-x"]).unwrap();
    assert_pile!(s.engine, s.root, "", "step 2: feat-x is empty");
    // (3) The cherry-pick folds.
    s.repo.git(&["cherry-pick", "run-1"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "step 3");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);
    assert!(pile.rows.iter().all(|r| r.seen_on == ["run-1"]));
    // (6) One file changes: its own row, `[seen] 2 files`.
    s.repo.write("b.rs", "b\nrun-1 edit\nextra\n");
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "step 6");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "c.rs"]);
    // (7) A flagged member leaves the group; `[seen] 1 file`.
    s.engine
        .flag(&s.root, b"c.rs", "look again", None, None)
        .unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "step 7");
    assert_eq!(seen_paths(&pile), vec!["a.rs"]);
    assert_eq!(
        pile.row(b"c.rs").unwrap().seen_on,
        ["run-1"],
        "still badged"
    );
    // (8) Accept the group, then `z`.
    accept_seen_group(&mut s, &pile);
    assert_pile!(
        s.engine,
        s.root,
        "b.rs|c.rs",
        "step 8: accepted [seen] 1 file"
    );
    s.engine.undo(&s.root).unwrap();
    let pile = assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs", "step 8: z");
    assert_eq!(seen_paths(&pile), vec!["a.rs"]);
    // (9) Ten scratch files so `ctrl-a` asks; it accepts every pending file.
    for i in 1..=10 {
        s.repo
            .write(&format!("note-{i}.txt"), format!("note {i}\n"));
    }
    let pile = s.scan();
    assert_eq!(pile.rows.len(), 13);
    assert_eq!(seen_paths(&pile), vec!["a.rs"]);
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "step 9: everything accepted");
    s.repo.git(&["add", "-A"]).unwrap();
    s.repo.git(&["commit", "-q", "-m", "feat-x work"]).unwrap();
    assert_pile!(s.engine, s.root, "", "step 9: committed, clean");
    // (10) The rebase variant.
    s.repo.git(&["switch", "-q", "run-2"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "d.rs|e.rs", "step 10: run-2's two rows");
    assert!(pile.seen_group().is_none());
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "step 10: accepted");
    s.repo.git(&["switch", "-q", "feat-y"]).unwrap();
    assert_pile!(s.engine, s.root, "", "step 10: feat-y is empty");
    s.repo.git(&["rebase", "-q", "run-2"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "d.rs|e.rs", "step 10: rebased");
    assert_eq!(seen_paths(&pile), vec!["d.rs", "e.rs"]);
    assert!(pile.rows.iter().all(|r| r.seen_on == ["run-2"]));
    // (11) The squash-merge variant.
    s.repo.git(&["switch", "-q", "feat-z"]).unwrap();
    assert_pile!(s.engine, s.root, "", "step 11: feat-z is empty");
    s.repo.git(&["merge", "-q", "--squash", "run-2"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "d.rs|e.rs", "step 11: squashed");
    assert_eq!(seen_paths(&pile), vec!["d.rs", "e.rs"]);
    assert!(pile.rows.iter().all(|r| r.seen_on == ["run-2"]));
}

/// A clock the test moves: the retention tests' route to "31 days later".
#[derive(Debug)]
struct MovableClock(Mutex<SystemTime>);

impl MovableClock {
    fn at(t: SystemTime) -> Arc<Self> {
        Arc::new(Self(Mutex::new(t)))
    }

    fn set(&self, t: SystemTime) {
        *self.0.lock().unwrap() = t;
    }
}

impl Clock for MovableClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// 2026-01-01T00:00:00Z.
fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_767_225_600)
}

const DAY: Duration = Duration::from_secs(86_400);

fn fresh_at(name: &str, clock: &Arc<MovableClock>) -> Fresh {
    Fresh::with(
        name,
        Config::default(),
        EngineOptions {
            clock: clock.clone(),
            ..EngineOptions::default()
        },
        false,
    )
}

/// `retired.<name>.retired_at` from the ledger's own JSON.
fn retired_at(s: &Fresh, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ledger().to_json()).expect("ledger json");
    v.get("retired")?
        .get(name)?
        .get("retired_at")?
        .as_str()
        .map(str::to_owned)
}

/// `retired.<name>.seen_tree` from the ledger's own JSON.
fn retired_tree(s: &Fresh, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ledger().to_json()).expect("ledger json");
    v.get("retired")?
        .get(name)?
        .get("seen_tree")?
        .as_str()
        .map(str::to_owned)
}

/// Switch away and back (two scans), so the prune and the age check run.
fn round_trip(s: &mut Fresh, away: &str, back: &str) -> Pile {
    s.repo.checkout(away).unwrap();
    let _ = s.scan();
    s.repo.checkout(back).unwrap();
    s.scan()
}

#[test]
fn seen_group_a_retired_record_is_kept_30_days_then_dropped() {
    let clock = MovableClock::at(t0());
    let mut s = reviewed_run_on(fresh_at("retire-age", &clock));
    s.repo.git(&["cherry-pick", "main..run-1"]).unwrap();
    assert_pile!(s.engine, s.root, "a.rs|b.rs|c.rs");
    s.repo.git(&["branch", "-D", "run-1"]).unwrap();
    let pile = round_trip(&mut s, "main", "feat-x");
    assert_eq!(
        retired_at(&s, "run-1").as_deref(),
        Some("2026-01-01T00:00:00Z"),
        "stamped with the clock's now at the prune"
    );
    assert_eq!(retired_at(&s, "run-1"), Some(iso8601(t0())));
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);

    clock.set(t0() + 29 * DAY);
    let pile = round_trip(&mut s, "main", "feat-x");
    assert_eq!(retired(&s), vec!["run-1"], "29 days: kept");
    assert_eq!(seen_paths(&pile), vec!["a.rs", "b.rs", "c.rs"]);

    clock.set(t0() + 31 * DAY);
    let pile = round_trip(&mut s, "main", "feat-x");
    assert!(
        retired(&s).is_empty(),
        "31 days: dropped at the next switch"
    );
    assert_eq!(
        marks(&pile),
        vec![m("a.rs", &[]), m("b.rs", &[]), m("c.rs", &[])],
        "and the content no longer folds"
    );
    assert!(pile.groups().is_empty());
}

#[test]
fn seen_group_twenty_one_retirements_keep_the_twenty_newest() {
    let clock = MovableClock::at(t0());
    let mut s = fresh_at("retire-cap", &clock);
    assert_pile!(s.engine, s.root, "", "first sight on main");
    let names: Vec<String> = (0..21).map(|i| format!("b{i:02}")).collect();
    for name in names.iter().map(String::as_str).chain(["hop"]) {
        s.repo.checkout_b(name).unwrap();
        let _ = s.scan();
        s.repo.checkout("main").unwrap();
        let _ = s.scan();
    }
    // Retire b20 first (the oldest) and b00 last, so the name order is the reverse of the
    // age order and only the age can pick which one goes.
    let mut on_main = true;
    for (k, name) in names.iter().rev().enumerate() {
        clock.set(t0() + Duration::from_secs(60 * k as u64));
        s.repo.git(&["branch", "-D", name]).unwrap();
        s.repo
            .checkout(if on_main { "hop" } else { "main" })
            .unwrap();
        on_main = !on_main;
        let _ = s.scan();
    }
    let kept = retired(&s);
    assert_eq!(kept.len(), 20, "the cap: {kept:?}");
    assert_eq!(kept, names[..20].to_vec(), "the oldest, b20, went");
}

#[test]
fn seen_group_retiring_a_name_twice_keeps_the_newer_record() {
    let clock = MovableClock::at(t0());
    let mut s = reviewed_run_on(fresh_at("retire-twice", &clock));
    s.repo.git(&["branch", "-D", "run-1"]).unwrap();
    let _ = round_trip(&mut s, "main", "feat-x");
    let first_tree = retired_tree(&s, "run-1").expect("run-1 retired");
    assert_eq!(retired_at(&s, "run-1"), Some(iso8601(t0())));

    // run-1 again, with other accepted content, then deleted again a day later.
    s.repo.checkout_b("run-1").unwrap();
    let _ = s.scan();
    s.repo.write("d.rs", "d\n");
    s.repo.commit("run-1 again").unwrap();
    assert_pile!(s.engine, s.root, "d.rs", "the second run's work");
    assert!(s.accept_all().ok());
    s.repo.checkout("feat-x").unwrap();
    let _ = s.scan();
    clock.set(t0() + DAY);
    s.repo.git(&["branch", "-D", "run-1"]).unwrap();
    let _ = round_trip(&mut s, "main", "feat-x");
    assert_eq!(retired(&s), vec!["run-1"], "one entry per name");
    assert_eq!(retired_at(&s, "run-1"), Some(iso8601(t0() + DAY)));
    assert_ne!(
        retired_tree(&s, "run-1"),
        Some(first_tree),
        "the newer record replaced the older"
    );
}
