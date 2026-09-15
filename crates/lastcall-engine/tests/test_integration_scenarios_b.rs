//! Scenario suite B — local history operations (docs/spec/01-scenarios.md §B). Pending is
//! never cleared by history; HEAD is only consulted for the notice text.

mod common;

use common::Fresh;
use lastcall_engine::headstate::{self, InProgress};
use lastcall_testkit::assert_pile;

const F1_EDITED: &str = "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\nedit\n";

#[test]
fn scenario_b1_commit_does_not_clear_pending() {
    let mut s = Fresh::new("b1");
    s.repo.write("f1", F1_EDITED);
    let before = assert_pile!(s.engine, s.root, "f1", "B1 pre-commit");
    s.repo.commit("agent commit").unwrap();
    let after = assert_pile!(s.engine, s.root, "f1", "B1 commit does NOT clear pending");
    assert_eq!(
        after.rows[0].hunks, before.rows[0].hunks,
        "same hunks after the commit"
    );
    assert_eq!(
        s.head_notice().as_deref(),
        Some("committed on main (1 commit)")
    );
    assert_pile!(s.engine, s.root, "f1", "B1 after head inspection");
    s.restart();
    assert_pile!(s.engine, s.root, "f1", "B1 restart");
}

#[test]
fn scenario_b2_checkout_b_is_unchanged() {
    let mut s = Fresh::new("b2");
    s.repo.write("f1", F1_EDITED);
    s.repo.commit("agent commit").unwrap();
    assert_eq!(
        s.head_notice().as_deref(),
        Some("committed on main (1 commit)")
    );
    s.repo.checkout_b("feat-x").unwrap();
    assert_pile!(s.engine, s.root, "f1", "B2 checkout -b: unchanged");
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched main → feat-x (same commit)")
    );
}

#[test]
fn scenario_b3_flip_flop_is_unchanged() {
    let mut s = Fresh::new("b3");
    s.repo.write("f1", F1_EDITED);
    s.repo.commit("agent commit").unwrap();
    s.repo.checkout_b("feat-x").unwrap();
    assert_pile!(s.engine, s.root, "f1");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.checkout("main").unwrap();
    s.repo.checkout("feat-x").unwrap();
    assert_pile!(s.engine, s.root, "f1", "B3 flip-flop: unchanged");
    assert!(
        s.engine.inspect_head(&s.root).unwrap().is_none(),
        "net zero HEAD change"
    );
}

#[test]
fn scenario_b9_push_keeps_individual_rows() {
    let mut s = Fresh::new("b9");
    s.repo.write("f1", F1_EDITED);
    s.repo.commit("agent commit").unwrap();
    s.repo.checkout_b("feat-x").unwrap();
    s.repo
        .git(&["push", "-q", "-u", "origin", "feat-x"])
        .unwrap();
    let pile = assert_pile!(s.engine, s.root, "f1", "B9 push: still individual pending");
    assert!(pile.rows[0].annotation.is_none());
}

#[test]
fn scenario_b4_divergent_branch_and_back() {
    let mut s = Fresh::new("b4");
    s.repo.checkout_b("feat-y").unwrap();
    s.repo.write("g1", "g1\n");
    s.repo.write("g2", "g2\n");
    s.repo.commit("feat").unwrap();
    s.repo.checkout("main").unwrap();
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.write("f1", F1_EDITED);
    assert!(s.accept_file("f1").ok());
    assert_pile!(
        s.engine,
        s.root,
        "",
        "B4 baseline: override on f1, empty pile"
    );
    let over = s.ledger().overrides["f1"].clone();
    s.repo.checkout("feat-y").unwrap();
    assert_pile!(s.engine, s.root, "g1|g2", "B4 switch to feat-y over-shows");
    // Amendment v1.12: feat-y has never been in force, so the arrival is a first sight and
    // says where the state it is showing came from. The count is the same one.
    assert_eq!(
        s.head_notice().as_deref(),
        Some(
            "switched main → feat-y: first time here, seen state carried from main; 2 files pending"
        )
    );
    s.repo.checkout("main").unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "",
        "B4 switch back self-clears, override intact"
    );
    assert_eq!(s.ledger().overrides["f1"], over);
    assert_eq!(
        s.head_notice().as_deref(),
        Some("switched feat-y → main: 0 files differ from seen state")
    );
}

#[test]
fn scenario_b5_rebase_reflags_nothing() {
    let mut s = Fresh::new("b5");
    s.repo.checkout_b("feat-x").unwrap();
    s.repo.write("l1", "l1\n");
    s.repo.commit("local1").unwrap();
    s.repo.write("l2", "l2\n");
    s.repo.commit("local2").unwrap();
    assert!(s.accept_all().ok());
    assert_pile!(s.engine, s.root, "", "B5 accepted at feat-x tip");
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.coworker_push(1).unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    s.repo.git(&["rebase", "-q", "origin/main"]).unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "u1 upstream",
        "B5 rebase: only upstream group, rebased commits re-flag nothing"
    );
    let notice = s.head_notice().expect("rebase notice");
    assert!(notice.starts_with("rebased feat-x onto "), "{notice}");
}

#[test]
fn scenario_b5b_pull_rebase() {
    let mut s = Fresh::new("b5b");
    s.repo.checkout_b("feat-x").unwrap();
    s.repo.write("l1", "l1\n");
    s.repo.commit("local1").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.coworker_push(1).unwrap();
    s.repo
        .git(&["pull", "-q", "--rebase", "origin", "main"])
        .unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "u1 upstream",
        "B5b pull --rebase: upstream group only"
    );
    assert!(s.head_notice().is_some());
}

#[test]
fn scenario_b6_amend_shows_exactly_the_amended_delta() {
    let mut s = Fresh::new("b6");
    s.repo.write("f2", "v1\n");
    s.repo.commit("v1").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.write("f2", "v2\n");
    s.repo
        .git(&["commit", "-q", "--amend", "-am", "v2"])
        .unwrap();
    let pile = assert_pile!(
        s.engine,
        s.root,
        "f2",
        "B6 amend: exactly the amended delta"
    );
    let row = pile.row(b"f2").unwrap();
    assert_eq!((row.added, row.deleted), (1, 1));
    let base = s
        .store()
        .cat_blob(&row.baseline.as_ref().unwrap().oid)
        .unwrap();
    assert_eq!(base, b"v1\n", "baseline is the accepted v1, not the seed");
    let notice = s.head_notice().expect("amend notice");
    assert!(notice.starts_with("committed on main"), "{notice}");
}

#[test]
fn scenario_b7_reset_hard_over_shows_reverse_delta() {
    let mut s = Fresh::new("b7");
    s.repo.write("f2", "t\n");
    s.repo.commit("T").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo.git(&["reset", "-q", "--hard", "HEAD~1"]).unwrap();
    let pile = assert_pile!(s.engine, s.root, "f2", "B7 reset: reverse delta over-shows");
    let row = pile.row(b"f2").unwrap();
    let cur = s
        .store()
        .cat_blob(&row.current.as_ref().unwrap().oid)
        .unwrap();
    assert_eq!(cur, b"b\n");
    assert_eq!(s.head_notice().as_deref(), Some("reset: moving to HEAD~1"));
}

#[test]
fn scenario_b8_stash_and_pop() {
    let mut s = Fresh::new("b8");
    s.repo.write("f1", F1_EDITED);
    assert_pile!(s.engine, s.root, "f1", "B8 pending");
    s.repo.git(&["stash", "-q"]).unwrap();
    assert_pile!(s.engine, s.root, "", "B8 stash -> not on disk, not pending");
    assert!(
        s.engine.inspect_head(&s.root).unwrap().is_none(),
        "stash does not move HEAD"
    );
    s.repo.git(&["stash", "pop", "-q"]).unwrap();
    assert_pile!(s.engine, s.root, "f1", "B8 pop -> back");
}

#[test]
fn scenario_b10_rebase_stopped_on_conflict() {
    let mut s = Fresh::new("b10");
    s.repo.checkout_b("feat-x").unwrap();
    s.repo.write("f2", "mine\n");
    s.repo.commit("mine").unwrap();
    assert!(s.accept_all().ok());
    let _ = s.engine.inspect_head(&s.root).unwrap();
    s.repo
        .coworker_commit(&[("f2", "theirs\n"), ("u1", "other\n")], "cw")
        .unwrap();
    s.repo.git(&["fetch", "-q"]).unwrap();
    assert!(
        s.repo.git(&["rebase", "-q", "origin/main"]).is_err(),
        "rebase stops on f2"
    );
    let pile = s.scan();
    let row = pile.row(b"f2").expect("f2 pending during the conflict");
    assert!(row.conflicted);
    let cur = s
        .store()
        .cat_blob(&row.current.as_ref().unwrap().oid)
        .unwrap();
    assert!(
        cur.starts_with(b"<<<<<<<"),
        "markers on disk: {}",
        String::from_utf8_lossy(&cur)
    );
    let rg = s.engine.root(&s.root).unwrap().repo.as_ref().unwrap();
    assert_eq!(
        headstate::inspect(rg).unwrap().in_progress,
        Some(InProgress::Rebase)
    );
    let change = s.engine.inspect_head(&s.root).unwrap();
    assert!(
        change.and_then(|c| c.notice).is_none(),
        "no notice while in progress"
    );
    s.repo.write("f2", "resolved\n");
    s.repo.git(&["add", "f2"]).unwrap();
    s.repo
        .git(&["-c", "core.editor=true", "rebase", "--continue"])
        .unwrap();
    assert_pile!(
        s.engine,
        s.root,
        "f2 mixed|u1 upstream",
        "B10 after resolution"
    );
    let notice = s.head_notice().expect("notice once the rebase clears");
    assert!(notice.starts_with("rebased feat-x"), "{notice}");
}
